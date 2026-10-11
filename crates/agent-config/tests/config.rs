//! The contract this crate exists for: a value that was asked for is the value
//! that is used, and a value that was mistyped is refused rather than ignored.

use std::path::Path;

use agent_config::{ApprovalMode, Config, ConfigError, Loaded, ProviderKind, SandboxMode};

type Pairs = Vec<(String, String)>;

fn env(pairs: &Pairs) -> impl Fn(&str) -> Option<String> + '_ {
    move |key| {
        pairs
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.clone())
    }
}

fn load_in(dir: &Path, pairs: &Pairs) -> Result<Loaded, ConfigError> {
    agent_config::load_with(dir, &env(pairs))
}

fn push(pairs: &mut Pairs, key: &str, value: impl Into<String>) {
    pairs.push((key.to_owned(), value.into()));
}

/// A directory with no smoll.toml in it, and a HOME that has no user config
/// either, so a test cannot be influenced by this machine's real settings.
fn clean_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("temp dir");
    std::fs::create_dir_all(user_dir(dir.path())).expect("empty home");
    dir
}

fn user_dir(dir: &Path) -> std::path::PathBuf {
    dir.join("home/.config/smoll")
}

fn home_pairs(dir: &Path) -> Pairs {
    let home = dir.join("home");
    let config = home.join(".config");
    vec![
        ("HOME".to_owned(), home.display().to_string()),
        ("XDG_CONFIG_HOME".to_owned(), config.display().to_string()),
    ]
}

fn write_project(dir: &Path, text: &str) {
    std::fs::write(dir.join("smoll.toml"), text).expect("write project file");
}

fn write_user(dir: &Path, text: &str) {
    std::fs::create_dir_all(user_dir(dir)).expect("create user dir");
    std::fs::write(user_dir(dir).join("config.toml"), text).expect("write user file");
}

#[test]
fn no_files_at_all_gives_a_safe_configuration() {
    let dir = clean_dir();
    let loaded = load_in(dir.path(), &home_pairs(dir.path())).expect("defaults must load");
    let config = &loaded.config;
    assert_eq!(config.agent.approval_mode, ApprovalMode::ApproveEdits);
    assert!(
        config.privacy.workspace_only,
        "file access stays in the repository"
    );
    assert!(
        config.privacy.redact_secrets,
        "secrets are masked by default"
    );
    assert!(!config.privacy.telemetry, "there is nothing to opt out of");
    assert!(config.agent.audit_log, "every run leaves a record");
    assert_eq!(config.providers["mock"].kind, ProviderKind::Mock);
    assert_eq!(loaded.sources, vec!["built-in defaults".to_owned()]);
}

#[test]
fn the_default_mode_asks_before_writing() {
    // The shape of the default mode is the safety story; if it flips, this fails.
    let mode = ApprovalMode::default();
    assert!(mode.allows_write());
    assert!(!mode.auto_writes(), "approve-edits asks before writing");
    assert!(mode.auto_commands());

    assert!(!ApprovalMode::SuggestOnly.allows_write());
    assert!(!ApprovalMode::SuggestOnly.auto_commands());
    assert!(ApprovalMode::ApproveCommands.auto_writes());
    assert!(!ApprovalMode::ApproveCommands.auto_commands());
    assert!(ApprovalMode::AutonomousSafe.auto_writes());
    assert!(ApprovalMode::AutonomousSafe.auto_commands());
}

#[test]
fn sandbox_modes_only_block_when_told_to() {
    assert!(!SandboxMode::Off.blocks());
    assert!(!SandboxMode::Warn.blocks());
    assert!(SandboxMode::Strict.blocks());
}

#[test]
fn a_project_file_overrides_only_the_keys_it_names() {
    let dir = clean_dir();
    write_user(
        dir.path(),
        "[agent]\nmax_steps = 40\naudit_log = false\n\n[privacy]\nredact_secrets = true\n",
    );
    write_project(dir.path(), "[agent]\napproval_mode = \"suggest-only\"\n");

    let loaded = load_in(dir.path(), &home_pairs(dir.path())).expect("loads");
    assert_eq!(
        loaded.config.agent.approval_mode,
        ApprovalMode::SuggestOnly,
        "the project file wins"
    );
    assert_eq!(loaded.config.agent.max_steps, 40, "kept from the user file");
    assert!(!loaded.config.agent.audit_log, "kept from the user file");
    assert!(
        loaded.config.privacy.redact_secrets,
        "kept from the user file"
    );
    assert_eq!(loaded.sources.len(), 2, "both files are named");
}

#[test]
fn a_broken_toml_names_its_file() {
    let dir = clean_dir();
    write_project(dir.path(), "[agent\nmax_steps = ");
    let error = load_in(dir.path(), &home_pairs(dir.path())).expect_err("must fail");
    let message = error.to_string();
    assert!(message.contains("smoll.toml"), "{message}");
    assert!(message.contains("not valid TOML"), "{message}");
}

#[test]
fn an_unknown_section_is_refused_rather_than_ignored() {
    let dir = clean_dir();
    // The classic failure: `[aegnt]` read as an empty section would drop every
    // setting in it, including the approval mode.
    write_project(dir.path(), "[aegnt]\nmax_steps = 5\n");
    let error = load_in(dir.path(), &home_pairs(dir.path())).expect_err("must fail");
    match error {
        ConfigError::Shape { message, sources } => {
            assert!(message.contains("aegnt"), "{message}");
            assert!(sources.contains("smoll.toml"), "{sources}");
        }
        other => panic!("expected a shape error, got {other:?}"),
    }
}

#[test]
fn a_key_typo_inside_a_section_is_refused() {
    let dir = clean_dir();
    write_project(dir.path(), "[agent]\nmax_stepz = 5\n");
    let error = load_in(dir.path(), &home_pairs(dir.path())).expect_err("must fail");
    assert!(matches!(error, ConfigError::Shape { .. }), "{error:?}");
    assert!(error.to_string().contains("max_stepz"), "{error}");
}

#[test]
fn a_mistyped_approval_mode_does_not_fall_back_to_the_default() {
    let dir = clean_dir();
    write_project(dir.path(), "[agent]\napproval_mode = \"yolo\"\n");
    let error = load_in(dir.path(), &home_pairs(dir.path())).expect_err("must fail");
    let message = error.to_string();
    assert!(message.contains("yolo"), "{message}");
    assert!(
        message.contains("approve-edits"),
        "the message lists what is allowed: {message}"
    );
}

#[test]
fn environment_beats_files() {
    let dir = clean_dir();
    write_project(
        dir.path(),
        "[agent]\napproval_mode = \"autonomous-safe\"\nmax_steps = 9\n",
    );
    let mut pairs = home_pairs(dir.path());
    push(&mut pairs, "SMOLL_APPROVAL_MODE", "suggest-only");
    push(&mut pairs, "SMOLL_MAX_STEPS", "3");

    let loaded = load_in(dir.path(), &pairs).expect("loads");
    assert_eq!(loaded.config.agent.approval_mode, ApprovalMode::SuggestOnly);
    assert_eq!(loaded.config.agent.max_steps, 3);
}

#[test]
fn a_bad_variable_names_the_variable_it_refused() {
    let dir = clean_dir();
    let mut pairs = home_pairs(dir.path());
    push(&mut pairs, "SMOLL_APPROVAL_MODE", "nope");
    let error = load_in(dir.path(), &pairs).expect_err("a bad variable must fail");
    match error {
        ConfigError::Invalid { problems } => {
            assert_eq!(
                problems[0].key, "SMOLL_APPROVAL_MODE",
                "the variable, not the file key"
            );
        }
        other => panic!("expected invalid, got {other:?}"),
    }
}

#[test]
fn a_bad_number_in_the_environment_is_named() {
    let dir = clean_dir();
    let mut pairs = home_pairs(dir.path());
    push(&mut pairs, "SMOLL_MAX_STEPS", "lots");
    let error = load_in(dir.path(), &pairs).expect_err("must fail");
    assert!(error.to_string().contains("SMOLL_MAX_STEPS"), "{error}");
}

#[test]
fn booleans_are_read_the_way_people_type_them() {
    let dir = clean_dir();
    for (value, expected) in [("1", true), ("yes", true), ("off", false), ("no", false)] {
        let mut pairs = home_pairs(dir.path());
        push(&mut pairs, "SMOLL_WORKSPACE_ONLY", value);
        let loaded = load_in(dir.path(), &pairs).expect("{value} parses");
        assert_eq!(loaded.config.privacy.workspace_only, expected, "{value}");
    }
    let mut pairs = home_pairs(dir.path());
    push(&mut pairs, "SMOLL_WORKSPACE_ONLY", "maybe");
    assert!(
        load_in(dir.path(), &pairs).is_err(),
        "an unclear boolean is refused"
    );
}

#[test]
fn smoll_config_pointing_nowhere_is_an_error_not_a_shrug() {
    let dir = clean_dir();
    let mut pairs = home_pairs(dir.path());
    let missing = dir.path().join("nowhere.toml");
    push(&mut pairs, "SMOLL_CONFIG", missing.display().to_string());
    let error = load_in(dir.path(), &pairs).expect_err("an explicit path must exist");
    assert!(error.to_string().contains("nowhere.toml"), "{error}");
}

#[test]
fn a_provider_that_cannot_run_is_refused_with_its_name() {
    let dir = clean_dir();
    write_project(
        dir.path(),
        "[providers.ollama]\ntype = \"openai_compatible\"\nmodel = \"qwen2.5-coder:1.5b\"\n",
    );
    let error = load_in(dir.path(), &home_pairs(dir.path())).expect_err("needs base_url");
    let problems = error.problems();
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert_eq!(problems[0].key, "providers.ollama.base_url");
}

#[test]
fn a_gguf_path_that_is_not_a_gguf_is_refused() {
    let dir = clean_dir();
    write_project(
        dir.path(),
        "[providers.local]\ntype = \"gguf\"\npath = \"models/qwen.safetensors\"\n",
    );
    let error = load_in(dir.path(), &home_pairs(dir.path())).expect_err("must fail");
    assert!(error.to_string().contains(".gguf"), "{error}");
}

#[test]
fn a_context_window_too_small_to_hold_a_prompt_is_refused() {
    let dir = clean_dir();
    write_project(
        dir.path(),
        "[providers.m]\ntype = \"mock\"\ncontext_length = 8\n",
    );
    let error = load_in(dir.path(), &home_pairs(dir.path())).expect_err("must fail");
    assert!(
        error.to_string().contains("providers.m.context_length"),
        "{error}"
    );
}

#[test]
fn a_tool_calling_switch_on_a_provider_that_cannot_honour_it_is_refused() {
    let dir = clean_dir();
    write_project(
        dir.path(),
        "[providers.local]\ntype = \"gguf\"\npath = \"~/Models/small.gguf\"\ntool_calling = true\n",
    );
    let error = load_in(dir.path(), &home_pairs(dir.path()))
        .expect_err("a gguf provider would ignore the switch");
    let said = error.to_string();
    assert!(said.contains("providers.local.tool_calling"), "{said}");
    // The two kinds that do read it are left alone.
    write_project(
        dir.path(),
        "[providers.served]\ntype = \"openai_compatible\"\nbase_url = \"http://127.0.0.1:11434/v1\"\ntool_calling = true\n",
    );
    assert!(load_in(dir.path(), &home_pairs(dir.path())).is_ok());
}

#[test]
fn pasting_a_key_into_the_config_is_refused() {
    let dir = clean_dir();
    write_project(
        dir.path(),
        "[providers.hf]\ntype = \"huggingface\"\nrepo_id = \"org/model\"\ntoken_env = \"HF_TOKEN=abc123\"\n",
    );
    let error =
        load_in(dir.path(), &home_pairs(dir.path())).expect_err("a secret is not a variable name");
    assert!(
        error.to_string().contains("environment variable"),
        "{error}"
    );
}

#[test]
fn a_remote_provider_warns_that_prompts_leave_the_machine() {
    let dir = clean_dir();
    write_project(
        dir.path(),
        "[providers.cloud]\ntype = \"openai_compatible\"\nbase_url = \"https://api.example.com/v1\"\nmodel = \"small\"\n",
    );
    let loaded = load_in(dir.path(), &home_pairs(dir.path())).expect("valid, but warned");
    assert_eq!(loaded.warnings.len(), 1, "{:?}", loaded.warnings);
    assert!(
        loaded.warnings[0].contains("not this machine"),
        "{}",
        loaded.warnings[0]
    );
}

#[test]
fn a_loopback_provider_is_silent() {
    let dir = clean_dir();
    for url in [
        "http://localhost:11434/v1",
        "http://127.0.0.1:8080/v1",
        "http://[::1]:8080/v1",
        "http://ollama.local:11434",
    ] {
        write_project(
            dir.path(),
            &format!("[providers.local]\ntype = \"openai_compatible\"\nbase_url = \"{url}\"\n"),
        );
        let loaded = load_in(dir.path(), &home_pairs(dir.path())).expect("loopback is fine");
        assert!(
            loaded.warnings.is_empty(),
            "{url} warned: {:?}",
            loaded.warnings
        );
    }
}

#[test]
fn a_user_can_ask_for_telemetry_and_is_told_it_does_nothing() {
    let dir = clean_dir();
    write_project(dir.path(), "[privacy]\ntelemetry = true\n");
    let loaded = load_in(dir.path(), &home_pairs(dir.path())).expect("not fatal");
    assert!(
        loaded
            .warnings
            .iter()
            .any(|w| w.contains("no telemetry code")),
        "{:?}",
        loaded.warnings
    );
}

#[test]
fn too_many_steps_is_a_mistake_not_a_setting() {
    let dir = clean_dir();
    for value in ["0", "5000"] {
        write_project(dir.path(), &format!("[agent]\nmax_steps = {value}\n"));
        let error = load_in(dir.path(), &home_pairs(dir.path())).expect_err("must fail");
        assert!(error.to_string().contains("agent.max_steps"), "{error}");
    }
}

#[test]
fn every_problem_is_reported_at_once() {
    let dir = clean_dir();
    write_project(dir.path(), "[agent]\nmax_steps = 0\ntimeout_seconds = 0\n");
    let error = load_in(dir.path(), &home_pairs(dir.path())).expect_err("must fail");
    assert_eq!(error.problems().len(), 2, "{:?}", error.problems());
}

#[test]
fn choosing_between_two_providers_is_required_and_named() {
    let dir = clean_dir();
    write_project(
        dir.path(),
        "[providers.a]\ntype = \"mock\"\n\n[providers.b]\ntype = \"echo\"\n",
    );
    let error = load_in(dir.path(), &home_pairs(dir.path())).expect_err("ambiguous");
    let message = error.to_string();
    assert!(message.contains("agent.provider"), "{message}");
    assert!(message.contains("a, b"), "{message}");

    write_project(
        dir.path(),
        "[agent]\nprovider = \"b\"\n\n[providers.a]\ntype = \"mock\"\n\n[providers.b]\ntype = \"echo\"\n",
    );
    let loaded = load_in(dir.path(), &home_pairs(dir.path())).expect("chosen");
    assert_eq!(loaded.config.resolve_provider().expect("b").0, "b");
    assert_eq!(
        loaded.config.resolve_provider().expect("b").1.kind,
        ProviderKind::Echo
    );

    write_project(
        dir.path(),
        "[agent]\nprovider = \"missing\"\n\n[providers.a]\ntype = \"mock\"\n",
    );
    let error = load_in(dir.path(), &home_pairs(dir.path())).expect_err("unknown name");
    assert!(error.to_string().contains("not defined"), "{error}");
}

#[test]
fn smoll_model_lands_on_the_provider_in_use() {
    let dir = clean_dir();
    write_project(
        dir.path(),
        "[agent]\nprovider = \"ollama\"\n\n[providers.ollama]\ntype = \"openai_compatible\"\nbase_url = \"http://localhost:11434/v1\"\nmodel = \"old\"\n",
    );
    let mut pairs = home_pairs(dir.path());
    push(&mut pairs, "SMOLL_MODEL", "qwen2.5-coder:1.5b");
    let loaded = load_in(dir.path(), &pairs).expect("loads");
    assert_eq!(
        loaded
            .config
            .resolve_provider()
            .expect("one")
            .1
            .model
            .as_deref(),
        Some("qwen2.5-coder:1.5b")
    );
}

#[test]
fn an_allowlist_needs_a_sandbox_to_enforce_it() {
    let dir = clean_dir();
    write_project(
        dir.path(),
        "[tools.run_command]\nallowlist = [\"cargo test\"]\n",
    );
    let error = load_in(dir.path(), &home_pairs(dir.path()))
        .expect_err("a list with no mode is decoration");
    assert!(
        error.to_string().contains("tools.run_command.sandbox"),
        "{error}"
    );

    write_project(
        dir.path(),
        "[tools.run_command]\nsandbox = \"strict\"\nallowlist = [\"cargo test\", \"   \"]\n",
    );
    let error =
        load_in(dir.path(), &home_pairs(dir.path())).expect_err("an empty pattern matches all");
    assert!(error.to_string().contains("every command"), "{error}");
}

/// The point of the variable is to tighten or loosen a whole run from a shell,
/// including the tools a file never mentions — which is why it sets one default
/// rather than writing into whichever sections happen to exist. A version of
/// this that created a section for a tool nobody registers would look like a
/// setting and enforce nothing.
#[test]
fn smoll_sandbox_sets_the_boundary_for_tools_that_no_section_named() {
    let dir = clean_dir();
    let mut pairs = home_pairs(dir.path());
    push(&mut pairs, "SMOLL_SANDBOX", "warn");
    let loaded = load_in(dir.path(), &pairs).expect("loads");
    assert_eq!(
        loaded.config.agent.sandbox,
        Some(SandboxMode::Warn),
        "the variable has to apply somewhere"
    );
    assert!(
        loaded.config.tools.keys().next().is_none(),
        "the run-wide mode is not a per-tool section: {:?}",
        loaded.config.tools.keys().collect::<Vec<_>>()
    );

    // A tool's own section still wins over the run-wide mode.
    let dir = clean_dir();
    write_project(dir.path(), "[tools.run_command]\nsandbox = \"strict\"\n");
    let mut pairs = home_pairs(dir.path());
    push(&mut pairs, "SMOLL_SANDBOX", "off");
    let loaded = load_in(dir.path(), &pairs).expect("loads");
    assert_eq!(loaded.config.agent.sandbox, Some(SandboxMode::Off));
    assert_eq!(
        loaded.config.tools["run_command"].sandbox,
        Some(SandboxMode::Strict),
        "the named boundary is the stricter one"
    );
}

#[test]
fn a_config_file_written_by_init_is_usable_immediately() {
    let dir = clean_dir();
    let path = agent_config::write_starter(dir.path(), false).expect("written");
    assert!(path.ends_with("smoll.toml"));
    let loaded = load_in(dir.path(), &home_pairs(dir.path())).expect("the starter is valid");
    assert_eq!(
        loaded.config.agent.approval_mode,
        ApprovalMode::ApproveEdits
    );
    assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
    assert_eq!(
        loaded.config.tools["run_command"].sandbox,
        Some(SandboxMode::Strict)
    );
    assert!(loaded.config.tools["run_command"]
        .allowlist
        .contains(&"cargo test".to_owned()));
}

#[test]
fn the_resolved_config_serialises_for_json_output() {
    let dir = clean_dir();
    let loaded = load_in(dir.path(), &home_pairs(dir.path())).expect("defaults");
    let json = serde_json::to_value(&loaded.config).expect("serialisable");
    assert_eq!(json["agent"]["approval_mode"], "approve-edits");
    assert_eq!(json["privacy"]["telemetry"], false);
    assert!(json["providers"]["mock"].is_object());
}

#[test]
fn the_default_config_validates_and_always_has_a_provider() {
    let mut config = Config::default();
    assert!(config.validate().expect("no problems").is_empty());
    config.providers.clear();
    config.ensure_a_provider();
    assert_eq!(config.providers.len(), 1);
    assert_eq!(config.resolve_provider().expect("mock").0, "mock");
}

#[test]
fn every_documented_variable_is_actually_read() {
    // ENV_VARS is printed by `smoll doctor`, so it must not list a variable the
    // loader ignores. Each one is set to a value only that variable can refuse,
    // and the refusal has to name it.
    let dir = clean_dir();
    for name in [
        "SMOLL_APPROVAL_MODE",
        "SMOLL_SANDBOX",
        "SMOLL_MAX_STEPS",
        "SMOLL_MAX_CONTEXT_TOKENS",
        "SMOLL_TIMEOUT_SECONDS",
        "SMOLL_WORKSPACE_ONLY",
        "SMOLL_REDACT_SECRETS",
        "SMOLL_TELEMETRY",
    ] {
        let mut pairs = home_pairs(dir.path());
        push(&mut pairs, name, "not-a-valid-value");
        let error = load_in(dir.path(), &pairs)
            .expect_err(&format!("{name} was accepted, so nothing reads it"));
        let problems = error.problems();
        assert_eq!(problems.len(), 1, "{name}: {problems:?}");
        assert_eq!(problems[0].key, name, "the message must name the variable");
    }

    // The two string variables set values rather than refusing them, and
    // SMOLL_PROVIDER is checked: naming a provider that is not defined has to
    // be a problem that repeats the name, which is what proves it was read.
    let mut pairs = home_pairs(dir.path());
    push(&mut pairs, "SMOLL_PROVIDER", "chosen");
    let error = load_in(dir.path(), &pairs).expect_err("no provider is named `chosen`");
    assert!(error.to_string().contains("chosen"), "{error}");

    let mut pairs = home_pairs(dir.path());
    push(&mut pairs, "SMOLL_MODEL", "tiny");
    let loaded = load_in(dir.path(), &pairs).expect("a model name is taken as given");
    assert_eq!(
        loaded.config.providers["mock"].model.as_deref(),
        Some("tiny"),
        "SMOLL_MODEL has to reach the provider in use"
    );

    let documented: Vec<&str> = agent_config::ENV_VARS.iter().map(|(n, _, _)| *n).collect();
    for name in ["SMOLL_CONFIG", "SMOLL_PROVIDER", "SMOLL_MODEL"] {
        assert!(
            documented.contains(&name),
            "{name} is read but not documented"
        );
    }
}

#[test]
fn an_absolute_xdg_config_home_is_required_before_it_is_used() {
    // A relative XDG_CONFIG_HOME is invalid per the spec and would make the
    // agent read a file relative to whatever directory it was run from.
    let dir = clean_dir();
    let mut pairs = vec![
        (
            "HOME".to_owned(),
            dir.path().join("home").display().to_string(),
        ),
        ("XDG_CONFIG_HOME".to_owned(), "relative".to_owned()),
    ];
    push(&mut pairs, "SMOLL_MAX_STEPS", "not-a-number");
    let error = load_in(dir.path(), &pairs).expect_err("the value is refused");
    assert_eq!(error.problems()[0].key, "SMOLL_MAX_STEPS");
    assert_eq!(
        agent_config::user_config_path(&env(&pairs)),
        Some(dir.path().join("home/.config/smoll/config.toml")),
        "it falls back to HOME, not the relative path"
    );
}

#[test]
fn a_tool_section_that_only_sets_a_timeout_leaves_the_tool_on() {
    // `enabled` defaults to true when it is parsed, so a hand-built policy that
    // said false would make the two ways of writing a configuration disagree.
    let dir = clean_dir();
    std::fs::write(
        dir.path().join("smoll.toml"),
        "[tools.read_file]\ntimeout_seconds = 5\ndenylist = [\"secrets/**\"]\n",
    )
    .expect("writable");
    let loaded = load_in(dir.path(), &Vec::new()).expect("a valid section");
    let from_file = &loaded.config.tools["read_file"];
    assert!(from_file.enabled, "an absent key means on");
    assert_eq!(from_file.timeout_seconds, Some(5));
    assert_eq!(
        *from_file,
        agent_config::ToolPolicy {
            timeout_seconds: Some(5),
            denylist: vec!["secrets/**".to_owned()],
            ..agent_config::ToolPolicy::default()
        },
        "the default is the parsed one, so a caller can build a policy in code"
    );
}
