//! What kind of project this is, and the commands that tell the agent whether
//! its work broke it.
//!
//! Detection reads manifests as text rather than asking the build tool:
//! `cargo metadata` resolves the dependency graph, which can reach the network,
//! and an agent that shells out to learn what a project is has run a command
//! before it knows whether it should have. So the answer comes from the files on
//! disk, and a manifest that does not parse is reported rather than guessed
//! around.
//!
//! Commands come back as argument lists, never as a line for a shell, because
//! the loop hands them to the sandbox and the sandbox refuses a shell on
//! purpose.

use serde::{Deserialize, Serialize};

use crate::index::{FileEntry, Index};

const CARGO_MANIFEST: &str = "Cargo.toml";
const NODE_MANIFEST: &str = "package.json";
const PYTHON_MANIFESTS: [&str; 2] = ["pyproject.toml", "setup.py"];
const MAKE_MANIFESTS: [&str; 3] = ["Makefile", "makefile", "GNUmakefile"];

/// The kind of project a repository declared itself to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Ecosystem {
    Cargo,
    Node,
    Python,
    Make,
    Unknown,
}

impl Ecosystem {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cargo => "cargo",
            Self::Node => "node",
            Self::Python => "python",
            Self::Make => "make",
            Self::Unknown => "unknown",
        }
    }

    /// Cargo first, because a Rust workspace that also ships a frontend is a
    /// Rust project with a frontend, not a frontend that happens to build Rust.
    fn priority(self) -> u8 {
        match self {
            Self::Cargo => 0,
            Self::Node => 1,
            Self::Python => 2,
            Self::Make => 3,
            Self::Unknown => 4,
        }
    }
}

/// Why the agent would run a command.
///
/// The loop and the tools ask by purpose rather than by name, so an agent that
/// wants to know whether its change compiles and a person who knows the command
/// for that reach for the same argv.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Purpose {
    Validate,
    /// Does it compile and type-check, without running the suite? Only a
    /// project that says how is offered one: cargo has `cargo check`, a Python
    /// project with `[tool.mypy]` has `mypy`, and a script called `check` is
    /// already the suite.
    Check,
    Build,
    Lint,
    Format,
}

impl Purpose {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Validate => "validate",
            Self::Check => "check",
            Self::Build => "build",
            Self::Lint => "lint",
            Self::Format => "format",
        }
    }

    /// The script and target names that mean this. Anything else is not turned
    /// into a command: a script called `deploy` is not an invitation.
    fn from_name(name: &str) -> Option<Self> {
        match name {
            "test" | "tests" | "check" | "ci" | "verify" => Some(Self::Validate),
            "lint" | "eslint" | "clippy" | "typecheck" | "check-types" => Some(Self::Lint),
            "fmt" | "format" | "prettier" | "style" => Some(Self::Format),
            "build" | "compile" | "dist" => Some(Self::Build),
            _ => None,
        }
    }
}

/// One command the project says is worth running.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Suggestion {
    pub purpose: Purpose,
    pub ecosystem: Ecosystem,
    /// The directory to run it in, relative to the repository root; `""` is the
    /// root itself. A nested manifest means the build lives there, not here.
    pub directory: String,
    pub argv: Vec<String>,
    /// The file and field this came from, so a reader can check the guess.
    pub reason: String,
}

impl Suggestion {
    pub fn at_root(&self) -> bool {
        self.directory.is_empty()
    }

    /// The command as a human would type it, quoting what needs quoting.
    pub fn display(&self) -> String {
        self.argv
            .iter()
            .map(|argument| {
                if argument.is_empty() || argument.chars().any(|c| c.is_whitespace()) {
                    format!("\"{argument}\"")
                } else {
                    argument.clone()
                }
            })
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// One buildable thing a manifest declared.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Crate {
    pub name: String,
    pub directory: String,
    pub has_library: bool,
    pub has_binary: bool,
    /// A `tests/` directory of its own, which is where an integration test goes
    /// and therefore where a change here should be checked.
    pub has_tests: bool,
}

impl Crate {
    /// The narrowest command that still checks this crate, because a failure in
    /// an unrelated crate should not stop a run.
    pub fn validate_argv(&self) -> Vec<String> {
        vec![
            "cargo".to_owned(),
            "test".to_owned(),
            "-p".to_owned(),
            self.name.clone(),
        ]
    }
}

/// The repository as a build.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Project {
    pub name: String,
    pub ecosystem: Ecosystem,
    pub crates: Vec<Crate>,
    pub suggestions: Vec<Suggestion>,
    /// What detection could not say: a manifest that did not parse, a second
    /// ecosystem, a workspace whose members name no manifest.
    pub notes: Vec<String>,
}

impl Project {
    /// Read the manifests the index found.
    pub fn detect(index: &Index) -> Self {
        let mut project = Self {
            name: String::new(),
            ecosystem: Ecosystem::Unknown,
            crates: Vec::new(),
            suggestions: Vec::new(),
            notes: Vec::new(),
        };
        let mut found = Vec::new();
        for (ecosystem, detect) in [
            (Ecosystem::Cargo, cargo as fn(&Index, &mut Project) -> bool),
            (Ecosystem::Node, node),
            (Ecosystem::Python, python),
            (Ecosystem::Make, make),
        ] {
            if detect(index, &mut project) {
                found.push(ecosystem);
            }
        }
        found.sort_by_key(|ecosystem| ecosystem.priority());
        found.dedup();
        if let Some(top) = found.first() {
            project.ecosystem = *top;
        }
        if found.len() > 1 {
            let names: Vec<&str> = found.iter().map(|ecosystem| ecosystem.as_str()).collect();
            project.notes.push(format!(
                "more than one ecosystem here: {}",
                names.join(", ")
            ));
        }
        if project.name.is_empty() {
            project.name = directory_name(index);
        }
        project.suggestions.sort_by(|a, b| {
            a.purpose
                .cmp(&b.purpose)
                .then(a.ecosystem.priority().cmp(&b.ecosystem.priority()))
                .then_with(|| a.display().cmp(&b.display()))
        });
        project.crates.sort_by(|a, b| a.directory.cmp(&b.directory));
        project
    }

    /// The first command for a purpose, which is the one worth running: the list
    /// is sorted so the primary ecosystem and the root directory come first.
    pub fn for_purpose(&self, purpose: Purpose) -> Option<&Suggestion> {
        self.suggestions
            .iter()
            .find(|suggestion| suggestion.purpose == purpose)
    }

    pub fn validate(&self) -> Option<&Suggestion> {
        self.for_purpose(Purpose::Validate)
    }

    /// The crate a path belongs to, which is the deepest one that contains it.
    pub fn crate_for(&self, path: &str) -> Option<&Crate> {
        let wanted = path.trim_start_matches("./").replace('\\', "/");
        self.crates
            .iter()
            .filter(|crate_| {
                !crate_.directory.is_empty()
                    && (wanted == crate_.directory
                        || wanted.starts_with(&format!("{}/", crate_.directory)))
            })
            .max_by_key(|crate_| crate_.directory.len())
    }

    pub fn is_workspace(&self) -> bool {
        self.crates.len() > 1
    }

    /// One line for a prompt header or `smoll doctor`.
    pub fn describe(&self) -> String {
        let mut kind = self.ecosystem.as_str().to_owned();
        if self.is_workspace() {
            kind.push_str(&format!(", {} crates", self.crates.len()));
        }
        let mut text = format!("{} ({})", self.name, kind);
        match self.validate() {
            Some(suggestion) => text.push_str(&format!(" — validate: {}", suggestion.display())),
            None => text.push_str(" — no validate command found"),
        }
        text
    }
}

/// The name of the root directory, used when no manifest declares a better one.
fn directory_name(index: &Index) -> String {
    index
        .root
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| index.root.to_string_lossy().into_owned())
}

/// The manifest with one of these names that is nearest the repository root,
/// which is the one that says what the project is rather than what one part of
/// it is.
fn nearest<'a>(index: &'a Index, names: &[&str]) -> Option<&'a FileEntry> {
    index
        .files
        .iter()
        .filter(|entry| names.contains(&entry.file_name()))
        .min_by_key(|entry| entry.depth)
}

/// A path inside a directory, spelled the way an index spells the root itself.
fn under(directory: &str, rest: &str) -> String {
    if directory.is_empty() {
        rest.to_owned()
    } else {
        format!("{directory}/{rest}")
    }
}

fn suggestion(
    purpose: Purpose,
    ecosystem: Ecosystem,
    directory: String,
    argv: Vec<String>,
    reason: String,
) -> Suggestion {
    Suggestion {
        purpose,
        ecosystem,
        directory,
        argv,
        reason,
    }
}

fn words(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| (*item).to_owned()).collect()
}

/// A Cargo workspace or package, read from the manifests themselves.
fn cargo(index: &Index, project: &mut Project) -> bool {
    let Some(primary) = nearest(index, &[CARGO_MANIFEST]) else {
        return false;
    };
    let directory = primary.directory().to_owned();
    let Some(text) = index.read(&primary.path) else {
        project.notes.push(format!(
            "{} is not readable as text, so the workspace is unknown",
            primary.path
        ));
        return true;
    };
    let Ok(value) = text.parse::<toml::Value>() else {
        project.notes.push(format!(
            "{} did not parse as TOML, so no crates or commands were detected",
            primary.path
        ));
        return true;
    };

    let listed = member_globs(
        value
            .get("workspace")
            .and_then(|workspace| workspace.get("members")),
        &prefix_of(&directory),
        &mut project.notes,
    );
    let excluded = string_list(
        value
            .get("workspace")
            .and_then(|workspace| workspace.get("exclude")),
    )
    .into_iter()
    .map(|pattern| under(&directory, &pattern))
    .collect::<Vec<_>>();

    let mut declared = 0usize;
    for manifest in index
        .files
        .iter()
        .filter(|entry| entry.file_name() == CARGO_MANIFEST)
    {
        let here = manifest.directory();
        if here != directory && !listed.matches(here) {
            continue;
        }
        if excluded.iter().any(|pattern| pattern == here) {
            continue;
        }
        let read = index
            .read(&manifest.path)
            .and_then(|text| text.parse::<toml::Value>().ok());
        let Some(package) = read.as_ref().and_then(|value| value.get("package")) else {
            // A virtual workspace root has no package of its own, and a manifest
            // that does not parse is reported once, not per member.
            continue;
        };
        declared += 1;
        let name = package
            .get("name")
            .and_then(|name| name.as_str())
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| here.rsplit('/').next().unwrap_or(here).to_owned());
        if here == directory && project.name.is_empty() {
            // Only the manifest that is the project's own names it: a member
            // crate is a part, not the whole.
            project.name = name.clone();
        }
        project.crates.push(Crate {
            has_library: index.contains(&under(here, "src/lib.rs")),
            has_binary: index.contains(&under(here, "src/main.rs"))
                || index
                    .files
                    .iter()
                    .any(|entry| entry.path.starts_with(&under(here, "src/bin/"))),
            has_tests: index
                .files
                .iter()
                .any(|entry| entry.path.starts_with(&under(here, "tests/"))),
            name,
            directory: here.to_owned(),
        });
    }
    if declared == 0 {
        project.notes.push(format!(
            "{} names no package the agent can find, so there is nothing to run",
            primary.path
        ));
        return true;
    }

    let wide = project.crates.len() > 1;
    let reason = format!(
        "{} declares {}",
        primary.path,
        if wide { "a workspace" } else { "one package" }
    );
    let every = |sub: &str, extra: &[&str], purpose: Purpose| {
        suggestion(
            purpose,
            Ecosystem::Cargo,
            directory.clone(),
            cargo_command(sub, wide, extra),
            reason.clone(),
        )
    };
    project
        .suggestions
        .push(every("test", &[], Purpose::Validate));
    project
        .suggestions
        .push(every("check", &[], Purpose::Check));
    project.suggestions.push(every(
        "clippy",
        &["--all-targets", "--", "-D", "warnings"],
        Purpose::Lint,
    ));
    project.suggestions.push(every("fmt", &[], Purpose::Format));
    project
        .suggestions
        .push(every("build", &[], Purpose::Build));
    true
}

/// `cargo fmt` spells the workspace flag `--all` and the rest of them spell it
/// `--workspace`, which is the kind of thing a small model should not have to
/// remember.
fn cargo_command(sub: &str, wide: bool, extra: &[&str]) -> Vec<String> {
    let mut argv = vec!["cargo".to_owned(), sub.to_owned()];
    if wide {
        argv.push(if sub == "fmt" { "--all" } else { "--workspace" }.to_owned());
    }
    argv.extend(words(extra));
    argv
}

/// The member patterns of a workspace, as globs over the index's own paths.
#[derive(Default)]
struct Members {
    literal: Vec<String>,
    globs: globset::GlobSet,
}

impl Members {
    fn matches(&self, directory: &str) -> bool {
        self.literal.iter().any(|listed| listed == directory) || self.globs.is_match(directory)
    }
}

fn member_globs(members: Option<&toml::Value>, prefix: &str, notes: &mut Vec<String>) -> Members {
    let mut listed = Members::default();
    let Some(toml::Value::Array(items)) = members else {
        return listed;
    };
    let mut builder = globset::GlobSetBuilder::new();
    for item in items {
        let Some(pattern) = item.as_str() else {
            continue;
        };
        let joined = format!("{}{}", prefix, pattern.trim_end_matches('/'));
        if pattern.contains('*') || pattern.contains('?') {
            match globset::Glob::new(&joined) {
                Ok(glob) => {
                    builder.add(glob);
                }
                Err(_) => notes.push(format!("unreadable member pattern `{pattern}`")),
            }
        } else {
            listed.literal.push(joined);
        }
    }
    listed.globs = builder
        .build()
        .unwrap_or_else(|_| globset::GlobSet::empty());
    listed
}

fn prefix_of(directory: &str) -> String {
    if directory.is_empty() {
        String::new()
    } else {
        format!("{directory}/")
    }
}

fn string_list(value: Option<&toml::Value>) -> Vec<String> {
    match value {
        Some(toml::Value::Array(items)) => items
            .iter()
            .filter_map(|item| item.as_str())
            .map(ToOwned::to_owned)
            .collect(),
        _ => Vec::new(),
    }
}

/// A Node project, from its `package.json` and the lockfile that says which
/// runner the repository actually uses.
fn node(index: &Index, project: &mut Project) -> bool {
    let Some(primary) = nearest(index, &[NODE_MANIFEST]) else {
        return false;
    };
    let directory = primary.directory().to_owned();
    let Some(text) = index.read(&primary.path) else {
        project
            .notes
            .push(format!("{} could not be read", primary.path));
        return true;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        project.notes.push(format!(
            "{} did not parse as JSON, so no scripts were detected",
            primary.path
        ));
        return true;
    };
    let runner = package_runner(index).to_owned();
    let mut offered = 0usize;
    if let Some(scripts) = value.get("scripts").and_then(|s| s.as_object()) {
        for (name, _) in scripts {
            let Some(purpose) = Purpose::from_name(name) else {
                continue;
            };
            offered += 1;
            project.suggestions.push(suggestion(
                purpose,
                Ecosystem::Node,
                directory.clone(),
                vec![runner.clone(), "run".to_owned(), name.clone()],
                format!("{} has a script called \"{name}\"", primary.path),
            ));
        }
    }
    if offered == 0 {
        project.notes.push(format!(
            "{} has no script the agent would run",
            primary.path
        ));
    }
    true
}

/// The lockfile is the honest signal: a repository with `pnpm-lock.yaml` was
/// installed with pnpm, and running npm in it would use a different tree.
fn package_runner(index: &Index) -> &'static str {
    if index.contains("pnpm-lock.yaml") {
        "pnpm"
    } else if index.contains("bun.lockb") || index.contains("bun.lock") {
        "bun"
    } else if index.contains("yarn.lock") {
        "yarn"
    } else {
        "npm"
    }
}

/// A Python project, from `pyproject.toml`, `setup.py` and the tests on disk.
fn python(index: &Index, project: &mut Project) -> bool {
    let Some(primary) = nearest(index, &PYTHON_MANIFESTS) else {
        return false;
    };
    let directory = primary.directory().to_owned();
    let value = if primary.file_name() == "pyproject.toml" {
        match index
            .read(&primary.path)
            .and_then(|text| text.parse::<toml::Value>().ok())
        {
            Some(value) => Some(value),
            None => {
                project.notes.push(format!(
                    "{} did not parse as TOML, so only files on disk were used",
                    primary.path
                ));
                None
            }
        }
    } else {
        None
    };
    let configured = |name: &str| {
        value
            .as_ref()
            .and_then(|value| value.get("tool"))
            .and_then(|tool| tool.get(name))
            .is_some()
    };
    let declared = |name: &str| -> bool {
        let Some(dependencies) = value
            .as_ref()
            .and_then(|value| value.get("project"))
            .and_then(|project| project.get("dependencies"))
        else {
            return false;
        };
        dependency_names(Some(dependencies))
            .iter()
            .any(|have| have == name)
    };
    let test_files = index.files.iter().any(|entry| {
        let name = entry.file_name();
        name.starts_with("test_") || name.ends_with("_test.py")
    });
    if configured("pytest") || test_files || declared("pytest") {
        let argv = if index.contains("uv.lock") {
            vec!["uv".to_owned(), "run".to_owned(), "pytest".to_owned()]
        } else {
            vec!["python".to_owned(), "-m".to_owned(), "pytest".to_owned()]
        };
        project.suggestions.push(suggestion(
            Purpose::Validate,
            Ecosystem::Python,
            directory.clone(),
            argv,
            format!(
                "{} and test files here say the suite is pytest",
                primary.path
            ),
        ));
    }
    if configured("mypy") {
        project.suggestions.push(suggestion(
            Purpose::Check,
            Ecosystem::Python,
            directory.clone(),
            words(&["mypy", "."]),
            format!("{} configures mypy", primary.path),
        ));
    }
    if configured("ruff") {
        project.suggestions.push(suggestion(
            Purpose::Lint,
            Ecosystem::Python,
            directory.clone(),
            words(&["ruff", "check", "."]),
            format!("{} configures ruff", primary.path),
        ));
        project.suggestions.push(suggestion(
            Purpose::Format,
            Ecosystem::Python,
            directory,
            words(&["ruff", "format", "."]),
            format!("{} configures ruff", primary.path),
        ));
    } else if configured("black") {
        project.suggestions.push(suggestion(
            Purpose::Format,
            Ecosystem::Python,
            directory,
            words(&["black", "."]),
            format!("{} configures black", primary.path),
        ));
    }
    true
}

/// The names a dependency list spells, stripped of their version constraints.
fn dependency_names(value: Option<&toml::Value>) -> Vec<String> {
    match value {
        Some(toml::Value::Array(items)) => items
            .iter()
            .filter_map(|item| item.as_str())
            .map(dependency_name)
            .collect(),
        Some(toml::Value::Table(map)) => map
            .values()
            .filter_map(|value| value.as_array())
            .flatten()
            .filter_map(|item| item.as_str())
            .map(dependency_name)
            .collect(),
        _ => Vec::new(),
    }
}

fn dependency_name(spec: &str) -> String {
    spec.split(['=', '<', '>', '!', '~', ' ', ';', '['])
        .next()
        .unwrap_or(spec)
        .trim()
        .to_owned()
}

/// A Makefile, whose target names are the only contract it offers.
fn make(index: &Index, project: &mut Project) -> bool {
    let Some(primary) = nearest(index, &MAKE_MANIFESTS) else {
        return false;
    };
    let directory = primary.directory().to_owned();
    let Some(text) = index.read(&primary.path) else {
        project
            .notes
            .push(format!("{} could not be read", primary.path));
        return true;
    };
    let targets = make_targets(&text);
    let mut offered = 0usize;
    for target in &targets {
        let Some(purpose) = Purpose::from_name(target) else {
            continue;
        };
        offered += 1;
        project.suggestions.push(suggestion(
            purpose,
            Ecosystem::Make,
            directory.clone(),
            vec!["make".to_owned(), target.clone()],
            format!("{} has a {target} target", primary.path),
        ));
    }
    if offered == 0 && !targets.is_empty() {
        project.notes.push(format!(
            "{} has targets ({}), none of them named test, lint, format or build",
            primary.path,
            targets.join(", ")
        ));
    }
    true
}

/// The rules a Makefile declares: the lines that begin in column zero with a
/// name and a colon. Comments, variable assignments, recipe lines, pattern
/// rules and `.PHONY` are not things the agent should run.
fn make_targets(text: &str) -> Vec<String> {
    let mut targets: Vec<String> = Vec::new();
    for line in text.lines() {
        if line.starts_with([' ', '\t', '#', '!', '-', '@', '+', '%']) || line.contains(".PHONY:") {
            continue;
        }
        let Some((name, _)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim();
        if name.is_empty()
            || name.contains('(')
            || name.contains('%')
            || name.contains('$')
            || name.contains('=')
            || name.contains(':')
            || targets.iter().any(|known| known == name)
        {
            continue;
        }
        targets.push(name.to_owned());
    }
    targets
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn write(dir: &Path, path: &str, contents: &str) {
        let full = dir.join(path);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).expect("a writable parent");
        }
        std::fs::write(full, contents).expect("writable");
    }

    fn project_at(dir: &Path) -> Project {
        Project::detect(&Index::walk(dir))
    }

    #[test]
    fn a_cargo_workspace_lists_its_members_and_its_commands() {
        let dir = tempfile::tempdir().expect("a temp repo");
        write(
            dir.path(),
            "Cargo.toml",
            "[workspace]\nresolver = \"2\"\nmembers = [\"crates/*\"]\n",
        );
        write(
            dir.path(),
            "crates/one/Cargo.toml",
            "[package]\nname = \"one\"\n",
        );
        write(dir.path(), "crates/one/src/lib.rs", "pub fn one() {}");
        write(dir.path(), "crates/one/tests/one.rs", "fn t() {}");
        write(
            dir.path(),
            "crates/two/Cargo.toml",
            "[package]\nname = \"two\"\n",
        );
        write(dir.path(), "crates/two/src/main.rs", "fn main() {}");
        write(dir.path(), "crates/three/README.md", "no manifest here");
        let project = project_at(dir.path());
        assert_eq!(project.ecosystem, Ecosystem::Cargo);
        assert!(project.is_workspace(), "{:?}", project.crates);
        let names: Vec<&str> = project.crates.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["one", "two"], "no manifest, no crate");
        let one = project.crate_for("crates/one/src/lib.rs").expect("one");
        assert!(
            one.has_library && !one.has_binary && one.has_tests,
            "{one:?}"
        );
        let two = project.crate_for("crates/two/src/main.rs").expect("two");
        assert!(two.has_binary && !two.has_tests, "{two:?}");
        assert_eq!(
            project.crate_for("README.md"),
            None,
            "a root file is in no crate"
        );
        assert_eq!(one.validate_argv(), vec!["cargo", "test", "-p", "one"]);
        let validate = project.validate().expect("a command");
        assert_eq!(validate.argv, vec!["cargo", "test", "--workspace"]);
        assert!(validate.at_root(), "{validate:?}");
        assert!(validate.reason.contains("workspace"), "{}", validate.reason);
        assert_eq!(
            project.for_purpose(Purpose::Lint).expect("lint").argv,
            vec![
                "cargo",
                "clippy",
                "--workspace",
                "--all-targets",
                "--",
                "-D",
                "warnings"
            ]
        );
        assert_eq!(
            project.for_purpose(Purpose::Check).expect("check").argv,
            vec!["cargo", "check", "--workspace"],
            "a check is the same shape as a build and quicker than either"
        );
        assert_eq!(
            project.for_purpose(Purpose::Format).expect("fmt").argv,
            vec!["cargo", "fmt", "--all"]
        );
        let folder = dir
            .path()
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        assert_eq!(
            project.name, folder,
            "a virtual workspace borrows the folder's name"
        );
        assert!(
            project.describe().contains("2 crates"),
            "{}",
            project.describe()
        );
    }

    #[test]
    fn a_single_package_is_named_and_is_not_told_it_is_a_workspace() {
        let dir = tempfile::tempdir().expect("a temp repo");
        write(dir.path(), "Cargo.toml", "[package]\nname = \"solo\"\n");
        write(dir.path(), "src/lib.rs", "pub fn solo() {}");
        let project = project_at(dir.path());
        assert_eq!(project.crates.len(), 1, "{:?}", project.crates);
        assert_eq!(project.name, "solo");
        assert_eq!(project.ecosystem, Ecosystem::Cargo);
        assert!(!project.is_workspace());
        assert_eq!(
            project.validate().expect("a command").argv,
            vec!["cargo", "test"]
        );
        assert_eq!(
            project.describe(),
            format!("solo (cargo) — validate: cargo test")
        );
    }

    #[test]
    fn an_excluded_member_is_not_a_crate() {
        let dir = tempfile::tempdir().expect("a temp repo");
        write(
            dir.path(),
            "Cargo.toml",
            "[workspace]\nmembers = [\"crates/*\"]\nexclude = [\"crates/dropped\"]\n",
        );
        write(
            dir.path(),
            "crates/kept/Cargo.toml",
            "[package]\nname = \"kept\"\n",
        );
        write(
            dir.path(),
            "crates/dropped/Cargo.toml",
            "[package]\nname = \"dropped\"\n",
        );
        let project = project_at(dir.path());
        assert_eq!(project.crates.len(), 1, "{:?}", project.crates);
        assert_eq!(project.crates[0].name, "kept");
    }

    #[test]
    fn a_manifest_that_does_not_parse_is_reported_not_guessed() {
        let dir = tempfile::tempdir().expect("a temp repo");
        write(dir.path(), "Cargo.toml", "[package\nname = broken ");
        let project = project_at(dir.path());
        assert_eq!(
            project.ecosystem,
            Ecosystem::Cargo,
            "the file says what this is"
        );
        assert!(
            project
                .notes
                .iter()
                .any(|note| note.contains("did not parse")),
            "{:?}",
            project.notes
        );
        assert!(project.validate().is_none(), "there is no command to trust");
        assert!(project.crates.is_empty());
    }

    #[test]
    fn a_workspace_that_names_no_package_says_so() {
        let dir = tempfile::tempdir().expect("a temp repo");
        write(
            dir.path(),
            "Cargo.toml",
            "[workspace]\nmembers = [\"crates/*\"]\n",
        );
        write(
            dir.path(),
            "crates/gone/README.md",
            "the crate was moved away",
        );
        let project = project_at(dir.path());
        assert!(project.suggestions.is_empty(), "{:?}", project.suggestions);
        assert!(
            project.notes.iter().any(|note| note.contains("no package")),
            "{:?}",
            project.notes
        );
    }

    #[test]
    fn a_nested_manifest_runs_its_commands_from_its_own_directory() {
        let dir = tempfile::tempdir().expect("a temp repo");
        write(
            dir.path(),
            "tools/helper/Cargo.toml",
            "[package]\nname = \"helper\"\n",
        );
        write(dir.path(), "tools/helper/src/main.rs", "fn main() {}");
        write(dir.path(), "README.md", "not a build");
        let project = project_at(dir.path());
        let validate = project.validate().expect("a command");
        assert_eq!(validate.directory, "tools/helper", "{validate:?}");
        assert!(!validate.at_root());
        assert_eq!(validate.display(), "cargo test");
        assert_eq!(project.crates[0].name, "helper");
    }

    #[test]
    fn a_node_project_offers_its_scripts_and_uses_the_lockfile_runner() {
        let dir = tempfile::tempdir().expect("a temp repo");
        write(
            dir.path(),
            "package.json",
            r#"{"name":"web","scripts":{"test":"vitest","lint":"eslint .","deploy":"sh ./x.sh","nope":"echo hi"}}"#,
        );
        write(dir.path(), "pnpm-lock.yaml", "lockfileVersion: 9\n");
        let project = project_at(dir.path());
        assert_eq!(project.ecosystem, Ecosystem::Node);
        assert_eq!(project.crates.len(), 0);
        let argv: Vec<String> = project
            .suggestions
            .iter()
            .map(Suggestion::display)
            .collect();
        assert_eq!(argv, vec!["pnpm run test", "pnpm run lint"], "{argv:?}");
        let validate = project.validate().expect("a command");
        assert_eq!(validate.argv, vec!["pnpm", "run", "test"]);
        assert!(validate.reason.contains("\"test\""), "{}", validate.reason);
    }

    #[test]
    fn a_script_that_is_not_a_check_is_not_a_command() {
        let dir = tempfile::tempdir().expect("a temp repo");
        write(
            dir.path(),
            "package.json",
            r#"{"name":"web","scripts":{"start":"vite"}}"#,
        );
        let project = project_at(dir.path());
        assert!(project.suggestions.is_empty());
        assert!(
            project.notes.iter().any(|note| note.contains("no script")),
            "{:?}",
            project.notes
        );
        assert!(
            project.describe().ends_with("no validate command found"),
            "{}",
            project.describe()
        );
    }

    #[test]
    fn a_python_project_names_its_test_tool_and_linter() {
        let dir = tempfile::tempdir().expect("a temp repo");
        write(
            dir.path(),
            "pyproject.toml",
            "[project]\nname = \"tool\"\ndependencies = [\"requests>=2\", \"pytest>=7\"]\n[tool.ruff]\nline-length = 100\n",
        );
        write(dir.path(), "uv.lock", "");
        let project = project_at(dir.path());
        assert_eq!(project.ecosystem, Ecosystem::Python);
        assert_eq!(
            project.validate().expect("a command").argv,
            vec!["uv", "run", "pytest"],
            "the lockfile names the runner"
        );
        assert_eq!(
            project.for_purpose(Purpose::Lint).expect("lint").argv,
            vec!["ruff", "check", "."]
        );
        assert_eq!(
            project.for_purpose(Purpose::Format).expect("format").argv,
            vec!["ruff", "format", "."]
        );
    }

    #[test]
    fn test_files_on_disk_are_enough_to_call_a_suite_pytest() {
        let dir = tempfile::tempdir().expect("a temp repo");
        write(
            dir.path(),
            "setup.py",
            "from setuptools import setup\nsetup()\n",
        );
        write(
            dir.path(),
            "tests/test_thing.py",
            "def test_thing():\n    assert True\n",
        );
        let project = project_at(dir.path());
        assert_eq!(project.ecosystem, Ecosystem::Python);
        assert_eq!(
            project.validate().expect("a command").argv,
            vec!["python", "-m", "pytest"]
        );
    }

    #[test]
    fn a_project_only_has_a_check_command_when_it_declares_one() {
        // A check answers "does it compile", which is a different question from
        // "does the suite pass", so it is only offered where the manifests say
        // how: cargo has the subcommand, a Python project has to configure mypy,
        // and a script named `check` is the suite rather than a type check.
        let dir = tempfile::tempdir().expect("a temp repo");
        write(
            dir.path(),
            "pyproject.toml",
            "[project]\nname = \"app\"\n\n[tool.mypy]\nstrict = true\n",
        );
        let project = project_at(dir.path());
        assert_eq!(
            project.for_purpose(Purpose::Check).expect("mypy").argv,
            vec!["mypy", "."]
        );

        let dir = tempfile::tempdir().expect("a temp repo");
        write(
            dir.path(),
            "package.json",
            r#"{"name":"web","scripts":{"check":"node ./run-tests.js"}}"#,
        );
        let project = project_at(dir.path());
        assert!(
            project.for_purpose(Purpose::Check).is_none(),
            "{:?}",
            project.suggestions
        );
        assert_eq!(
            project.validate().expect("the suite").argv,
            vec!["npm", "run", "check"]
        );
    }

    #[test]
    fn a_makefile_offers_only_the_targets_it_declares() {
        let dir = tempfile::tempdir().expect("a temp repo");
        write(
            dir.path(),
            "Makefile",
            "# a makefile\nCC = gcc\nall: build\n.PHONY: test\ntest:\n\tcargo test\nlint:\n\ttrue\n%.o: %.c\n\t$(CC) -c $<\ndeploy:\n\trm -rf /\n",
        );
        let project = project_at(dir.path());
        assert_eq!(project.ecosystem, Ecosystem::Make);
        let argv: Vec<String> = project
            .suggestions
            .iter()
            .map(Suggestion::display)
            .collect();
        assert_eq!(argv, vec!["make test", "make lint"], "{argv:?}");
        assert!(project.notes.is_empty(), "{:?}", project.notes);
    }

    #[test]
    fn a_makefile_with_nothing_useful_says_what_it_has() {
        let dir = tempfile::tempdir().expect("a temp repo");
        write(
            dir.path(),
            "Makefile",
            "all:\n\ttrue\nclean:\n\trm -f out\n",
        );
        let project = project_at(dir.path());
        assert!(project.suggestions.is_empty());
        assert!(
            project.notes.iter().any(|note| note.contains("all, clean")),
            "{:?}",
            project.notes
        );
    }

    #[test]
    fn a_rust_workspace_with_a_frontend_still_answers_cargo() {
        let dir = tempfile::tempdir().expect("a temp repo");
        write(dir.path(), "Cargo.toml", "[package]\nname = \"app\"\n");
        write(dir.path(), "src/main.rs", "fn main() {}");
        write(
            dir.path(),
            "web/package.json",
            r#"{"name":"web","scripts":{"test":"vitest"}}"#,
        );
        let project = project_at(dir.path());
        assert_eq!(project.ecosystem, Ecosystem::Cargo);
        assert_eq!(project.name, "app");
        assert_eq!(
            project.validate().expect("a command").argv,
            vec!["cargo", "test"]
        );
        assert!(
            project
                .notes
                .iter()
                .any(|note| note.contains("cargo, node")),
            "{:?}",
            project.notes
        );
    }

    #[test]
    fn a_plain_directory_is_honest_about_not_being_a_project() {
        let dir = tempfile::tempdir().expect("a temp repo");
        write(dir.path(), "notes.md", "# notes\n");
        let index = Index::walk(dir.path());
        let project = Project::detect(&index);
        assert_eq!(project.ecosystem, Ecosystem::Unknown);
        assert!(project.suggestions.is_empty());
        assert!(project.crates.is_empty());
        assert_eq!(
            project.name,
            directory_name(&index),
            "the folder is all there is"
        );
        assert!(project.notes.is_empty(), "{:?}", project.notes);
        assert!(
            project.describe().ends_with("no validate command found"),
            "{}",
            project.describe()
        );
    }

    #[test]
    fn a_recipe_line_is_not_a_target() {
        assert_eq!(make_targets("all:\n\tmake other\n"), vec!["all"]);
        assert_eq!(make_targets("VERSION = 1\n\nfmt:\n"), vec!["fmt"]);
        assert_eq!(make_targets("# test:\ntest:\n"), vec!["test"]);
    }
}
