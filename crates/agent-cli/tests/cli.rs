//! The binary's own contract: what it prints, and which exit code it leaves
//! behind, because those two are what a script or a CI job reads.

use std::path::Path;
use std::process::{Command, Output};

fn smoll(dir: &Path, args: &[&str]) -> Output {
    // Hermetic: a developer's real ~/.config/smoll must not change what these
    // assertions see.
    Command::new(env!("CARGO_BIN_EXE_smoll"))
        .args(["-C"])
        .arg(dir)
        .args(args)
        .env("HOME", dir.join("home"))
        .env("XDG_CONFIG_HOME", dir.join("home/.config"))
        .env_remove("SMOLL_CONFIG")
        .env_remove("SMOLL_APPROVAL_MODE")
        .env_remove("SMOLL_PROVIDER")
        .env_remove("SMOLL_MODEL")
        .output()
        .expect("the binary runs")
}

fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    )
}

#[test]
fn help_lists_the_subcommands_and_the_flags_a_ci_job_needs() {
    let dir = tempfile::tempdir().expect("temp dir");
    let output = smoll(dir.path(), &["--help"]);
    let help = text(&output);
    for command in [
        "init", "doctor", "config", "tools", "task", "chat", "review", "rollback", "eval",
    ] {
        assert!(
            help.contains(command),
            "{command} missing from --help:\n{help}"
        );
    }
    assert!(
        help.contains("--json"),
        "the machine-readable flag must be discoverable"
    );
    assert!(
        help.contains("-C"),
        "the working directory flag must be discoverable"
    );

    // The run's own flags live on the subcommand, where `smoll --help` does not
    // show them.
    let task_help = text(&smoll(dir.path(), &["task", "--help"]));
    for flag in ["--yes", "--dry-run", "--approval-mode"] {
        assert!(
            task_help.contains(flag),
            "{flag} missing from `smoll task --help`:\n{task_help}"
        );
    }
    assert!(
        help.contains("-C"),
        "the working directory flag must be discoverable"
    );
}

#[test]
fn init_writes_a_file_and_exits_zero() {
    let dir = tempfile::tempdir().expect("temp dir");
    let output = smoll(dir.path(), &["init"]);
    assert!(output.status.success(), "{}", text(&output));
    assert!(
        dir.path().join("smoll.toml").exists(),
        "the file is written"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("approve-edits"),
        "it says what will happen:\n{stdout}"
    );
    assert!(
        stdout.contains("mock"),
        "it says which provider answers:\n{stdout}"
    );
}

#[test]
fn init_twice_refuses_unless_forced() {
    let dir = tempfile::tempdir().expect("temp dir");
    smoll(dir.path(), &["init"]);
    let second = smoll(dir.path(), &["init"]);
    assert_eq!(
        second.status.code(),
        Some(2),
        "a refused overwrite is a usage error"
    );
    assert!(
        text(&second).contains("--force"),
        "it tells you the way out: {}",
        text(&second)
    );
    assert!(smoll(dir.path(), &["init", "--force"]).status.success());
}

#[test]
fn config_prints_toml_for_a_human_and_json_for_a_script() {
    let dir = tempfile::tempdir().expect("temp dir");
    smoll(dir.path(), &["init"]);

    let human = smoll(dir.path(), &["config"]);
    let stdout = String::from_utf8_lossy(&human.stdout);
    assert!(stdout.contains("[agent]"), "{stdout}");
    assert!(stdout.contains("approval_mode"), "{stdout}");

    let machine = smoll(dir.path(), &["config", "--json"]);
    let value: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&machine.stdout)).expect("valid json");
    assert_eq!(value["config"]["agent"]["approval_mode"], "approve-edits");
    assert_eq!(value["config"]["privacy"]["telemetry"], false);
    assert!(
        !value["sources"].as_array().expect("a list").is_empty(),
        "the file that supplied the values is named"
    );
}

#[test]
fn environment_overrides_are_visible_in_the_resolved_output() {
    let dir = tempfile::tempdir().expect("temp dir");
    smoll(dir.path(), &["init"]);
    let output = Command::new(env!("CARGO_BIN_EXE_smoll"))
        .args(["-C"])
        .arg(dir.path())
        .args(["config", "--json"])
        .env("HOME", dir.path().join("home"))
        .env("XDG_CONFIG_HOME", dir.path().join("home/.config"))
        .env("SMOLL_APPROVAL_MODE", "suggest-only")
        .output()
        .expect("the binary runs");
    let value: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&output.stdout)).expect("valid json");
    assert_eq!(value["config"]["agent"]["approval_mode"], "suggest-only");
}

#[test]
fn a_broken_config_is_error_exit_two_with_the_offending_key() {
    let dir = tempfile::tempdir().expect("temp dir");
    std::fs::write(dir.path().join("smoll.toml"), "[agent]\nmax_stepz = 4\n").expect("write");
    let output = smoll(dir.path(), &["config"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(text(&output).contains("max_stepz"), "{}", text(&output));
}

#[test]
fn a_cwd_that_is_not_a_directory_is_refused_before_anything_reads() {
    let dir = tempfile::tempdir().expect("temp dir");
    let output = Command::new(env!("CARGO_BIN_EXE_smoll"))
        .args(["-C", "no-such-directory", "config"])
        .current_dir(dir.path())
        .env("HOME", dir.path().join("home"))
        .output()
        .expect("the binary runs");
    assert_eq!(output.status.code(), Some(2));
    assert!(
        text(&output).contains("not a directory"),
        "{}",
        text(&output)
    );
}

#[test]
fn an_uncompiled_provider_is_named_rather_than_replaced_by_a_script() {
    let dir = tempfile::tempdir().expect("temp dir");
    std::fs::write(
        dir.path().join("smoll.toml"),
        "[providers.local]\ntype = \"gguf\"\npath = \"/models/small.gguf\"\n\n[agent]\nprovider = \"local\"\n",
    )
    .expect("write");

    // `task` refuses rather than answering from the mock the config did not ask
    // for. In a build without the engine the refusal names the cargo flag; in one
    // with it, it names the file that is not there. Both say what is missing.
    let task = smoll(dir.path(), &["task", "--yes", "anything"]);
    assert_eq!(task.status.code(), Some(2), "{}", text(&task));
    let said = text(&task);
    assert!(said.contains("gguf"), "{said}");
    assert!(
        said.contains("--features") || said.contains("small.gguf"),
        "the refusal should say what to do: {said}"
    );

    // `doctor` finds the same thing and leaves the code a CI job reads.
    let doctor = smoll(dir.path(), &["doctor"]);
    assert_eq!(doctor.status.code(), Some(2), "{}", text(&doctor));
    assert!(text(&doctor).contains("!!"), "{}", text(&doctor));
}

#[test]
fn a_run_leaves_a_record_that_review_and_rollback_read_back() {
    let dir = tempfile::tempdir().expect("temp dir");
    smoll(dir.path(), &["init"]);

    let task = smoll(dir.path(), &["task", "--yes", "summarise this repository"]);
    assert!(task.status.success(), "{}", text(&task));
    let stdout = String::from_utf8_lossy(&task.stdout);
    assert!(
        stdout.contains("no scripted replies"),
        "the mock says what it is: {stdout}"
    );
    assert!(
        !stdout.contains("finished"),
        "stdout is the answer, not the running feed: {stdout}"
    );
    let feed = String::from_utf8_lossy(&task.stderr);
    assert!(
        feed.contains("task      summarise") && feed.contains("finished  completed"),
        "a human sees the run as it goes: {feed}"
    );
    let record = dir.path().join(".smoll/sessions");
    let files = std::fs::read_dir(&record).expect("the session directory exists");
    assert_eq!(files.count(), 1, "one run leaves exactly one record behind");

    let review = smoll(dir.path(), &["review"]);
    assert!(review.status.success(), "{}", text(&review));
    let reviewed = String::from_utf8_lossy(&review.stdout);
    assert!(reviewed.contains("task      summarise"), "{reviewed}");
    assert!(
        reviewed.contains("wrote no files"),
        "it admits there is nothing to undo: {reviewed}"
    );

    // Undoing a run that wrote nothing is a report, not an error.
    let rollback = smoll(dir.path(), &["rollback"]);
    assert!(rollback.status.success(), "{}", text(&rollback));
    assert!(
        text(&rollback).contains("nothing to put back"),
        "{}",
        text(&rollback)
    );
}

#[test]
fn task_json_answers_with_one_document_and_keeps_the_feed_on_stderr() {
    let dir = tempfile::tempdir().expect("temp dir");
    smoll(dir.path(), &["init"]);
    let output = smoll(
        dir.path(),
        &["task", "--json", "--yes", "what does this repo do?"],
    );
    assert!(output.status.success(), "{}", text(&output));

    // stdout is parseable only because the event feed went elsewhere.
    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("one json document on stdout");
    assert_eq!(value["task"], "what does this repo do?");
    assert_eq!(value["kind"], "completed");
    assert_eq!(value["succeeded"], true);
    assert!(value["stoppedBy"].is_null(), "{}", value["stoppedBy"]);
    assert!(
        value["record"]
            .as_str()
            .expect("a path")
            .contains(".smoll/sessions"),
        "the answer names the record it left"
    );
    assert!(
        value["plan"].as_str().expect("a plan").contains("1."),
        "the plan the run worked to is in the answer"
    );
    assert!(
        value["usage"]["totalTokens"].as_u64().expect("tokens") > 0,
        "and what it cost"
    );

    let feed = String::from_utf8_lossy(&output.stderr);
    assert!(
        !feed.contains("finished"),
        "--json answers with the document and stays off the feed: {feed}"
    );
}

#[test]
fn a_prompt_that_cannot_fit_a_small_window_says_what_buys_the_room_back() {
    let dir = tempfile::tempdir().expect("temp dir");
    smoll(dir.path(), &["init"]);
    // A 2B-class window: after the loop has dropped every file and every line of
    // the repository map, the tool definitions themselves still do not fit.
    let file = dir.path().join("smoll.toml");
    let config = std::fs::read_to_string(&file).expect("`init` wrote a file");
    std::fs::write(
        &file,
        config.replace(
            "[providers.mock]",
            "[providers.mock]\ncontext_length = 1024",
        ),
    )
    .expect("writable");

    let output = smoll(dir.path(), &["task", "--yes", "summarise this repository"]);
    assert_eq!(output.status.code(), Some(1), "{}", text(&output));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("context tokens needed"), "{stderr}");
    assert!(
        stderr.contains("hint:")
            && stderr.contains("smoll tools")
            && stderr.contains("enabled = false"),
        "a number is not a way out, so the failure names one: {stderr}"
    );
}

#[test]
fn a_dry_run_asks_nobody_and_still_leaves_its_record() {
    let dir = tempfile::tempdir().expect("temp dir");
    smoll(dir.path(), &["init"]);
    std::fs::write(dir.path().join("notes.md"), "old text\n").expect("write");
    // No terminal here and no `--yes`, yet the run is not refused: a dry run
    // asks nobody, because it is not allowed to write anything.
    let output = smoll(dir.path(), &["task", "--dry-run", "change the notes"]);
    assert!(output.status.success(), "{}", text(&output));
    assert_eq!(
        std::fs::read_to_string(dir.path().join("notes.md")).expect("read"),
        "old text\n",
        "and the file is as it was"
    );
    assert_eq!(
        std::fs::read_dir(dir.path().join(".smoll/sessions"))
            .expect("a record")
            .count(),
        1,
        "a dry run still leaves its transcript"
    );
}

#[test]
fn tools_lists_every_default_tool_and_what_offering_it_costs() {
    let dir = tempfile::tempdir().expect("temp dir");
    let output = smoll(dir.path(), &["tools"]);
    assert!(output.status.success(), "{}", text(&output));
    let stdout = String::from_utf8_lossy(&output.stdout);
    for name in [
        "read_file",
        "write_file",
        "patch_file",
        "delete_file",
        "search_text",
        "search_symbols",
        "run_command",
        "git_status",
        "git_diff",
        "git_log",
        "git_show",
        "git_add",
        "git_commit",
        "list_dir",
        "search_files",
        "memory_store",
        "memory_recall",
        "task_plan_create",
        "task_plan_update",
        "test_run",
        "check_run",
        "lint_run",
        "format_run",
        "build_run",
    ] {
        assert!(stdout.contains(name), "{name} missing from:\n{stdout}");
    }
    assert!(
        stdout.contains("prompt tokens"),
        "a small-model agent has to say what the tool list costs: {stdout}"
    );
    assert!(
        stdout.contains("destructive"),
        "the one class a person should not have to guess at is in the listing:\n{stdout}"
    );

    let machine = smoll(dir.path(), &["tools", "--json"]);
    let value: serde_json::Value =
        serde_json::from_slice(&machine.stdout).expect("valid json for tools --json");
    let rows = value["tools"].as_array().expect("a list");
    assert_eq!(rows.len(), 24, "{rows:?}");
    assert!(
        rows.iter()
            .all(|row| row["permission"].is_string() && row["enabled"].is_boolean()),
        "each row says its class and whether this config offers it"
    );
    assert!(value["offeredTokensPerStep"].as_u64().expect("a number") > 0);
}

#[test]
fn a_tool_section_that_names_no_registered_tool_is_called_out() {
    // The bug this guards against is the quiet kind: a denylist written under a
    // name the build does not register parses fine, gates nothing, and leaves
    // its author believing `rm -rf` is barred from the run.
    let dir = tempfile::tempdir().expect("temp dir");
    std::fs::write(
        dir.path().join("smoll.toml"),
        "[providers.mock]\ntype = \"mock\"\n\n[tools.shell]\ndenylist = [\"rm -rf\"]\n",
    )
    .expect("write");

    let output = smoll(dir.path(), &["doctor"]);
    assert!(
        output.status.success(),
        "an inert section is a warning, not a broken workspace: {}",
        text(&output)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("warning: [tools.shell] gates nothing"),
        "{stderr}"
    );

    let value: serde_json::Value =
        serde_json::from_slice(&smoll(dir.path(), &["tools", "--json"]).stdout)
            .expect("valid json for tools --json");
    assert_eq!(
        value["unmatchedConfigSections"].as_array().expect("a list"),
        &vec![serde_json::json!("shell")],
        "a script reading the json gets the same finding"
    );

    // A section that names a real tool is left alone.
    std::fs::write(
        dir.path().join("smoll.toml"),
        "[providers.mock]\ntype = \"mock\"\n\n[tools.run_command]\nsandbox = \"strict\"\ndenylist = [\"rm -rf\"]\n",
    )
    .expect("write");
    let output = smoll(dir.path(), &["doctor"]);
    assert!(
        !text(&output).contains("gates nothing"),
        "{}",
        text(&output)
    );
}

#[test]
fn a_switched_off_tool_is_listed_and_marked() {
    let dir = tempfile::tempdir().expect("temp dir");
    std::fs::write(
        dir.path().join("smoll.toml"),
        "[tools.run_command]\nenabled = false\n",
    )
    .expect("write");
    let output = smoll(dir.path(), &["tools"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("off  run_command"),
        "a tool that is off is still shown, so a person can switch it on:\n{stdout}"
    );
    assert!(
        stdout.contains("23 of 24 tool(s) offered"),
        "and the count says so: {stdout}"
    );
}

#[test]
fn doctor_reports_the_machine_the_repo_and_the_record_without_changing_anything() {
    let dir = tempfile::tempdir().expect("temp dir");
    std::fs::write(dir.path().join("notes.md"), "old text\n").expect("write");
    let output = smoll(dir.path(), &["doctor"]);
    assert!(output.status.success(), "{}", text(&output));
    let stdout = String::from_utf8_lossy(&output.stdout);
    for check in [
        "workspace",
        "config",
        "provider",
        "approval",
        "sandbox",
        "privacy",
        "limits",
        "tools",
        "repository",
        "validation",
        "audit",
        "runs",
        "memory",
        "machine",
    ] {
        assert!(
            stdout.contains(check),
            "{check} missing from doctor:\n{stdout}"
        );
    }
    assert!(
        stdout.contains("no smoll.toml"),
        "a workspace with no smoll.toml is told: {stdout}"
    );

    // It reads, so nothing appears that was not there.
    let value: serde_json::Value =
        serde_json::from_slice(&smoll(dir.path(), &["doctor", "--json"]).stdout.to_vec())
            .expect("valid json for doctor --json");
    assert_eq!(value["ok"], true);
    assert_eq!(value["checks"].as_array().expect("checks").len(), 14);
    assert!(
        dir.path().join("notes.md").is_file(),
        "doctor leaves the repository alone"
    );
    assert!(
        !dir.path().join("smoll.toml").exists(),
        "and writes no config of its own"
    );
    assert!(
        !dir.path().join(".smoll").exists(),
        "nor the audit log it reports on",
    );
}

#[test]
fn a_run_written_by_the_loop_is_undone_by_the_binary() {
    // The record comes from a real run through the loop, exactly the way
    // `smoll task` writes one; the binary then reviews and undoes it. That is
    // the part a scripted mock cannot do inside a single command.
    use std::sync::Arc;

    use agent_core::{Agent, StoredSession};
    use agent_providers::{MockProvider, Reply};
    use agent_tools::{Context, Registry};
    use serde_json::json;

    let dir = tempfile::tempdir().expect("temp dir");
    let notes = dir.path().join("notes.md");
    std::fs::write(&notes, "old text\n").expect("write");

    let provider = Arc::new(MockProvider::scripted([
        Reply::call(
            "write_file",
            json!({"path": "notes.md", "content": "new text\n"}),
        ),
        Reply::text("Rewrote notes.md."),
    ]));
    let mut agent = Agent::new(
        provider,
        Registry::with_defaults(),
        Context::new(dir.path()),
    )
    .autoapprove();
    let runtime = tokio::runtime::Runtime::new().expect("a runtime");
    let run = runtime.block_on(agent.run("replace the text in notes.md"));
    assert!(run.succeeded(), "the scripted run finishes");
    assert_eq!(std::fs::read_to_string(&notes).expect("read"), "new text\n");
    let stored = StoredSession::new(run.session, agent.context().snapshots());
    stored.write(dir.path()).expect("recorded");

    let review = smoll(dir.path(), &["review"]);
    let reviewed = String::from_utf8_lossy(&review.stdout);
    assert!(
        reviewed.contains("notes.md") && reviewed.contains("`smoll rollback`"),
        "review says what can be put back:\n{reviewed}"
    );

    let rollback = smoll(dir.path(), &["rollback"]);
    assert!(rollback.status.success(), "{}", text(&rollback));
    assert_eq!(
        std::fs::read_to_string(&notes).expect("read"),
        "old text\n",
        "the binary undid the run's write"
    );
    let restored = String::from_utf8_lossy(&rollback.stdout);
    assert!(restored.contains("restored  notes.md"), "{restored}");

    // The record survives the undo and says it happened, so a second rollback
    // reports rather than silently doing nothing.
    let again = smoll(dir.path(), &["review", "--json"]);
    let value: serde_json::Value =
        serde_json::from_slice(&again.stdout).expect("the record still parses");
    let events = value["session"]["events"].as_array().expect("events");
    assert!(
        events.iter().any(|event| event["event"] == "rolled_back"),
        "the rollback is in the transcript: {events:?}"
    );
}

#[test]
fn a_record_that_is_not_there_is_a_usage_error_not_a_failed_run() {
    let dir = tempfile::tempdir().expect("temp dir");
    let missing = smoll(dir.path(), &["review"]);
    assert_eq!(missing.status.code(), Some(2), "{}", text(&missing));
    assert!(
        text(&missing).contains("`smoll task`"),
        "it says what writes one: {}",
        text(&missing)
    );

    let unknown_id = smoll(dir.path(), &["rollback", "--id", "not-a-uuid"]);
    assert_eq!(unknown_id.status.code(), Some(2));
    assert!(
        text(&unknown_id).contains("not a session id"),
        "{}",
        text(&unknown_id)
    );
}

#[test]
fn running_with_no_subcommand_is_a_usage_error() {
    let dir = tempfile::tempdir().expect("temp dir");
    let output = Command::new(env!("CARGO_BIN_EXE_smoll"))
        .env("HOME", dir.path().join("home"))
        .output()
        .expect("the binary runs");
    assert_eq!(output.status.code(), Some(2));
    assert!(text(&output).contains("--help"), "{}", text(&output));
}

/// A test process has pipes on both ends, which is exactly the case `smoll chat`
/// has to refuse: nothing here can leave a shell in raw mode, and the answer a
/// script gets is one line and exit 2.
#[test]
fn chat_refuses_a_place_with_no_terminal() {
    let dir = tempfile::tempdir().expect("temp dir");
    let output = smoll(dir.path(), &["chat"]);
    assert_eq!(output.status.code(), Some(2), "{}", text(&output));
    let shown = text(&output);
    assert!(shown.contains("no terminal to draw on"), "{shown}");
    assert!(
        shown.contains("smoll task"),
        "the refusal must name the command that works here: {shown}"
    );
    assert!(
        !shown.contains('\x1b'),
        "no escape sequence reaches a pipe, so a log file stays readable"
    );

    let both = smoll(dir.path(), &["chat", "--json"]);
    assert_eq!(both.status.code(), Some(2));
    let shown = text(&both);
    assert!(shown.contains("no json form"), "{shown}");
    assert!(
        shown.contains("--json"),
        "the pointer names the flag that does work: {shown}"
    );
}

#[test]
fn chat_help_names_the_flags_the_screen_uses() {
    let dir = tempfile::tempdir().expect("temp dir");
    let help = text(&smoll(dir.path(), &["chat", "--help"]));
    for flag in ["--yes", "--dry-run", "--approval-mode", "--theme"] {
        assert!(
            help.contains(flag),
            "{flag} missing from `chat --help`:\n{help}"
        );
    }
}

/// `smoll eval` reports on itself as it runs, so a CI job can read the same
/// numbers a person does. The JSON is what a script parses.
fn eval_report(dir: &Path) -> serde_json::Value {
    let output = smoll(dir, &["eval", "--json"]);
    assert_eq!(output.status.code(), Some(0), "{}", text(&output));
    serde_json::from_slice(&output.stdout).expect("`smoll eval --json` prints one JSON report")
}

#[test]
fn eval_keeps_every_promise_it_prints() {
    let dir = tempfile::tempdir().expect("temp dir");
    let report = eval_report(dir.path());
    let fixtures = report["fixtures"].as_array().expect("a list");
    assert!(
        fixtures.len() >= 8,
        "the harness is supposed to cover the safety promises, found {}",
        fixtures.len()
    );
    assert_eq!(report["failed"].as_u64(), Some(0), "{report}");
    assert_eq!(
        report["passed"].as_u64(),
        Some(fixtures.len() as u64),
        "a fixture that failed is not counted as passed"
    );
    for fixture in fixtures {
        assert!(
            fixture["passed"].as_bool().unwrap_or(false),
            "{} failed: {}",
            fixture["name"],
            fixture["problems"]
        );
    }
    // The command that prices the tool list agrees with the registry that
    // charges for it, rather than with a number copied into a test.
    let expected = agent_tools::Registry::with_defaults().token_cost();
    assert_eq!(report["listTokens"].as_u64(), Some(expected as u64));
    assert_eq!(
        report["toolsOffered"].as_u64(),
        Some(agent_tools::Registry::with_defaults().len() as u64)
    );
}

#[test]
fn eval_says_in_every_run_that_no_model_answered() {
    let dir = tempfile::tempdir().expect("temp dir");
    let report = eval_report(dir.path());
    let measures = report["measures"].as_str().expect("a scope line");
    for phrase in ["not any model", "real weights", "capability"] {
        assert!(
            measures.contains(phrase),
            "the scope line omits {phrase}: {measures}"
        );
    }

    let printed = smoll(dir.path(), &["eval"]);
    let shown = text(&printed);
    assert_eq!(printed.status.code(), Some(0));
    assert!(
        shown.contains("No model was run to make this table"),
        "{shown}"
    );

    let rows = report["cost"].as_array().expect("a cost table");
    let native = rows
        .iter()
        .find(|row| row["window"] == serde_json::json!(4096) && row["toolBlock"] == "native")
        .expect("a native row at the window a 4B model is configured with");
    assert_eq!(native["repositoryMapInFirstTurn"], serde_json::json!(true));
    assert_eq!(
        native["fileBodiesInFirstTurn"],
        serde_json::json!(true),
        "at 4096 the loop is supposed to hand the model the files, not only their names"
    );
    let prose = rows
        .iter()
        .find(|row| row["window"] == serde_json::json!(4096) && row["toolBlock"] == "prose")
        .expect("a prose row at the same window");
    assert!(
        prose["firstTurnTokens"].as_u64() < native["firstTurnTokens"].as_u64(),
        "a prose block is asked for less than a native one: {prose} vs {native}"
    );
}

#[test]
fn eval_leaves_the_workspace_it_was_called_from_alone() {
    let dir = tempfile::tempdir().expect("temp dir");
    eval_report(dir.path());
    // Every fixture builds its repository somewhere else, so nothing lands
    // here — not a session file, not a `.smoll` directory, not a notes.md.
    let left = std::fs::read_dir(dir.path().join("home"))
        .map(|entries| entries.count())
        .unwrap_or(0);
    assert_eq!(left, 0, "`smoll eval` wrote into HOME");
    for name in [".smoll", "notes.md", "src"] {
        assert!(
            !dir.path().join(name).exists(),
            "`smoll eval` left {name} in the workspace it was run in"
        );
    }
}
