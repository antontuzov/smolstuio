//! A run left on disk, and taken back off it.
//!
//! A session belongs to the repository it worked in, so its file lives at
//! `.smoll/sessions/<id>.json` next to `.smoll/audit.jsonl`: a person reviewing
//! this workspace finds the record of what happened here, in here, rather than
//! in a cache directory that does not travel with the code.
//!
//! The file carries the write journal as well as the transcript. That is what
//! lets `smoll rollback` undo a run from a process that never made it — the
//! transcript says what was touched, the journal says what was there before.

use std::path::{Path, PathBuf};

use agent_repo::patch::{restore_all, Restored, Snapshot};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::AgentError;
use crate::types::{now_ms, Session};

/// The directory a workspace keeps its runs in, named from its root.
pub const SESSIONS_DIR: &str = ".smoll/sessions";

/// A transcript plus the records of what its writes replaced.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredSession {
    pub session: Session,
    /// Oldest first, the order the writes happened; a rollback works from the
    /// other end.
    pub journal: Vec<Snapshot>,
    /// When this file was written. `latest` trusts this rather than the
    /// filesystem's own timestamp, which a copy or a sync rewrites.
    pub stored_at_ms: u64,
}

impl StoredSession {
    pub fn new(session: Session, journal: Vec<Snapshot>) -> Self {
        Self {
            stored_at_ms: now_ms(),
            session,
            journal,
        }
    }

    /// Where this run's file sits in a workspace.
    pub fn path_for(root: &Path, id: Uuid) -> PathBuf {
        root.join(SESSIONS_DIR).join(format!("{id}.json"))
    }

    pub fn path(&self, root: &Path) -> PathBuf {
        Self::path_for(root, self.session.id)
    }

    /// Write the run, creating the directory it lives in.
    pub fn write(&self, root: &Path) -> Result<PathBuf, AgentError> {
        let dir = root.join(SESSIONS_DIR);
        std::fs::create_dir_all(&dir).map_err(|source| AgentError::Io {
            path: dir.display().to_string(),
            source,
        })?;
        let target = self.path(root);
        let text = serde_json::to_string_pretty(self)?;
        std::fs::write(&target, text).map_err(|source| AgentError::Io {
            path: target.display().to_string(),
            source,
        })?;
        Ok(target)
    }

    /// Read one run back by its id, refusing if the file is not there.
    pub fn read(root: &Path, id: Uuid) -> Result<Self, AgentError> {
        Self::read_file(&Self::path_for(root, id))
    }

    fn read_file(path: &Path) -> Result<Self, AgentError> {
        let text = std::fs::read_to_string(path).map_err(|source| AgentError::Io {
            path: path.display().to_string(),
            source,
        })?;
        Ok(serde_json::from_str(&text)?)
    }

    /// The most recently stored run in the workspace.
    ///
    /// A file that does not parse is skipped with a warning rather than ending
    /// the search: half a session left by a killed process is not a reason to
    /// stop showing the runs that did finish.
    pub fn latest(root: &Path) -> Result<Option<Self>, AgentError> {
        let dir = root.join(SESSIONS_DIR);
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => {
                return Err(AgentError::Io {
                    path: dir.display().to_string(),
                    source,
                })
            }
        };
        let mut newest: Option<Self> = None;
        for entry in entries {
            let path = match entry {
                Ok(entry) => entry.path(),
                Err(source) => {
                    return Err(AgentError::Io {
                        path: dir.display().to_string(),
                        source,
                    })
                }
            };
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            let stored = match Self::read_file(&path) {
                Ok(stored) => stored,
                Err(source) => {
                    tracing::warn!(%source, path = %path.display(), "unreadable session file skipped");
                    continue;
                }
            };
            let keep_newer = match &newest {
                Some(current) => {
                    stored.stored_at_ms > current.stored_at_ms
                        || (stored.stored_at_ms == current.stored_at_ms
                            && stored.session.started_at_ms > current.session.started_at_ms)
                }
                None => true,
            };
            if keep_newer {
                newest = Some(stored);
            }
        }
        Ok(newest)
    }

    /// Undo what this run wrote, newest write first.
    ///
    /// Files a person has edited since are named in the answer rather than
    /// removed; see [`agent_repo::patch::Snapshot::restore`].
    pub fn restore(&self, root: &Path) -> Vec<Restored> {
        restore_all(root, &self.journal)
    }

    /// The files this run can put back, as the repository names them.
    pub fn restorable(&self) -> Vec<PathBuf> {
        self.journal
            .iter()
            .flat_map(|snapshot| snapshot.files())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(task: &str) -> Session {
        Session::new(task)
    }

    /// A snapshot of a file this process created, written for real.
    fn journal_for(root: &Path, name: &str, text: &str) -> Vec<Snapshot> {
        let patch = agent_repo::patch::diff(name, "", text);
        let planned = agent_repo::patch::plan(root, &patch).expect("a plan");
        vec![agent_repo::patch::write(root, &planned).expect("written")]
    }

    #[test]
    fn a_run_written_to_disk_comes_back_with_its_journal_intact() {
        let dir = tempfile::tempdir().expect("a temp repo");
        let stored = StoredSession::new(
            session("add a notes file"),
            journal_for(dir.path(), "notes.md", "agent words\n"),
        );
        let path = stored.write(dir.path()).expect("writable");
        assert_eq!(path, stored.path(dir.path()));
        assert!(
            path.to_string_lossy().contains(SESSIONS_DIR),
            "{}",
            path.display()
        );

        let read = StoredSession::read_file(&path).expect("readable");
        assert_eq!(read, stored, "the transcript and the journal both survive");
        assert_eq!(read.session.task, "add a notes file");
    }

    #[test]
    fn an_older_process_can_undo_the_run_a_file_records() {
        let dir = tempfile::tempdir().expect("a temp repo");
        let stored = StoredSession::new(
            session("write notes"),
            journal_for(dir.path(), "notes.md", "agent words\n"),
        );
        stored.write(dir.path()).expect("writable");
        let read = StoredSession::latest(dir.path())
            .expect("a directory to read")
            .expect("the run is there");

        let restored = read.restore(dir.path());
        assert_eq!(restored.len(), 1);
        assert!(
            !dir.path().join("notes.md").exists(),
            "the file the run created is gone"
        );
    }

    #[test]
    fn a_file_a_person_edited_afterwards_is_left_alone_by_a_rollback_from_disk() {
        let dir = tempfile::tempdir().expect("a temp repo");
        let stored = StoredSession::new(
            session("write notes"),
            journal_for(dir.path(), "notes.md", "agent words\n"),
        );
        std::fs::write(dir.path().join("notes.md"), "grown into something else\n")
            .expect("writable");

        let restored = stored.restore(dir.path());
        assert_eq!(
            restored[0].left_alone,
            vec![PathBuf::from("notes.md")],
            "work that grew after the run is not the run's to remove"
        );
        assert!(dir.path().join("notes.md").exists());
    }

    #[test]
    fn the_newest_run_is_the_one_latest_answers_with() {
        let dir = tempfile::tempdir().expect("a temp repo");
        let mut older = StoredSession::new(session("the older one"), Vec::new());
        older.stored_at_ms = 1_700_000_000_000;
        let mut newer = StoredSession::new(session("the newer one"), Vec::new());
        newer.stored_at_ms = 1_700_000_000_500;
        older.write(dir.path()).expect("writable");
        newer.write(dir.path()).expect("writable");

        let latest = StoredSession::latest(dir.path())
            .expect("readable")
            .expect("two runs stored");
        assert_eq!(latest.session.task, "the newer one");
        assert_eq!(latest.session.id, newer.session.id);
    }

    #[test]
    fn a_workspace_that_has_never_run_answers_that_it_has_not() {
        let dir = tempfile::tempdir().expect("a temp repo");
        assert!(StoredSession::latest(dir.path())
            .expect("no error")
            .is_none());
        let missing = StoredSession::read(dir.path(), Uuid::new_v4());
        assert!(
            matches!(missing, Err(AgentError::Io { .. })),
            "asking for a run by name that is not there is an error, not a silence"
        );
    }

    #[test]
    fn a_half_written_session_does_not_hide_the_runs_that_finished() {
        let dir = tempfile::tempdir().expect("a temp repo");
        let good = StoredSession::new(session("finished"), Vec::new());
        good.write(dir.path()).expect("writable");
        std::fs::create_dir_all(dir.path().join(SESSIONS_DIR)).expect("writable");
        std::fs::write(
            dir.path().join(SESSIONS_DIR).join("truncated.json"),
            "{\"session\":{\"id\":",
        )
        .expect("writable");

        let latest = StoredSession::latest(dir.path())
            .expect("readable")
            .expect("the whole file is still found");
        assert_eq!(latest.session.id, good.session.id);
    }

    #[test]
    fn a_journals_files_are_named_the_way_the_repository_names_them() {
        let dir = tempfile::tempdir().expect("a temp repo");
        let stored = StoredSession::new(
            session("two writes"),
            journal_for(dir.path(), "notes.md", "agent words\n"),
        );
        assert_eq!(
            stored.restorable(),
            vec![PathBuf::from("notes.md")],
            "relative, so a review printed next to the repository reads true"
        );
    }
}
