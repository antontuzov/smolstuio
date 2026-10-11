//! Running the project's own checks, as five separate tools.
//!
//! These are the five commands an agent wants at the end of a change: the tests,
//! the compile check, the linter, the build, and the question of whether the
//! formatting is what the project's formatter would leave. Each takes its
//! arguments from the manifests
//! on disk — [`agent_repo::Project`] reads them — and **not** from the model, so
//! `test_run` cannot be talked into `rm -rf target` and a run cannot check itself
//! with a command nobody declared. All four therefore take no arguments at all,
//! which is also the cheapest thing to offer in a prompt.
//!
//! Two consequences are worth stating plainly, because both are the tool saying
//! no rather than doing something surprising:
//!
//! - A project that declares no such command is **reported**, not guessed at.
//!   There is no fallback to `cargo test` in a repository with no manifest, and
//!   no inventing a command that looks plausible.
//! - `format_run` only **reports**. It asks the project's formatter which files
//!   it would change (`cargo fmt --check`, `prettier --check`, `black --check`,
//!   `ruff format --check`, `gofmt -l`) and rewrites nothing. A command this tool
//!   cannot put in report mode — a `npm run fmt` script, a `make fmt` target, an
//!   `eslint --fix` — is refused rather than run, because a rewrite is not what a
//!   check asks for and the difference is what makes the tool safe to call
//!   without a person watching it.
//!
//! Everything else about running a program — no shell, a directory inside the
//! workspace, a limit that kills it, output kept and capped — is
//! [`crate::shell::run_program`], shared with `run_command` so there is one
//! implementation of those rules.

use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::json;

use agent_repo::{Index, Project, Purpose};
use agent_sandbox::Permission;

use crate::call::{ToolCall, ToolOutput, ToolResult};
use crate::context::Context;
use crate::schema;
use crate::shell::{run_program, COMMAND_TIMEOUT_SECONDS};
use crate::tool::{Intent, Tool, ToolDefinition};

/// A lint run over a workspace is slower than a read and faster than a suite.
const LINT_TIMEOUT_SECONDS: u64 = 120;
/// A formatter reads every file it is pointed at, and that is all it does.
const FORMAT_TIMEOUT_SECONDS: u64 = 60;

/// One project command, as a tool the model can call.
pub struct ProjectCommand {
    name: &'static str,
    /// What to ask [`Project::for_purpose`] for, and the word a message uses.
    purpose: Purpose,
    phrase: &'static str,
    description: &'static str,
    timeout: Duration,
    /// Whether the command is only allowed to report what it would change.
    check_only: bool,
}

impl ProjectCommand {
    pub fn test_run() -> Self {
        Self {
            name: "test_run",
            purpose: Purpose::Validate,
            phrase: "test",
            description: "Run the test command this project declares in its own manifests and \
                          report its exit code and output. Takes no arguments: the command comes \
                          from the project, never from the caller. A failing test is an answer, \
                          not a broken tool.",
            timeout: Duration::from_secs(COMMAND_TIMEOUT_SECONDS),
            check_only: false,
        }
    }

    pub fn check_run() -> Self {
        Self {
            name: "check_run",
            purpose: Purpose::Check,
            phrase: "check",
            description: "Compile and type-check this project with the check command its own \
                          manifests declare, without running its tests. Takes no arguments. \
                          Quicker than `build_run` or `test_run`, and it is the one to ask after \
                          an edit that might not build.",
            timeout: Duration::from_secs(COMMAND_TIMEOUT_SECONDS),
            check_only: false,
        }
    }

    pub fn lint_run() -> Self {
        Self {
            name: "lint_run",
            purpose: Purpose::Lint,
            phrase: "lint",
            description: "Run the lint or type-check command this project declares in its own \
                          manifests and report what it finds. Takes no arguments. A project with \
                          no declared lint command is reported rather than guessed at.",
            timeout: Duration::from_secs(LINT_TIMEOUT_SECONDS),
            check_only: false,
        }
    }

    pub fn format_run() -> Self {
        Self {
            name: "format_run",
            purpose: Purpose::Format,
            phrase: "format",
            description: "Ask this project's formatter which files it would change, in report \
                          mode only: nothing is rewritten. Takes no arguments. A formatter that \
                          cannot be put in report mode is refused rather than run.",
            timeout: Duration::from_secs(FORMAT_TIMEOUT_SECONDS),
            check_only: true,
        }
    }

    pub fn build_run() -> Self {
        Self {
            name: "build_run",
            purpose: Purpose::Build,
            phrase: "build",
            description: "Build the project with the build command it declares in its own \
                          manifests and report its exit code and output. Takes no arguments, and \
                          nothing is uploaded or published: a build only writes build output.",
            timeout: Duration::from_secs(COMMAND_TIMEOUT_SECONDS),
            check_only: false,
        }
    }

    /// The command to run and the directory to run it in, read from the tree.
    ///
    /// `Err` is the tool's honest answer to a call it cannot carry out: nothing
    /// here declares the command, or the command it declares cannot be run in
    /// the mode this tool is limited to. No process is started either way.
    fn plan(&self, ctx: &Context) -> Result<(Vec<String>, PathBuf), String> {
        let project = Project::detect(&Index::walk(&ctx.root));
        let Some(suggestion) = project.for_purpose(self.purpose) else {
            return Err(format!(
                "{}: no {} command is declared in this project's manifests, and this tool will \
                 not invent one. `run_command` is the tool for a command you know.",
                self.name, self.phrase
            ));
        };
        let mut argv = suggestion.argv.clone();
        if self.check_only {
            argv = report_mode(&suggestion.argv)?;
        }
        let directory = if suggestion.at_root() {
            ctx.root.clone()
        } else {
            ctx.resolve(&suggestion.directory)?
        };
        Ok((argv, directory))
    }
}

#[async_trait]
impl Tool for ProjectCommand {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            self.name,
            self.description,
            // No arguments, because the arguments are the project's business.
            schema::object(json!({}), &[]),
            Permission::Execute,
        )
        .with_timeout(self.timeout)
    }

    fn intent(&self, _call: &ToolCall, ctx: &Context) -> Result<Intent, String> {
        let (argv, directory) = self.plan(ctx)?;
        let mut intent = Intent::running(argv);
        // The directory is part of the intent so the workspace rule and a
        // denylist of paths have something to hold on to, as they do for
        // `run_command`.
        intent.paths.push(directory);
        Ok(intent)
    }

    async fn run(&self, call: &ToolCall, ctx: &Context) -> ToolResult {
        let (argv, directory) = match self.plan(ctx) {
            Ok(plan) => plan,
            Err(problem) => {
                return ToolResult::failed(
                    call.clone(),
                    Permission::Execute,
                    problem,
                    ToolOutput::empty(),
                )
            }
        };
        let limit = ctx.policy.timeout_for(self.name, self.timeout);
        run_program(call, &argv, &directory, limit).await
    }
}

/// The same command asked to report rather than rewrite.
///
/// A formatter's own flag is inserted directly after its name — before any path,
/// because `gofmt` and friends stop reading flags at the first file argument.
fn report_mode(argv: &[String]) -> Result<Vec<String>, String> {
    // A command already told to rewrite is not made a check by adding a flag to
    // it: the fix stays in, and the tool would be lying about what it runs.
    if let Some(fix) = argv
        .iter()
        .find(|argument| matches!(argument.as_str(), "--fix" | "--write" | "-w"))
    {
        return Err(cannot(argv, &format!("`{fix}` rewrites files")));
    }

    // A runner that executes a named program forwards its arguments, so the
    // formatter is the word after it; a runner that runs a *script* is a promise
    // about a file this tool does not read.
    let mut offset = 0;
    if let Some(runner) = argv.first().map(String::as_str) {
        if EXEC_RUNNERS.contains(&runner) {
            match argv.get(1).map(String::as_str) {
                Some("exec" | "x" | "dlx") => offset = 2,
                Some("run") | None => {
                    return Err(cannot(
                        argv,
                        "what the script itself runs is not visible here",
                    ))
                }
                _ => offset = 1,
            }
        }
    }
    let program = argv
        .get(offset)
        .map(|word| {
            std::path::Path::new(word)
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| word.clone())
        })
        .unwrap_or_default();
    let after = offset + 1;
    let sub = argv.get(after).map(String::as_str).unwrap_or_default();

    // (flag, whether the tool has one word of its own before it)
    let (flag, place) = match (program.as_str(), sub) {
        ("cargo", "fmt") | ("ruff", "format") => ("--check", after + 1),
        ("rustfmt", _) | ("prettier", _) | ("black", _) => ("--check", after),
        ("gofmt", _) => ("-l", after),
        // These report as they stand, so the command is run exactly as declared.
        ("eslint", _) | ("biome", _) | ("ruff", "check") => return Ok(argv.to_vec()),
        _ => return Err(cannot(argv, "no report mode is known for it")),
    };
    if argv.iter().any(|argument| argument == flag) {
        return Ok(argv.to_vec());
    }
    let mut checked = argv.to_vec();
    checked.insert(place.min(checked.len()), flag.to_owned());
    Ok(checked)
}

const EXEC_RUNNERS: [&str; 5] = ["npx", "pnpm", "yarn", "bun", "npm"];

fn cannot(argv: &[String], why: &str) -> String {
    format!(
        "format_run only reports, and `{}` cannot be put in report mode: {}. Run it through \
         `run_command` if a rewrite is what the task needs, and read what it changed with \
         `git_diff`.",
        argv.join(" "),
        why
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Registry;
    use agent_config::{SandboxMode, ToolPolicy};
    use agent_sandbox::Policy;
    use serde_json::json;

    fn cargo_project(dir: &std::path::Path) {
        std::fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"tiny\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .expect("writable");
        std::fs::create_dir_all(dir.join("src")).expect("writable");
        std::fs::write(dir.join("src/lib.rs"), "pub fn tiny() {}\n").expect("writable");
    }

    fn npm_project(dir: &std::path::Path, scripts: &str) {
        std::fs::write(
            dir.join("package.json"),
            format!("{{\"name\": \"tiny\", \"scripts\": {{{scripts}}}}}\n"),
        )
        .expect("writable");
    }

    fn context(dir: &std::path::Path) -> Context {
        Context::new(dir.to_path_buf())
    }

    fn argv_of(tool: ProjectCommand, dir: &std::path::Path) -> Vec<String> {
        let call = ToolCall::new(tool.name, json!({}));
        tool.intent(&call, &context(dir))
            .expect("a command")
            .command
            .expect("an argv")
    }

    #[test]
    fn the_command_comes_from_the_manifest_and_not_from_the_call() {
        let dir = tempfile::tempdir().expect("a temp repo");
        cargo_project(dir.path());
        // The model asks for a deletion; the tool runs what the project calls its
        // tests, because that is the only thing this tool is.
        let call = ToolCall::new("test_run", json!({"argv": ["rm", "-rf", "."]}));
        let intent = ProjectCommand::test_run()
            .intent(&call, &context(dir.path()))
            .expect("declared");
        assert_eq!(intent.command, Some(vec!["cargo".into(), "test".into()]));
        assert_eq!(intent.paths, vec![dir.path().to_path_buf()]);
        assert_eq!(
            intent.permission(Permission::Execute),
            Permission::Execute,
            "a test run is a command, and no more than one"
        );
    }

    #[test]
    fn a_project_that_declares_nothing_is_reported_instead_of_guessed_at() {
        let dir = tempfile::tempdir().expect("a temp repo");
        std::fs::write(dir.path().join("README.md"), "notes\n").expect("writable");
        for tool in [
            ProjectCommand::test_run(),
            ProjectCommand::check_run(),
            ProjectCommand::lint_run(),
            ProjectCommand::format_run(),
            ProjectCommand::build_run(),
        ] {
            let call = ToolCall::new(tool.name, json!({}));
            let problem = tool
                .intent(&call, &context(dir.path()))
                .expect_err("nothing to run");
            assert!(problem.contains(tool.name), "{problem}");
            assert!(
                problem.contains("run_command"),
                "{problem} should say what to use instead"
            );
        }
    }

    #[test]
    fn the_command_each_tool_runs_is_the_one_declared_for_its_purpose() {
        let dir = tempfile::tempdir().expect("a temp repo");
        cargo_project(dir.path());
        assert_eq!(
            argv_of(ProjectCommand::lint_run(), dir.path()),
            vec!["cargo", "clippy", "--all-targets", "--", "-D", "warnings"]
        );
        assert_eq!(
            argv_of(ProjectCommand::check_run(), dir.path()),
            vec!["cargo", "check"],
            "the compile check, not the suite"
        );
        assert_eq!(
            argv_of(ProjectCommand::build_run(), dir.path()),
            vec!["cargo", "build"]
        );
        assert_eq!(
            argv_of(ProjectCommand::format_run(), dir.path()),
            vec!["cargo", "fmt", "--check"],
            "the check flag goes before any path"
        );
    }

    #[test]
    fn a_formatter_that_cannot_be_asked_to_report_is_refused_rather_than_run() {
        // A script is a promise about a file this tool does not read.
        let dir = tempfile::tempdir().expect("a temp repo");
        npm_project(dir.path(), "\"fmt\": \"prettier .\"");
        let problem = ProjectCommand::format_run()
            .intent(
                &ToolCall::new("format_run", json!({})),
                &context(dir.path()),
            )
            .expect_err("a script may rewrite");
        assert!(problem.contains("only reports"), "{problem}");
        assert!(problem.contains("run_command"), "{problem}");
        // So is a command that says rewrite in its own arguments.
        assert!(report_mode(&words(&["prettier", "--write"]))
            .expect_err("--write stays in")
            .contains("rewrites files"));
    }

    #[test]
    fn report_mode_names_the_flag_each_formatter_answers_to() {
        let cases = [
            (vec!["cargo", "fmt"], vec!["cargo", "fmt", "--check"]),
            (
                vec!["cargo", "fmt", "--all"],
                vec!["cargo", "fmt", "--check", "--all"],
            ),
            (
                vec!["prettier", "--check", "."],
                vec!["prettier", "--check", "."],
            ),
            (vec!["black", "."], vec!["black", "--check", "."]),
            (
                vec!["ruff", "format", "."],
                vec!["ruff", "format", "--check", "."],
            ),
            (vec!["ruff", "check", "."], vec!["ruff", "check", "."]),
            (vec!["eslint", "src"], vec!["eslint", "src"]),
            (vec!["gofmt", "."], vec!["gofmt", "-l", "."]),
            (
                vec!["npx", "prettier", "."],
                vec!["npx", "prettier", "--check", "."],
            ),
            (
                vec!["pnpm", "exec", "prettier", "."],
                vec!["pnpm", "exec", "prettier", "--check", "."],
            ),
        ];
        for (declared, expected) in cases {
            let got = report_mode(&words(&declared)).expect("reportable");
            assert_eq!(got.join(" "), expected.join(" "), "from {declared:?}");
        }
    }

    fn words(argv: &[&str]) -> Vec<String> {
        argv.iter().map(|word| (*word).to_owned()).collect()
    }

    #[tokio::test]
    async fn a_denied_command_stops_the_process_from_starting() {
        let dir = tempfile::tempdir().expect("a temp repo");
        cargo_project(dir.path());
        let mut config = agent_config::Config::default();
        config.tools.insert(
            "test_run".to_owned(),
            ToolPolicy {
                denylist: vec!["cargo test".to_owned()],
                sandbox: Some(SandboxMode::Strict),
                ..ToolPolicy::default()
            },
        );
        let ctx = context(dir.path()).with_policy(Policy::from_config(dir.path(), &config));
        let result = Registry::with_defaults()
            .execute(&ToolCall::new("test_run", json!({})), &ctx)
            .await;
        assert!(
            matches!(result.status, crate::ToolStatus::Refused { .. }),
            "{}",
            result.summary()
        );
        assert!(
            result.summary().contains("denylist"),
            "{}",
            result.summary()
        );
    }

    #[tokio::test]
    async fn a_project_with_no_declared_command_never_starts_a_process() {
        let dir = tempfile::tempdir().expect("a temp repo");
        let result = Registry::with_defaults()
            .execute(&ToolCall::new("build_run", json!({})), &context(dir.path()))
            .await;
        assert!(!result.is_success(), "{}", result.summary());
        assert!(
            result.summary().contains("no build command"),
            "{}",
            result.summary()
        );
    }

    #[tokio::test]
    async fn a_command_the_project_declares_really_runs_and_its_output_comes_back() {
        if which("make").is_none() {
            return;
        }
        let dir = tempfile::tempdir().expect("a temp repo");
        std::fs::write(
            dir.path().join("Makefile"),
            "test:\n\t@echo made-by-the-project\n",
        )
        .expect("writable");
        let result = ProjectCommand::test_run()
            .run(&ToolCall::new("test_run", json!({})), &context(dir.path()))
            .await;
        assert!(result.is_success(), "{}", result.summary());
        assert!(
            result.output.text.contains("made-by-the-project"),
            "{}",
            result.output.text
        );
    }

    #[tokio::test]
    async fn a_check_compiles_the_project_and_reports_what_does_not_build() {
        if which("cargo").is_none() {
            return;
        }
        let dir = tempfile::tempdir().expect("a temp repo");
        cargo_project(dir.path());
        let call = ToolCall::new("check_run", json!({}));
        let clean = ProjectCommand::check_run()
            .run(&call, &context(dir.path()))
            .await;
        assert!(clean.is_success(), "{}", clean.summary());

        std::fs::write(
            dir.path().join("src/lib.rs"),
            "pub fn oops() {\n    does_not_exist()\n}\n",
        )
        .expect("writable");
        let result = ProjectCommand::check_run()
            .run(&call, &context(dir.path()))
            .await;
        assert!(!result.is_success(), "{}", result.summary());
        assert!(
            result.output.text.contains("does_not_exist"),
            "the compiler's own words rather than a generic failure: {}",
            result.output.text
        );
    }

    #[test]
    fn the_project_commands_are_all_execute_and_none_of_them_is_offered_an_argument() {
        for tool in [
            ProjectCommand::test_run(),
            ProjectCommand::check_run(),
            ProjectCommand::lint_run(),
            ProjectCommand::format_run(),
            ProjectCommand::build_run(),
        ] {
            let definition = tool.definition();
            assert_eq!(definition.permission, Permission::Execute);
            assert_eq!(definition.timeout, tool.timeout);
            assert_eq!(
                definition.parameters["required"].as_array().map(Vec::len),
                Some(0),
                "{} takes no arguments",
                definition.name
            );
            assert_eq!(
                definition.parameters["properties"]
                    .as_object()
                    .map(|properties| properties.len()),
                Some(0),
                "and says so"
            );
            assert!(
                definition.description.contains("Takes no arguments"),
                "{}",
                definition.name
            );
        }
        assert!(
            ProjectCommand::format_run()
                .definition()
                .description
                .contains("nothing is rewritten"),
            "the model is told the one tool that cannot change files"
        );
    }

    /// Where a program would be found, so a test that needs `make` can stand up
    /// rather than fail on a machine that has none.
    fn which(program: &str) -> Option<PathBuf> {
        let paths = std::env::var_os("PATH")?;
        std::env::split_paths(&paths)
            .map(|directory| directory.join(program))
            .find(|candidate| candidate.is_file())
    }
}
