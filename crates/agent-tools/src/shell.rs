//! Running one program, as the agent's hands.
//!
//! This is the only tool here that changes the machine outside a file write, so
//! it is the one the sandbox watches closest. Three rules do the work:
//!
//! - **the arguments arrive as a list.** No shell is started, so `>`, `|`, `&&`
//!   and `;` are not features a model can reach: they become literal arguments
//!   that the program rejects. A bare string is accepted too and split on
//!   whitespace, because a 1.5B model writes `"cargo test"` more reliably than
//!   it writes `["cargo", "test"]` — but the split never hands the string to a
//!   shell either.
//! - **the working directory is inside the workspace**, and so is a program
//!   named by path. A `./scripts/build.sh` two directories up is not this
//!   repository's build script.
//! - **the class comes from the argv**, not from this file. `classify` turns
//!   `rm build.rs` into [`Permission::Destructive`] whatever the tool declares,
//!   so the default approval mode asks before a deletion and not before a test
//!   run.
//!
//! The program's own timeout, output ceiling and audit entry are applied by
//! [`crate::Registry::execute`]; this tool only has to make sure a command it
//! gave up on is actually dead, which is what `kill_on_drop` is for.

use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::json;
use tokio::process::Command;

use agent_sandbox::Permission;

use crate::call::{ToolCall, ToolOutput, ToolResult};
use crate::context::Context;
use crate::schema;
use crate::tool::{Intent, Tool, ToolDefinition};

/// A test run is the most common call and it is not fast, so this tool's own
/// default is longer than a read's. The configuration can still lower it.
pub const COMMAND_TIMEOUT_SECONDS: u64 = 300;

pub struct RunCommand;

#[async_trait]
impl Tool for RunCommand {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "run_command",
            "Run one program with these arguments; its exit code, stdout and stderr. There \
             is no shell: `&&`, `|`, `>` and `;` are literal text, so each argument is its \
             own item — [\"cargo\", \"test\"]. `path` runs it there instead of the root.",
            schema::object(
                json!({
                    "argv": schema::string_array("the program first, then each argument"),
                    "command": schema::string("a command line split on spaces; prefer argv"),
                    "path": schema::path("directory to run in; default the repository root"),
                }),
                &[],
            ),
            Permission::Execute,
        )
        .with_timeout(Duration::from_secs(COMMAND_TIMEOUT_SECONDS))
    }

    fn intent(&self, call: &ToolCall, ctx: &Context) -> Result<Intent, String> {
        let argv = argv_of(call)?;
        let mut intent = Intent::running(argv);
        let raw = call.str_arg("path").unwrap_or_else(|| ".".to_owned());
        intent.paths.push(ctx.resolve(&raw)?);
        // A program named by path is a file the call touches, so the workspace
        // rule applies to it as it applies to the directory it runs in.
        if let Some(program) = intent.command.as_ref().and_then(|argv| argv.first()) {
            if program.contains('/') || program.contains('\\') {
                intent.paths.push(ctx.resolve(program)?);
            }
        }
        Ok(intent)
    }

    async fn run(&self, call: &ToolCall, ctx: &Context) -> ToolResult {
        let intent = match self.intent(call, ctx) {
            Ok(intent) => intent,
            Err(problem) => {
                return ToolResult::failed(
                    call.clone(),
                    Permission::Execute,
                    problem,
                    ToolOutput::empty(),
                )
            }
        };
        let Some(argv) = intent.command.clone() else {
            return ToolResult::failed(
                call.clone(),
                Permission::Execute,
                "run_command needs the program to run".to_owned(),
                ToolOutput::empty(),
            );
        };
        let cwd = intent
            .paths
            .first()
            .cloned()
            .unwrap_or_else(|| ctx.root.clone());
        let limit = ctx
            .policy
            .timeout_for("run_command", Duration::from_secs(COMMAND_TIMEOUT_SECONDS));
        run_program(call, &argv, &cwd, limit).await
    }
}

/// Run one program to its end and answer with what it said.
///
/// Shared with [`crate::validate`]'s project commands so that the rules a
/// program runs under — no shell, a directory inside the workspace, a limit that
/// kills it, stdout and stderr both kept, and a non-zero exit reported as the
/// *program's* answer rather than a broken tool — have one implementation.
pub(crate) async fn run_program(
    call: &ToolCall,
    argv: &[String],
    cwd: &std::path::Path,
    limit: Duration,
) -> ToolResult {
    let Some(program) = argv.first() else {
        return ToolResult::failed(
            call.clone(),
            Permission::Execute,
            "nothing was named to run".to_owned(),
            ToolOutput::empty(),
        );
    };
    let started = std::time::Instant::now();
    let output = Command::new(program)
        .args(&argv[1..])
        .current_dir(cwd)
        // git is the program most likely to ask a question, and a prompt
        // this agent cannot answer is a step wasted.
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn();
    let result = match output {
        Err(source) => {
            return ToolResult::failed(
                call.clone(),
                Permission::Execute,
                format!("cannot start {program}: {source}"),
                ToolOutput::empty(),
            )
        }
        Ok(child) => match tokio::time::timeout(limit, child.wait_with_output()).await {
            Ok(Ok(output)) => output,
            Ok(Err(source)) => {
                return ToolResult::failed(
                    call.clone(),
                    Permission::Execute,
                    format!("{program} failed to run: {source}"),
                    ToolOutput::empty(),
                )
            }
            Err(_) => {
                return ToolResult::failed(
                    call.clone(),
                    Permission::Execute,
                    format!(
                        "`{}` was stopped after {}s of not finishing",
                        argv.join(" "),
                        limit.as_secs()
                    ),
                    ToolOutput::new(
                        "the program was killed; a long build or a test that \
                                         waits on input is worth running outside the agent"
                            .to_owned(),
                    ),
                )
            }
        },
    };
    let duration_ms = started.elapsed().as_millis() as u64;
    let stdout = String::from_utf8_lossy(&result.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&result.stderr).into_owned();
    let mut text = String::new();
    if !stdout.trim().is_empty() {
        text.push_str("--- out ---\n");
        text.push_str(stdout.trim_end());
        text.push('\n');
    }
    if !stderr.trim().is_empty() {
        text.push_str("--- err ---\n");
        text.push_str(stderr.trim_end());
        text.push('\n');
    }
    if text.is_empty() {
        text = "(the program printed nothing)\n".to_owned();
    }
    let code = match result.status.code() {
        Some(code) => code.to_string(),
        None => "on a signal".to_owned(),
    };
    let message = format!("`{}` exited {code}", argv.join(" "));
    let output = ToolOutput::new(format!("[exit {code}]\n{text}"));
    if result.status.success() {
        ToolResult::completed(call.clone(), Permission::Execute, output, duration_ms)
    } else {
        // A failed test is not a broken tool: the output is the answer, and
        // the status says the thing it reports about did not work.
        ToolResult::failed(call.clone(), Permission::Execute, message, output)
    }
}

/// The command line the model meant, from either spelling it may have used.
fn argv_of(call: &ToolCall) -> Result<Vec<String>, String> {
    if let Some(list) = call.str_list_arg("argv") {
        if !list.is_empty() {
            return Ok(list);
        }
    }
    if let Some(line) = call.str_arg("command") {
        return split_words(&line);
    }
    Err("run_command needs \"argv\": the program first, then each argument".to_owned())
}

/// Split a command line on whitespace, honouring quotes, and without
/// interpreting anything else.
///
/// A backslash, a `$` or a `>` survives as literal text. That is the point: this
/// is a convenience for a model that spells a command as one string, not a
/// shell, and the difference is what the sandbox's guarantee rests on.
fn split_words(line: &str) -> Result<Vec<String>, String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut in_word = false;
    for character in line.chars() {
        match quote {
            Some(open) if character == open => {
                quote = None;
            }
            Some(_) => current.push(character),
            None => match character {
                '"' | '\'' => {
                    quote = Some(character);
                    in_word = true;
                }
                ' ' | '\t' | '\n' | '\r' => {
                    if in_word {
                        words.push(std::mem::take(&mut current));
                        in_word = false;
                    }
                }
                _ => {
                    current.push(character);
                    in_word = true;
                }
            },
        }
    }
    if quote.is_some() {
        return Err(format!(
            "the command `{line}` has a quote that never closes, so its arguments are unclear"
        ));
    }
    if in_word {
        words.push(current);
    }
    if words.is_empty() {
        return Err("run_command was given an empty command".to_owned());
    }
    Ok(words)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Registry;
    use agent_config::{ApprovalMode, SandboxMode};
    use agent_sandbox::Policy;
    use std::path::PathBuf;

    /// Where a program would be found, so a test that needs `/bin/echo` can
    /// stand up rather than fail on a machine that has no coreutils.
    fn which(program: &str) -> Option<PathBuf> {
        if program.contains('/') {
            let path = PathBuf::from(program);
            return path.is_file().then_some(path);
        }
        let paths = std::env::var_os("PATH")?;
        std::env::split_paths(&paths)
            .map(|directory| directory.join(program))
            .find(|candidate| candidate.is_file())
    }

    fn runs(program: &str) -> bool {
        which(program).is_some()
    }

    #[tokio::test]
    async fn a_program_that_succeeds_reports_what_it_printed() {
        if !runs("echo") {
            return;
        }
        let dir = tempfile::tempdir().expect("a temp dir");
        let ctx = Context::new(dir.path().to_path_buf());
        let call = ToolCall::new("run_command", json!({"argv": ["echo", "hello agent"]}));
        let result = RunCommand.run(&call, &ctx).await;
        assert!(result.is_success(), "{}", result.summary());
        assert!(
            result.output.text.contains("hello agent"),
            "{}",
            result.output.text
        );
        assert!(
            result.output.text.starts_with("[exit 0]"),
            "{}",
            result.output.text
        );
    }

    #[tokio::test]
    async fn a_program_that_fails_is_a_failure_with_its_own_output() {
        if !runs("false") {
            return;
        }
        let dir = tempfile::tempdir().expect("a temp dir");
        let ctx = Context::new(dir.path().to_path_buf());
        // `false` is the smallest program that exits non-zero.
        let call = ToolCall::new("run_command", json!({"argv": ["false"]}));
        let result = RunCommand.run(&call, &ctx).await;
        assert!(!result.is_success(), "{}", result.summary());
        assert!(result.summary().contains("exited"), "{}", result.summary());
    }

    #[tokio::test]
    async fn a_missing_program_says_which_one() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let ctx = Context::new(dir.path().to_path_buf());
        let call = ToolCall::new(
            "run_command",
            json!({"argv": ["this-program-does-not-exist"]} ),
        );
        let result = RunCommand.run(&call, &ctx).await;
        assert!(
            result.summary().contains("cannot start"),
            "{}",
            result.summary()
        );
    }

    #[tokio::test]
    async fn a_command_line_is_split_without_a_shell() {
        if !runs("echo") {
            return;
        }
        let dir = tempfile::tempdir().expect("a temp dir");
        let ctx = Context::new(dir.path().to_path_buf());
        let call = ToolCall::new("run_command", json!({"command": "echo one \"two words\""}));
        let result = RunCommand.run(&call, &ctx).await;
        assert!(result.is_success(), "{}", result.summary());
        assert!(
            result.output.text.contains("one two words"),
            "{}",
            result.output.text
        );
    }

    #[tokio::test]
    async fn a_redirection_is_an_argument_not_a_feature() {
        if !runs("echo") {
            return;
        }
        let dir = tempfile::tempdir().expect("a temp dir");
        let ctx = Context::new(dir.path().to_path_buf());
        let call = ToolCall::new("run_command", json!({"command": "echo hi > out.txt"}));
        let result = RunCommand.run(&call, &ctx).await;
        // echo happily prints the redirect as text, and no file appears.
        assert!(result.is_success(), "{}", result.summary());
        assert!(
            result.output.text.contains("> out.txt"),
            "{}",
            result.output.text
        );
        assert!(!dir.path().join("out.txt").exists());
    }

    #[tokio::test]
    async fn a_shell_is_refused_before_it_runs() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let registry = Registry::with_defaults();
        let ctx = Context::new(dir.path().to_path_buf());
        for line in [
            json!({"argv": ["sh", "-c", "echo hi"]}),
            json!({"argv": ["bash", "-lc", "echo hi"]}),
        ] {
            let call = ToolCall::new("run_command", line);
            let result = registry.execute(&call, &ctx).await;
            assert!(!result.is_success(), "{}", result.summary());
            assert!(
                result.summary().contains("shell"),
                "the refusal should say why: {}",
                result.summary()
            );
            assert!(
                matches!(result.status, crate::ToolStatus::Refused { .. }),
                "a shell never reaches the process list: {}",
                result.summary()
            );
        }
    }

    #[tokio::test]
    async fn a_destructive_command_waits_for_a_human_even_when_commands_are_automatic() {
        let dir = tempfile::tempdir().expect("a temp dir");
        std::fs::write(dir.path().join("doomed.rs"), "x").expect("writable");
        let registry = Registry::with_defaults();
        let ctx = Context::new(dir.path().to_path_buf()).with_policy(
            Policy::new(dir.path().to_path_buf())
                .with_approval(ApprovalMode::AutonomousSafe)
                .with_sandbox(SandboxMode::Off),
        );
        let call = ToolCall::new("run_command", json!({"argv": ["rm", "doomed.rs"]}));
        let result = registry.execute(&call, &ctx).await;
        assert!(
            crate::needs_approval(&result),
            "even autonomous-safe asks about a deletion: {}",
            result.summary()
        );
        assert!(
            result.output.text.contains("rm doomed.rs"),
            "{}",
            result.output.text
        );
        assert!(dir.path().join("doomed.rs").exists(), "nothing ran");
    }

    #[tokio::test]
    async fn a_path_outside_the_workspace_is_refused_for_the_directory_and_the_program() {
        let ctx = Context::new(PathBuf::from("/repo"));
        let call = ToolCall::new(
            "run_command",
            json!({"argv": ["true"], "path": "../elsewhere"}),
        );
        assert!(RunCommand.intent(&call, &ctx).is_err());

        let call = ToolCall::new("run_command", json!({"argv": ["../../tmp/evil", "x"]}));
        let problem = RunCommand
            .intent(&call, &ctx)
            .expect_err("program climbs out");
        assert!(problem.contains("climbs out"), "{problem}");
    }

    #[tokio::test]
    async fn a_relative_program_inside_the_workspace_is_a_path_intent() {
        let dir = tempfile::tempdir().expect("a temp dir");
        std::fs::create_dir_all(dir.path().join("scripts")).expect("writable");
        std::fs::write(
            dir.path().join("scripts/build.sh"),
            "#!/bin/sh\necho built\n",
        )
        .expect("writable");
        let ctx = Context::new(dir.path().to_path_buf());
        let call = ToolCall::new("run_command", json!({"argv": ["./scripts/build.sh"]}));
        let intent = RunCommand.intent(&call, &ctx).expect("inside");
        assert_eq!(intent.paths.len(), 2, "the directory and the program");
        assert!(intent.paths[1].ends_with("scripts/build.sh"));
    }

    #[test]
    fn an_empty_or_unquoted_command_is_a_message_not_a_run() {
        assert!(argv_of(&ToolCall::new("run_command", json!({"argv": []}))).is_err());
        assert!(argv_of(&ToolCall::new("run_command", json!({"command": "   "}))).is_err());
        let problem = split_words("echo \"unterminated").expect_err("quote never closes");
        assert!(problem.contains("quote"), "{problem}");
        assert_eq!(
            split_words("cargo   test --quiet").expect("split"),
            vec!["cargo", "test", "--quiet"]
        );
        // A literal that would mean something to a shell means nothing here.
        assert_eq!(
            split_words("rm 'a; b'").expect("single quotes"),
            vec!["rm", "a; b"]
        );
    }

    #[test]
    fn a_command_is_never_read_only_however_quiet_it_is() {
        let definition = RunCommand.definition();
        assert_eq!(definition.permission, Permission::Execute);
        assert!(definition.timeout >= Duration::from_secs(60));
        assert!(
            definition.description.contains("no shell"),
            "the model is told the rule it has to follow"
        );
    }

    #[tokio::test]
    async fn a_program_that_never_finishes_is_stopped_and_killed() {
        if !runs("sleep") {
            return;
        }
        let dir = tempfile::tempdir().expect("a temp dir");
        let mut config = agent_config::Config::default();
        config.tools.insert(
            "run_command".to_owned(),
            agent_config::ToolPolicy {
                timeout_seconds: Some(1),
                sandbox: Some(agent_config::SandboxMode::Warn),
                ..Default::default()
            },
        );
        let ctx = Context::new(dir.path().to_path_buf())
            .with_policy(Policy::from_config(dir.path(), &config));
        let call = ToolCall::new("run_command", json!({"argv": ["sleep", "30"]}));
        let started = std::time::Instant::now();
        let result = RunCommand.run(&call, &ctx).await;
        assert!(
            result.summary().contains("stopped after 1s"),
            "{}",
            result.summary()
        );
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the wait ended early"
        );
    }
}
