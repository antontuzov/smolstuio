//! Whether this action may run, and who has to say yes before it does.
//!
//! Two independent gates, checked in this order and combined:
//!
//! - the **sandbox policy** asks "is this within the boundaries this
//!   configuration drew" — the workspace root, the per-tool allow and deny
//!   lists, and the commands no configuration is allowed to permit. Its verdict
//!   is softened by [`SandboxMode`]: `strict` blocks, `warn` runs the action and
//!   says out loud that it would have stopped it, `off` records and moves on.
//! - the **approval mode** asks "does a human have to answer first". It is not
//!   softened by anything, because it is the user's own line rather than a
//!   guard rail around the model.
//!
//! The distinction is what keeps the two features from cancelling each other
//! out. `sandbox = "off"` with `approval_mode = "approve-edits"` still asks
//! before writing a file, and `autonomous-safe` still refuses `sudo`.

use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use agent_config::{ApprovalMode, Config, SandboxMode, ToolPolicy};
use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::{Deserialize, Serialize};

use crate::permission::Permission;

/// Programs this agent will not run at all, in any sandbox mode, because no
/// repository task needs them and every one of them can empty a disk.
///
/// A configuration cannot re-enable these; the human who wants them is one
/// terminal away, and that is the point.
const NEVER_PROGRAMS: [&str; 11] = [
    "sudo", "su", "doas", "mkfs", "fdisk", "diskutil", "dd", "shutdown", "reboot", "poweroff",
    "halt",
];

/// Programs whose purpose is to run other programs.
///
/// Admitting one of these would put a whole command line inside an argument and
/// hand back everything the "argv, never a shell" rule took away: `sh -c
/// "sudo rm -rf /"` hides a forbidden program behind an allowed one, and no
/// denylist can see through the quotes. A repository task names the program it
/// wants; an agent that needs a shell has a terminal.
const SHELL_PROGRAMS: [&str; 8] = ["sh", "bash", "zsh", "dash", "ksh", "csh", "fish", "tcsh"];

/// Programs that are legitimate but irreversible, so they always reach a human.
const DESTRUCTIVE_PROGRAMS: [&str; 9] = [
    "rm", "rmdir", "unlink", "shred", "chown", "chgrp", "pkill", "killall", "truncate",
];

/// Subcommands that turn an otherwise-read-only program into a destructive one.
const DESTRUCTIVE_SUBCOMMANDS: [(&str, &[&str]); 5] = [
    ("git", &["push --force"]),
    ("git", &["push -f"]),
    ("git", &["reset --hard"]),
    ("git", &["clean -f"]),
    ("git", &["checkout ."]),
];

/// What a tool is about to do, as much of it as a decision needs.
///
/// Paths are expected to be absolute and already resolved against the
/// workspace; [`Policy::resolve`] is the helper that produces them from what the
/// model wrote, and refuses the ones that climb out.
#[derive(Debug, Clone)]
pub struct Action<'a> {
    pub tool: &'a str,
    pub permission: Permission,
    pub paths: Vec<PathBuf>,
    pub command: Option<&'a [String]>,
}

impl<'a> Action<'a> {
    pub fn new(tool: &'a str, permission: Permission) -> Self {
        Self {
            tool,
            permission,
            paths: Vec::new(),
            command: None,
        }
    }

    pub fn path(mut self, path: impl Into<PathBuf>) -> Self {
        self.paths.push(path.into());
        self
    }

    pub fn command(self, argv: &'a [String]) -> Self {
        Self {
            command: Some(argv),
            ..self
        }
    }

    /// The string the allow and deny lists are matched against: the command
    /// line for a program, otherwise the first path.
    pub fn subject(&self) -> Option<String> {
        if let Some(argv) = self.command {
            return Some(argv.join(" "));
        }
        self.paths
            .first()
            .map(|path| path.to_string_lossy().into_owned())
    }
}

/// The verdict, and the reason a human or a transcript gets.
///
/// Its JSON is written by hand rather than derived, because the shape the audit
/// log wants is flat — `{"decision":"block","reason":"…"}` — and the derived
/// representations cannot produce one that reads back for the variant with no
/// reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allow,
    /// Ran, though the policy objected. Only `warn` produces this.
    Warn {
        reason: String,
    },
    /// Nothing runs until a person answers.
    Ask {
        reason: String,
    },
    /// Did not run.
    Block {
        reason: String,
    },
}

impl Decision {
    /// Whether the action goes ahead as asked.
    pub fn proceeds(&self) -> bool {
        matches!(self, Self::Allow | Self::Warn { .. })
    }

    pub fn needs_approval(&self) -> bool {
        matches!(self, Self::Ask { .. })
    }

    pub fn blocked(&self) -> bool {
        matches!(self, Self::Block { .. })
    }

    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::Allow => None,
            Self::Warn { reason } | Self::Ask { reason } | Self::Block { reason } => Some(reason),
        }
    }

    fn word(&self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Warn { .. } => "warn",
            Self::Ask { .. } => "ask",
            Self::Block { .. } => "block",
        }
    }

    fn weight(&self) -> u8 {
        match self {
            Self::Allow => 0,
            Self::Warn { .. } => 1,
            Self::Ask { .. } => 2,
            Self::Block { .. } => 3,
        }
    }

    /// The stricter of two verdicts, which is how the two gates combine: a
    /// policy that would warn and an approval mode that must ask yields a run
    /// that asks.
    pub fn combine(self, other: Self) -> Self {
        if other.weight() > self.weight() {
            other
        } else {
            self
        }
    }
}

impl std::fmt::Display for Decision {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.reason() {
            Some(reason) => write!(f, "{}: {reason}", self.word()),
            None => f.write_str(self.word()),
        }
    }
}

/// The flat form an audit line wants: a word and, when there is more to say,
/// the reason beside it.
impl Serialize for Decision {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct as _;
        let reason = self.reason();
        let mut line =
            serializer.serialize_struct("Decision", usize::from(reason.is_some()) + 1)?;
        line.serialize_field("decision", self.word())?;
        if let Some(reason) = reason {
            line.serialize_field("reason", reason)?;
        }
        line.end()
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DecisionLine {
    decision: String,
    #[serde(default)]
    reason: Option<String>,
}

impl<'de> Deserialize<'de> for Decision {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let line = DecisionLine::deserialize(deserializer)?;
        let reason = || line.reason.unwrap_or_default();
        match line.decision.as_str() {
            "allow" => Ok(Self::Allow),
            "warn" => Ok(Self::Warn { reason: reason() }),
            "ask" => Ok(Self::Ask { reason: reason() }),
            "block" => Ok(Self::Block { reason: reason() }),
            other => Err(serde::de::Error::unknown_variant(
                other,
                &["allow", "warn", "ask", "block"],
            )),
        }
    }
}

/// The boundaries one run of the agent is held to.
#[derive(Debug, Clone)]
pub struct Policy {
    root: PathBuf,
    approval: ApprovalMode,
    sandbox: SandboxMode,
    workspace_only: bool,
    tools: Vec<(String, CompiledPolicy)>,
}

#[derive(Debug, Clone)]
struct CompiledPolicy {
    enabled: bool,
    sandbox: Option<SandboxMode>,
    timeout: Option<Duration>,
    max_output_bytes: Option<usize>,
    allow: Option<GlobSet>,
    deny: Option<GlobSet>,
}

/// A list compiled once, so a malformed pattern is reported when the
/// configuration is read rather than in the middle of a run.
fn compile_list(entries: &[String]) -> Option<GlobSet> {
    if entries.is_empty() {
        return None;
    }
    let mut builder = GlobSetBuilder::new();
    for entry in entries {
        // A bare word matches a command's own name or any path ending in it; a
        // pattern with a glob character, a separator or a space in it matches
        // as written.
        let shaped = entry.contains(|character: char| {
            matches!(character, '*' | '?' | '[' | ']') || character == '/' || character == ' '
        });
        let plain = !shaped;
        if plain {
            if let Ok(glob) = Glob::new(&format!("**/{entry}")) {
                builder.add(glob);
            }
            if let Ok(glob) = Glob::new(entry) {
                builder.add(glob);
            }
        } else if let Ok(glob) = Glob::new(entry) {
            builder.add(glob);
        }
    }
    builder.build().ok()
}

impl Policy {
    /// A policy that holds everything to the workspace and asks before it
    /// writes, which is the shape `smoll init` configures.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            approval: ApprovalMode::default(),
            sandbox: SandboxMode::Strict,
            workspace_only: true,
            tools: Vec::new(),
        }
    }

    pub fn from_config(root: impl Into<PathBuf>, config: &Config) -> Self {
        let root = root.into();
        let tools = config
            .tools
            .iter()
            .map(|(name, policy)| {
                (
                    name.clone(),
                    CompiledPolicy {
                        enabled: policy.enabled,
                        sandbox: policy.sandbox,
                        timeout: policy.timeout_seconds.map(Duration::from_secs),
                        max_output_bytes: policy.max_output_bytes,
                        allow: compile_list(&policy.allowlist),
                        deny: compile_list(&policy.denylist),
                    },
                )
            })
            .collect();
        Self {
            root,
            approval: config.agent.approval_mode,
            // Strict until the run says otherwise, and then until a tool's own
            // entry says otherwise. A configuration that wants a softer
            // boundary has to either set `agent.sandbox` or name the tool it
            // means; `SMOLL_SANDBOX` sets the first.
            sandbox: config.agent.sandbox.unwrap_or(SandboxMode::Strict),
            workspace_only: config.privacy.workspace_only,
            tools,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn approval_mode(&self) -> ApprovalMode {
        self.approval
    }

    pub fn sandbox_mode(&self) -> SandboxMode {
        self.sandbox
    }

    pub fn with_approval(mut self, approval: ApprovalMode) -> Self {
        self.approval = approval;
        self
    }

    pub fn with_sandbox(mut self, sandbox: SandboxMode) -> Self {
        self.sandbox = sandbox;
        self
    }

    pub fn with_workspace_only(mut self, workspace_only: bool) -> Self {
        self.workspace_only = workspace_only;
        self
    }

    pub fn with_tool_policy(mut self, name: impl Into<String>, policy: &ToolPolicy) -> Self {
        let compiled = CompiledPolicy {
            enabled: policy.enabled,
            sandbox: policy.sandbox,
            timeout: policy.timeout_seconds.map(Duration::from_secs),
            max_output_bytes: policy.max_output_bytes,
            allow: compile_list(&policy.allowlist),
            deny: compile_list(&policy.denylist),
        };
        let name = name.into();
        match self.tools.iter_mut().find(|(known, _)| *known == name) {
            Some((_, known)) => *known = compiled,
            None => self.tools.push((name, compiled)),
        }
        self
    }

    /// Which tools this configuration switches off, so the registry can leave
    /// them out of the list the model is shown.
    pub fn is_enabled(&self, tool: &str) -> bool {
        self.compiled(tool).map_or(true, |policy| policy.enabled)
    }

    /// The sandbox mode that applies to one tool.
    pub fn mode_for(&self, tool: &str) -> SandboxMode {
        self.compiled(tool)
            .and_then(|policy| policy.sandbox)
            .unwrap_or(self.sandbox)
    }

    pub fn timeout_for(&self, tool: &str, fallback: Duration) -> Duration {
        self.compiled(tool)
            .and_then(|policy| policy.timeout)
            .unwrap_or(fallback)
    }

    pub fn max_output_for(&self, tool: &str, fallback: usize) -> usize {
        self.compiled(tool)
            .and_then(|policy| policy.max_output_bytes)
            .unwrap_or(fallback)
    }

    fn compiled(&self, tool: &str) -> Option<&CompiledPolicy> {
        self.tools
            .iter()
            .find(|(name, _)| name == tool)
            .map(|(_, policy)| policy)
    }

    /// Turn what the model wrote into a path, refusing the ones that leave.
    ///
    /// Lexical, not canonical: the file a tool is about to create does not
    /// exist yet, so there is nothing to resolve through the filesystem, and a
    /// `..` that climbs out of the workspace is refused on the way through
    /// rather than after an escape has been followed.
    pub fn resolve(&self, raw: &str) -> Result<PathBuf, String> {
        let raw = raw.trim();
        if raw.is_empty() {
            return Err("an empty path".to_owned());
        }
        let given = Path::new(raw);
        if given.is_absolute() {
            let path = normalize(given);
            if !self.contains(&path) {
                return Err(format!(
                    "{raw} is an absolute path outside the workspace {}",
                    self.root.display()
                ));
            }
            return Ok(path);
        }
        // Walked segment by segment rather than joined and cleaned, so that a
        // `..` which would leave the workspace is caught at the moment it does.
        let mut segments: Vec<&std::ffi::OsStr> = Vec::new();
        for component in given.components() {
            match component {
                Component::CurDir => {}
                Component::ParentDir if segments.is_empty() => {
                    return Err(format!(
                        "{raw} climbs out of the workspace {}",
                        self.root.display()
                    ));
                }
                Component::ParentDir => {
                    segments.pop();
                }
                Component::Normal(name) => segments.push(name),
                // Only an absolute path can carry these, and it took the branch
                // above.
                Component::Prefix(_) | Component::RootDir => {}
            }
        }
        let mut path = self.root.clone();
        for segment in segments {
            path.push(segment);
        }
        Ok(path)
    }

    /// Whether a path a tool wants to touch is inside the workspace.
    pub fn contains(&self, path: &Path) -> bool {
        !self.workspace_only || starts_with(&self.root, path)
    }

    /// The full verdict for one action.
    pub fn check(&self, action: &Action<'_>) -> Decision {
        if !self.is_enabled(action.tool) {
            return Decision::Block {
                reason: format!(
                    "{} is disabled in the configuration, so this build will not run it",
                    action.tool
                ),
            };
        }
        let mode = self.mode_for(action.tool);
        let gated = if let Some(reason) = self.forbidden(action) {
            // No sandbox mode softens these: `off` means "let my own denylists
            // slide", not "run `dd`".
            Decision::Block { reason }
        } else {
            match (self.violation(action), mode) {
                (None, _) => Decision::Allow,
                (Some(reason), SandboxMode::Strict) => Decision::Block { reason },
                (Some(reason), SandboxMode::Warn) => Decision::Warn { reason },
                // Said nothing and ran anyway: the violation is still recorded,
                // it just has no verdict attached to it.
                (Some(_), SandboxMode::Off) => Decision::Allow,
            }
        };
        gated.combine(self.approval_for(action.permission))
    }

    /// The one class that never runs on its own, in any approval mode.
    fn approval_for(&self, permission: Permission) -> Decision {
        match permission {
            Permission::ReadOnly => Decision::Allow,
            Permission::Write if !self.approval.allows_write() => Decision::Block {
                reason: format!(
                    "{} proposes diffs and writes nothing; approval_mode = \"{}\" is what makes \
                     that true",
                    self.approval, self.approval
                ),
            },
            Permission::Write if self.approval.auto_writes() => Decision::Allow,
            Permission::Write => Decision::Ask {
                reason: "a file is about to be written".to_owned(),
            },
            Permission::Execute if self.approval.auto_commands() => Decision::Allow,
            Permission::Execute => Decision::Ask {
                reason: "a program is about to be run".to_owned(),
            },
            Permission::Network => Decision::Ask {
                reason: "this action reaches the network, which no default configuration does \
                         on its own"
                    .to_owned(),
            },
            Permission::Destructive if !self.approval.allows_write() => Decision::Block {
                reason: format!(
                    "approval_mode = \"{}\" does not let anything be removed",
                    self.approval
                ),
            },
            Permission::Destructive => Decision::Ask {
                reason: "this cannot be undone by writing the file back".to_owned(),
            },
        }
    }

    /// A violation the sandbox mode is not allowed to soften.
    fn forbidden(&self, action: &Action<'_>) -> Option<String> {
        let argv = action.command?;
        let program = argv.first().map(|word| program_of(word))?;
        if NEVER_PROGRAMS.contains(&program.as_str()) {
            return Some(format!(
                "`{program}` is not a program this agent will run, whatever the sandbox says"
            ));
        }
        if SHELL_PROGRAMS.contains(&program.as_str()) {
            return Some(format!(
                "`{program}` is a shell, and this agent runs programs rather than shell \
                 command lines: name the program you want instead"
            ));
        }
        if program == "rm" && recursive_and_force(argv) {
            if let Some(target) = argv.iter().skip(1).find(|arg| broad_target(arg)) {
                return Some(format!(
                    "`rm {target}` is a deletion this agent will not attempt"
                ));
            }
        }
        None
    }

    /// A violation the sandbox mode decides how loudly to make.
    fn violation(&self, action: &Action<'_>) -> Option<String> {
        for path in &action.paths {
            if !self.contains(path) {
                return Some(format!(
                    "{} is outside the workspace {}",
                    path.display(),
                    self.root.display()
                ));
            }
        }
        if let Some(argv) = action.command {
            if destructive_subcommand(argv) {
                let line = argv.join(" ");
                return Some(format!("`{line}` throws away work that is not committed"));
            }
        }
        let policy = self.compiled(action.tool)?;
        let subjects = self.subjects(action);
        if let Some(deny) = &policy.deny {
            // Every thing this call touches, not just the first: a tool that
            // stages three files must not slip a denied one through by naming
            // it second.
            if let Some(hit) = subjects.iter().find(|subject| deny.is_match(subject)) {
                return Some(format!("{hit} is on the denylist for {}", action.tool));
            }
        }
        if let Some(allow) = &policy.allow {
            if let Some(miss) = subjects.iter().find(|subject| !allow.is_match(subject)) {
                return Some(format!(
                    "{miss} is not on the allowlist for {}",
                    action.tool
                ));
            }
        }
        None
    }

    /// Everything the lists are matched against: the command line if there is
    /// one, otherwise each path the call touches.
    ///
    /// A path inside the workspace is offered relative, because that is the
    /// only spelling an author can write down in a file they expect to commit;
    /// a path that is not inside it keeps its absolute form, so an entry naming
    /// `/etc/hosts` means that file rather than a coincidence of suffixes.
    fn subjects(&self, action: &Action<'_>) -> Vec<String> {
        if let Some(argv) = action.command {
            return vec![argv.join(" ")];
        }
        action
            .paths
            .iter()
            .map(|path| {
                let relative = match path.strip_prefix(&self.root) {
                    Ok(relative) if !relative.as_os_str().is_empty() => relative,
                    _ => path.as_path(),
                };
                relative.to_string_lossy().into_owned()
            })
            .collect()
    }
}

/// What a command actually does to the machine, which the model does not get to
/// decide.
///
/// A tool that runs programs cannot declare itself read-only because the
/// program it was handed removes the working tree: the class comes from the
/// argv, so `run_command` asks a human before `rm` and not before `cargo test`.
pub fn classify(argv: &[String]) -> Permission {
    let Some(program) = argv.first().map(|word| program_of(word)) else {
        return Permission::Execute;
    };
    if DESTRUCTIVE_PROGRAMS.contains(&program.as_str()) || destructive_subcommand(argv) {
        Permission::Destructive
    } else {
        Permission::Execute
    }
}

fn starts_with(root: &Path, path: &Path) -> bool {
    path.starts_with(root)
}

/// The program a path or command names, without its directory.
fn program_of(word: &str) -> String {
    Path::new(word)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| word.to_owned())
}

fn recursive_and_force(argv: &[String]) -> bool {
    let mut recursive = false;
    let mut force = false;
    for arg in argv.iter().skip(1) {
        if arg == "--recursive" {
            recursive = true;
        } else if arg == "--force" {
            force = true;
        } else if let Some(flags) = arg.strip_prefix('-') {
            recursive |= flags.contains('r') || flags.contains('R');
            force |= flags.contains('f');
        }
    }
    recursive && force
}

/// Whether an `rm` target is a whole tree rather than the one file a refactor
/// left behind.
fn broad_target(arg: &str) -> bool {
    if arg.starts_with('-') {
        return false;
    }
    arg == "/" || arg == "~" || arg.ends_with('/') || arg.contains('*') || arg.contains("**")
}

fn destructive_subcommand(argv: &[String]) -> bool {
    let Some(program) = argv.first().map(|word| program_of(word)) else {
        return false;
    };
    let rest = argv[1..].join(" ");
    DESTRUCTIVE_SUBCOMMANDS
        .iter()
        .filter(|(name, _)| *name == program.as_str())
        .any(|(_, forms)| forms.iter().any(|form| *form == rest))
}

/// Drop `.` components and apply `..` without ever climbing past the root.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    let mut depth = 0usize;
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => out.push(prefix.as_os_str()),
            Component::RootDir => out.push(Path::new("/")),
            Component::CurDir => {}
            Component::ParentDir => {
                // Only pop components that came after the root we started from.
                if depth > 0 {
                    out.pop();
                    depth -= 1;
                }
            }
            Component::Normal(name) => {
                out.push(name);
                depth += 1;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> Policy {
        Policy::new("/repo")
    }

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| (*word).to_owned()).collect()
    }

    #[test]
    fn a_relative_path_lands_inside_the_workspace() {
        let resolved = policy().resolve("src/main.rs").expect("inside");
        assert_eq!(resolved, PathBuf::from("/repo/src/main.rs"));
    }

    #[test]
    fn a_path_that_climbs_out_is_refused_before_it_runs() {
        let error = policy().resolve("../secrets/id_rsa").unwrap_err();
        assert!(error.contains("climbs out"), "{error}");
        assert!(error.contains("/repo"), "{error}");
        let error = policy().resolve("/etc/passwd").unwrap_err();
        assert!(error.contains("outside"), "{error}");
        assert!(policy().resolve("./src").expect("inside").ends_with("src"));
    }

    #[test]
    fn a_dot_dot_that_stops_inside_the_workspace_is_allowed() {
        let resolved = policy().resolve("crates/../README.md").expect("inside");
        assert_eq!(resolved, PathBuf::from("/repo/README.md"));
    }

    #[test]
    fn a_workspace_that_lets_paths_anywhere_does_not() {
        let policy = policy().with_workspace_only(false);
        assert_eq!(
            policy.resolve("/tmp/anything").expect("allowed"),
            PathBuf::from("/tmp/anything")
        );
    }

    #[test]
    fn a_read_needs_nobody() {
        let action = Action::new("read_file", Permission::ReadOnly).path("/repo/src/lib.rs");
        assert_eq!(policy().check(&action), Decision::Allow);
    }

    #[test]
    fn a_write_asks_in_the_default_mode_and_blocks_in_suggest_only() {
        let action = Action::new("write_file", Permission::Write).path("/repo/src/lib.rs");
        let decision = policy().check(&action);
        assert!(decision.needs_approval(), "{decision}");
        assert!(decision.reason().unwrap().contains("written"), "{decision}");

        let strict = policy().with_approval(ApprovalMode::SuggestOnly);
        let decision = strict.check(&action);
        assert!(decision.blocked(), "{decision}");

        let automatic = policy().with_approval(ApprovalMode::ApproveCommands);
        assert_eq!(automatic.check(&action), Decision::Allow);
    }

    #[test]
    fn a_command_asks_when_commands_are_gated() {
        let command = argv(&["cargo", "test"]);
        let action = Action::new("run_command", Permission::Execute)
            .path("/repo")
            .command(&command);
        assert!(policy().check(&action).proceeds());

        let gating = policy().with_approval(ApprovalMode::ApproveCommands);
        assert!(gating.check(&action).needs_approval());
    }

    #[test]
    fn removing_a_file_reaches_a_human_in_every_approval_mode() {
        let command = argv(&["rm", "build.rs"]);
        let action = Action::new("run_command", Permission::Destructive).command(&command);
        for mode in ApprovalMode::ALL {
            if mode == ApprovalMode::SuggestOnly {
                continue;
            }
            let decision = policy().with_approval(mode).check(&action);
            assert!(decision.needs_approval(), "{mode} allowed {decision}");
        }
    }

    #[test]
    fn the_programs_that_are_never_run_stay_never_run() {
        for program in NEVER_PROGRAMS {
            let command = argv(&[program, "--help"]);
            let action = Action::new("run_command", Permission::Execute).command(&command);
            for mode in SandboxMode::ALL {
                let decision = policy().with_sandbox(mode).check(&action);
                assert!(
                    decision.blocked(),
                    "{program} ran under sandbox = {mode}: {decision}"
                );
            }
        }
    }

    #[test]
    fn a_shell_never_runs_because_it_would_run_anything() {
        for shell in SHELL_PROGRAMS {
            // The payload is a command this policy blocks outright, and the
            // point is that the sandbox never gets to see it: `sh` is refused
            // before its arguments mean anything.
            let command = argv(&[shell, "-c", "sudo rm -rf /"]);
            let action = Action::new("run_command", Permission::Execute).command(&command);
            for mode in SandboxMode::ALL {
                let decision = policy().with_sandbox(mode).check(&action);
                assert!(decision.blocked(), "{shell} ran under sandbox = {mode}");
                assert!(
                    decision.reason().unwrap().contains("shell"),
                    "the refusal says why: {decision}"
                );
            }
        }
    }

    #[test]
    fn a_recursive_force_rm_of_a_tree_is_refused_but_not_of_one_file() {
        let broad = argv(&["rm", "-rf", "/"]);
        let action = Action::new("run_command", Permission::Destructive).command(&broad);
        assert!(policy().check(&action).blocked());

        let one = argv(&["rm", "-f", "stale.rs"]);
        let action = Action::new("run_command", Permission::Destructive).command(&one);
        assert!(!policy().check(&action).blocked());
    }

    #[test]
    fn a_path_outside_the_workspace_is_blocked_then_warned_then_ignored() {
        let action = Action::new("read_file", Permission::ReadOnly).path("/etc/passwd");
        let strict = policy().check(&action);
        assert!(strict.blocked(), "{strict}");
        assert!(strict.reason().unwrap().contains("/etc/passwd"));

        let warning = policy().with_sandbox(SandboxMode::Warn).check(&action);
        assert_eq!(
            warning.reason(),
            strict.reason(),
            "the warning says what the block said"
        );
        assert!(warning.proceeds());

        let off = policy().with_sandbox(SandboxMode::Off).check(&action);
        assert_eq!(off, Decision::Allow);
    }

    #[test]
    fn a_denylist_entry_stops_a_command_and_an_allowlist_stops_everything_else() {
        let tool = ToolPolicy {
            sandbox: Some(SandboxMode::Strict),
            denylist: vec!["cargo publish".to_owned()],
            ..ToolPolicy::default()
        };
        let policy = policy().with_tool_policy("run_command", &tool);

        let publish = argv(&["cargo", "publish"]);
        let action = Action::new("run_command", Permission::Execute).command(&publish);
        assert!(policy.check(&action).blocked(), "{publish:?}");

        let test = argv(&["cargo", "test"]);
        let action = Action::new("run_command", Permission::Execute).command(&test);
        assert!(policy.check(&action).proceeds());
    }

    #[test]
    fn an_allowlist_only_admits_what_it_names() {
        let tool = ToolPolicy {
            sandbox: Some(SandboxMode::Strict),
            allowlist: vec!["src/**".to_owned()],
            ..ToolPolicy::default()
        };
        let policy = policy().with_tool_policy("read_file", &tool);

        let inside = Action::new("read_file", Permission::ReadOnly).path("/repo/src/lib.rs");
        assert!(policy.check(&inside).proceeds());

        let outside = Action::new("read_file", Permission::ReadOnly).path("/repo/docs/lib.md");
        let decision = policy.check(&outside);
        assert!(decision.blocked(), "{decision}");
        assert!(decision.reason().unwrap().contains("allowlist"));
    }

    /// A tool that touches several things at once — `git_add` stages a list of
    /// paths — must not be able to hide one of them by naming it second. This is
    /// the difference between a denylist that gates the call and one that gates
    /// the first argument.
    #[test]
    fn every_path_a_call_touches_is_gated_not_only_the_first() {
        let deny = ToolPolicy {
            sandbox: Some(SandboxMode::Strict),
            denylist: vec!["secrets.env".to_owned()],
            ..ToolPolicy::default()
        };
        let deny_policy = policy().with_tool_policy("git_add", &deny);
        let call = Action::new("git_add", Permission::Write)
            .path("/repo/src/lib.rs")
            .path("/repo/secrets.env");
        let decision = deny_policy.check(&call);
        assert!(decision.blocked(), "{decision}");
        assert!(
            decision.reason().unwrap().contains("secrets.env"),
            "the finding names the path that was refused: {}",
            decision.reason().unwrap_or_default()
        );

        let allow = ToolPolicy {
            sandbox: Some(SandboxMode::Strict),
            allowlist: vec!["src/**".to_owned()],
            ..ToolPolicy::default()
        };
        let allow_policy = policy().with_tool_policy("git_add", &allow);
        let outside = Action::new("git_add", Permission::Write)
            .path("/repo/src/lib.rs")
            .path("/repo/docs/notes.md");
        let decision = allow_policy.check(&outside);
        assert!(decision.blocked(), "{decision}");
        assert!(
            decision.reason().unwrap().contains("docs/notes.md"),
            "an allowlist that only saw the first path would have let this through: {}",
            decision.reason().unwrap_or_default()
        );

        // Both paths are on the list, so nothing is refused — and a write is
        // still a write: the approval mode asks about it, which is not the same
        // finding and should not be reported as one.
        let inside = Action::new("git_add", Permission::Write)
            .path("/repo/src/lib.rs")
            .path("/repo/src/main.rs");
        let decision = allow_policy.check(&inside);
        assert!(!decision.blocked(), "{decision}");
        assert!(decision.needs_approval(), "{decision}");
    }

    #[test]
    fn a_disabled_tool_is_blocked_even_though_the_registry_will_not_offer_it() {
        let tool = ToolPolicy {
            enabled: false,
            ..ToolPolicy::default()
        };
        let policy = policy().with_tool_policy("run_command", &tool);
        let command = argv(&["echo", "hi"]);
        let action = Action::new("run_command", Permission::Execute).command(&command);
        let decision = policy.check(&action);
        assert!(decision.blocked(), "{decision}");
        assert!(!policy.is_enabled("run_command"));
        assert!(policy.is_enabled("read_file"), "unlisted means on");
    }

    #[test]
    fn per_tool_limits_override_the_tools_own() {
        let tool = ToolPolicy {
            timeout_seconds: Some(5),
            max_output_bytes: Some(1024),
            sandbox: Some(SandboxMode::Warn),
            ..ToolPolicy::default()
        };
        let policy = policy().with_tool_policy("read_file", &tool);
        assert_eq!(
            policy.timeout_for("read_file", Duration::from_secs(30)),
            Duration::from_secs(5)
        );
        assert_eq!(policy.max_output_for("read_file", 8192), 1024);
        assert_eq!(policy.mode_for("read_file"), SandboxMode::Warn);
        assert_eq!(
            policy.timeout_for("write_file", Duration::from_secs(30)),
            Duration::from_secs(30),
            "a tool with no entry keeps the default"
        );
    }

    #[test]
    fn the_stricter_of_two_verdicts_wins() {
        let warning = Decision::Warn {
            reason: "outside".to_owned(),
        };
        assert_eq!(
            warning.clone().combine(Decision::Ask {
                reason: "writes".to_owned()
            }),
            Decision::Ask {
                reason: "writes".to_owned()
            }
        );
        assert_eq!(
            Decision::Ask {
                reason: "writes".to_owned()
            }
            .combine(warning.clone()),
            Decision::Ask {
                reason: "writes".to_owned()
            },
            "a warning cannot talk the agent out of asking"
        );
        assert_eq!(
            Decision::Allow.combine(Decision::Warn {
                reason: "x".to_owned()
            }),
            Decision::Warn {
                reason: "x".to_owned()
            }
        );
    }

    #[test]
    fn a_configured_policy_carries_the_approval_mode_and_the_workspace_rule() {
        let mut config = Config::default();
        config.agent.approval_mode = ApprovalMode::AutonomousSafe;
        config.privacy.workspace_only = false;
        let policy = Policy::from_config("/repo", &config);
        assert_eq!(policy.approval_mode(), ApprovalMode::AutonomousSafe);
        assert!(!policy.workspace_only);
        assert_eq!(policy.sandbox_mode(), SandboxMode::Strict);
    }

    /// `SMOLL_SANDBOX` is the fast way to loosen or tighten a whole run, so it
    /// has to reach a tool the configuration never mentions. A tool with its own
    /// section still answers for itself.
    #[test]
    fn the_run_wide_sandbox_reaches_tools_with_no_section() {
        let mut config = Config::default();
        config.agent.sandbox = Some(SandboxMode::Warn);
        config.tools.insert(
            "read_file".to_owned(),
            ToolPolicy {
                sandbox: Some(SandboxMode::Strict),
                ..ToolPolicy::default()
            },
        );
        let policy = Policy::from_config("/repo", &config);
        assert_eq!(policy.mode_for("search_text"), SandboxMode::Warn);
        assert_eq!(policy.mode_for("read_file"), SandboxMode::Strict);
    }

    #[test]
    fn a_git_command_that_throws_away_work_is_destructive() {
        let hard = argv(&["git", "reset", "--hard"]);
        let action = Action::new("run_command", Permission::Execute).command(&hard);
        assert!(destructive_subcommand(&hard));
        // Blocked by the default strict policy, whatever the approval mode says.
        assert!(policy()
            .with_approval(ApprovalMode::AutonomousSafe)
            .check(&action)
            .blocked());
        assert!(!destructive_subcommand(&argv(&["git", "status"])));
    }

    #[test]
    fn the_command_line_decides_the_class_not_the_tool() {
        assert_eq!(
            classify(&argv(&["cargo", "test"])),
            Permission::Execute,
            "a test run is not a deletion"
        );
        assert_eq!(classify(&argv(&["rm", "one.rs"])), Permission::Destructive);
        assert_eq!(
            classify(&argv(&["/usr/bin/git", "push", "--force"])),
            Permission::Destructive,
            "an absolute program name is the same program"
        );
        assert_eq!(classify(&argv(&["git", "diff"])), Permission::Execute);
        assert_eq!(classify(&argv(&[])), Permission::Execute);
    }

    #[test]
    fn a_decision_says_what_it_did_in_one_line() {
        assert_eq!(Decision::Allow.to_string(), "allow");
        assert_eq!(
            Decision::Block {
                reason: "no sudo".to_owned()
            }
            .to_string(),
            "block: no sudo"
        );
        let json = serde_json::to_string(&Decision::Ask {
            reason: "writes".to_owned(),
        })
        .expect("serialisable");
        assert!(json.contains("\"decision\":\"ask\""), "{json}");
        let back: Decision = serde_json::from_str(&json).expect("parses back");
        assert_eq!(back.reason(), Some("writes"));
    }
}
