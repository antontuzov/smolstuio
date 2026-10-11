//! Everything a tool needs from the run it is part of.
//!
//! A tool is given a workspace, a policy, a log and a journal, and nothing else:
//! no handle on the model, no way to widen its own permissions, no ambient
//! current directory to be surprised by. The paths a tool reports in an
//! [`crate::tool::Intent`] come from [`Context::resolve`], which is where a
//! `../` that climbs out of the repository stops being a path and becomes a
//! refusal.
//!
//! The journal is the part that belongs to the run rather than the call. A write
//! records what it replaced, so an agent that went wrong can be undone by the
//! same command that told it to go; and an approval is remembered here, so a
//! policy is asked once about a thing rather than once per retry of it.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use agent_repo::patch::{restore_all, Restored, Snapshot};
use agent_sandbox::{AuditLog, Permission, Policy};

/// What this run has written, and what a person has already agreed to.
#[derive(Debug, Default)]
struct Journal {
    /// Newest last, so a rollback undoes the last thing that happened first.
    snapshots: Vec<Snapshot>,
    approved: Vec<Permission>,
    /// A yes about one file, kept with the class of thing it was about, because
    /// approval to edit `a.rs` says nothing about running a program.
    grants: Vec<(Permission, String)>,
}

impl Journal {
    fn approved(&self, permission: Permission, subject: Option<&str>) -> bool {
        if self.approved.contains(&permission) {
            return true;
        }
        subject.is_some_and(|name| {
            self.grants
                .iter()
                .any(|(class, known)| *class == permission && known == name)
        })
    }
}

#[derive(Debug, Clone)]
pub struct Context {
    /// The repository the agent was pointed at.
    pub root: PathBuf,
    pub policy: Policy,
    pub audit: AuditLog,
    /// Whether output passes through the secret mask on its way back to the
    /// model. On by default; a run that turns it off has said so explicitly.
    pub redact_secrets: bool,
    /// Shared by every clone of one context: an approval given through one
    /// handle is a decision this run has made.
    journal: Arc<Mutex<Journal>>,
}

impl Context {
    /// A context that holds everything to the workspace and keeps no log, which
    /// is what a test and a first run want.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        Self {
            policy: Policy::new(root.clone()),
            root,
            audit: AuditLog::disabled(),
            redact_secrets: true,
            journal: Arc::new(Mutex::new(Journal::default())),
        }
    }

    pub fn from_config(root: impl Into<PathBuf>, config: &agent_config::Config) -> Self {
        let root = root.into();
        Self {
            policy: Policy::from_config(root.clone(), config),
            root,
            audit: AuditLog::disabled(),
            redact_secrets: config.privacy.redact_secrets,
            journal: Arc::new(Mutex::new(Journal::default())),
        }
    }

    pub fn with_audit(mut self, audit: AuditLog) -> Self {
        self.audit = audit;
        self
    }

    pub fn with_policy(mut self, policy: Policy) -> Self {
        self.policy = policy;
        self
    }

    /// The path a model named, resolved against the workspace and refused if it
    /// leaves it.
    pub fn resolve(&self, raw: &str) -> Result<PathBuf, String> {
        self.policy.resolve(raw)
    }

    /// A path as the model wrote it, which is what a tool prints back: an
    /// absolute path in a transcript teaches a model nothing it can reuse.
    pub fn relative(&self, path: &Path) -> String {
        match path.strip_prefix(&self.root) {
            Ok(relative) if !relative.as_os_str().is_empty() => {
                relative.to_string_lossy().into_owned()
            }
            _ => path.to_string_lossy().into_owned(),
        }
    }

    fn with_journal<T>(&self, then: impl FnOnce(&mut Journal) -> T) -> T {
        // A poisoned lock means a tool panicked mid-write. This is what a
        // rollback reads, so the entries made before the panic are kept rather
        // than lost with it.
        let mut journal = self
            .journal
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        then(&mut journal)
    }

    /// Keep what a write replaced, so [`Context::rollback`] can put it back.
    pub fn record(&self, snapshot: Snapshot) {
        self.with_journal(|journal| journal.snapshots.push(snapshot));
    }

    /// Every file this run has changed, as the repository names it.
    pub fn changes(&self) -> Vec<PathBuf> {
        let mut seen: Vec<PathBuf> = Vec::new();
        self.with_journal(|journal| {
            for snapshot in &journal.snapshots {
                for path in snapshot.files() {
                    if !seen.contains(&path) {
                        seen.push(path);
                    }
                }
            }
        });
        seen
    }

    /// What this run wrote, oldest first.
    ///
    /// Enough to put the tree back from another process, which is why a session
    /// file stores these rather than only the list of names: the record of what
    /// was there before has to survive the run that read it.
    pub fn snapshots(&self) -> Vec<Snapshot> {
        self.with_journal(|journal| journal.snapshots.clone())
    }

    /// Put the tree back the way it was, newest change first.
    ///
    /// A file a person has edited since the agent wrote it is left alone and
    /// named in the answer: work that grew after the run is not the run's to
    /// remove.
    pub fn rollback(&self) -> Vec<Restored> {
        restore_all(&self.root, &self.snapshots())
    }

    /// A person has answered for a call. `for_run` means the whole class of thing
    /// is agreed to, so a run that edits five files asks once.
    pub fn approve(&self, permission: Permission, subject: Option<&str>, for_run: bool) {
        self.with_journal(|journal| {
            if for_run || subject.is_none() {
                if !journal.approved.contains(&permission) {
                    journal.approved.push(permission);
                }
                return;
            }
            let name = subject.unwrap_or_default().to_owned();
            let grant = (permission, name);
            if !journal.grants.contains(&grant) {
                journal.grants.push(grant);
            }
        });
    }

    /// Whether [`crate::Registry::execute`] should run a call the policy wants
    /// asked about, rather than stopping to ask a second time.
    pub fn approved(&self, permission: Permission, subject: Option<&str>) -> bool {
        self.with_journal(|journal| journal.approved(permission, subject))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_relative_path_becomes_a_workspace_path_and_back() {
        let ctx = Context::new("/repo");
        let resolved = ctx.resolve("src/lib.rs").expect("inside");
        assert_eq!(resolved, PathBuf::from("/repo/src/lib.rs"));
        assert_eq!(ctx.relative(&resolved), "src/lib.rs");
    }

    #[test]
    fn a_path_that_leaves_the_workspace_is_refused_where_it_is_read() {
        let ctx = Context::new("/repo");
        assert!(ctx.resolve("../outside").is_err());
        assert!(ctx.resolve("/etc/passwd").is_err());
        assert!(ctx.resolve("./inside").is_ok());
    }

    #[test]
    fn a_path_outside_the_root_keeps_its_absolute_spelling_when_printed() {
        let ctx =
            Context::new("/repo").with_policy(Policy::new("/repo").with_workspace_only(false));
        let path = PathBuf::from("/tmp/elsewhere");
        assert_eq!(ctx.relative(&path), "/tmp/elsewhere");
    }

    #[test]
    fn an_approval_of_one_thing_does_not_answer_for_every_thing() {
        let ctx = Context::new("/repo");
        assert!(!ctx.approved(Permission::Write, Some("/repo/a.rs")));
        ctx.approve(Permission::Write, Some("/repo/a.rs"), false);
        assert!(ctx.approved(Permission::Write, Some("/repo/a.rs")));
        assert!(
            !ctx.approved(Permission::Write, Some("/repo/b.rs")),
            "the next file is still a question"
        );
        assert!(
            !ctx.approved(Permission::Execute, Some("/repo/a.rs")),
            "a yes about a file is not a yes about a program"
        );

        ctx.approve(Permission::Write, None, true);
        assert!(
            ctx.approved(Permission::Write, Some("/repo/b.rs")),
            "the class is agreed to"
        );
        assert!(!ctx.approved(Permission::Execute, None));
    }

    #[test]
    fn a_clone_shares_the_journal_because_the_run_made_the_decision() {
        let ctx = Context::new("/repo");
        let same = ctx.clone();
        ctx.approve(Permission::Write, Some("/repo/a.rs"), false);
        assert!(same.approved(Permission::Write, Some("/repo/a.rs")));
    }

    #[test]
    fn a_context_that_wrote_nothing_has_nothing_to_roll_back() {
        let ctx = Context::new("/repo");
        assert!(ctx.changes().is_empty());
        assert!(ctx.rollback().is_empty());
    }

    #[test]
    fn the_snapshots_a_run_keeps_are_enough_to_undo_it() {
        let dir = tempfile::tempdir().expect("a temp repo");
        let ctx = Context::new(dir.path().to_path_buf());
        let patch = agent_repo::patch::diff("notes.md", "", "agent words\n");
        let planned = agent_repo::patch::plan(dir.path(), &patch).expect("a plan");
        let snapshot = agent_repo::patch::write(dir.path(), &planned).expect("written");
        ctx.record(snapshot.clone());

        assert_eq!(ctx.snapshots(), vec![snapshot], "the journal, oldest first");
        assert_eq!(ctx.rollback().len(), 1);
        assert!(
            !dir.path().join("notes.md").exists(),
            "the file the run created is gone"
        );
        assert_eq!(
            ctx.snapshots().len(),
            1,
            "the journal is a record, not a queue: a rollback leaves it readable"
        );
    }
}
