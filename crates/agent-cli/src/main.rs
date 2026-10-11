//! The `smoll` binary: one CLI in front of the agent crates.
//!
//! Every command takes `-C/--cwd` and `--json`, so the same answer a human reads
//! can be consumed by a script or a CI job. The exit codes mean something: 0 for
//! a run that reached its goal, 1 for a run that finished and said the task did
//! not work, 2 for a usage, configuration, policy or approval stop (clap itself
//! uses 2, which is why a bad configuration exits 2 too rather than 1).
//!
//! Where output goes is a rule rather than an accident. A run's live event feed
//! and the questions it asks go to stderr; stdout holds only the command's
//! answer — the recap for a human, one document for `--json`. A script that
//! parses stdout cannot do that with a "[y]es?" in the middle of it.
//!
//! The commands that act on a repository all read and write the same record: a
//! run leaves `.smoll/sessions/<id>.json` behind, `smoll review` reads it back,
//! and `smoll rollback` undoes the writes it journals. That is what lets a
//! person run an agent, close the terminal, and still take the change back.

mod terminal;

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use serde_json::json;
use uuid::Uuid;

use agent_config::{ApprovalMode, Config, ConfigError};
use agent_core::{now_ms, Agent, AgentError, AgentEvent, Budget, Outcome, Run, StoredSession};
use agent_sandbox::{read_entries, AuditLog, Policy, AUDIT_FILE};
use agent_tools::{Context, Registry};

use crate::terminal::{Printer, Terminal};

#[derive(Debug, Parser)]
#[command(
    name = "smoll",
    version,
    about = "A local-first coding agent for small language models",
    long_about = "smoll reads your repository, plans a change, proposes a diff, asks before it \
                  writes, and leaves a record you can undo from.\n\n\
                  It answers from a model you run locally; nothing about your code leaves the \
                  machine unless you configure a remote provider.\n\n\
                  Build status: the loop, the twenty-four default tools, the sandbox, the audit \
                  log and the session record all work end to end. In a default build the providers that \
                  answer are the scripted mock and the echo inspector; a local .gguf file answers \
                  from its own weights in a binary built with --features llama-cpp, and a server \
                  on the OpenAI wire — Ollama, LM Studio, vLLM, TGI, llama-server, smoll serve — \
                  answers in one built with --features openai-compatible. Asking for a provider \
                  this build cannot run says so rather than falling back to a script."
)]
struct Cli {
    /// Run as if started in DIR, which is how a script points the agent at a
    /// repository without changing the process's working directory.
    #[arg(short = 'C', long, value_name = "DIR", global = true)]
    cwd: Option<PathBuf>,

    /// Machine-readable output where the command has it
    #[arg(long, global = true)]
    json: bool,

    /// Log to stderr, filtered by RUST_LOG when that is set
    #[arg(short = 'v', long, global = true)]
    verbose: bool,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Write a starter smoll.toml and show what it contains
    Init {
        /// Replace an existing smoll.toml
        #[arg(long)]
        force: bool,
    },
    /// What this machine, this repository and this config add up to
    Doctor,
    /// Show the resolved configuration: files, environment and defaults
    Config,
    /// List the tools this build exposes, with their permission class
    Tools,
    /// Run one task end to end
    Task {
        /// The instruction, in plain language
        prompt: String,

        /// Write and run without asking anyone. The sandbox still refuses what
        /// it refuses; this only skips the questions aimed at a person.
        #[arg(long)]
        yes: bool,

        /// Change nothing: every write comes back as a proposed diff
        #[arg(long)]
        dry_run: bool,

        /// Override the configured approval mode for this run
        #[arg(long, value_name = "MODE")]
        approval_mode: Option<ApprovalMode>,
    },
    /// Show a recorded run: its plan, its outcome and the files it wrote
    Review {
        /// The run to show, or the most recent one in this workspace
        #[arg(long, value_name = "ID")]
        id: Option<String>,
    },
    /// Put the files a recorded run wrote back the way it found them
    Rollback {
        #[arg(long, value_name = "ID")]
        id: Option<String>,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    init_tracing(cli.verbose);
    let cwd = match resolve_cwd(cli.cwd.as_deref()) {
        Ok(path) => path,
        Err(message) => return Failure::usage(message).print(),
    };
    let outcome = match cli.command {
        None => return print_usage_hint(),
        Some(Commands::Init { force }) => cmd_init(&cwd, cli.json, force),
        Some(Commands::Config) => cmd_config(&cwd, cli.json),
        Some(Commands::Tools) => cmd_tools(&cwd, cli.json),
        Some(Commands::Doctor) => cmd_doctor(&cwd, cli.json),
        Some(Commands::Task {
            prompt,
            yes,
            dry_run,
            approval_mode,
        }) => cmd_task(&cwd, cli.json, &prompt, yes, dry_run, approval_mode).await,
        Some(Commands::Review { id }) => cmd_review(&cwd, cli.json, id.as_deref()),
        Some(Commands::Rollback { id }) => cmd_rollback(&cwd, cli.json, id.as_deref()),
    };
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(failure) => failure.print(),
    }
}

/// A command's answer when it is not "it worked": an exit code, and a line to
/// say why when the command did not already have one on screen.
struct Failure {
    message: Option<String>,
    code: u8,
}

impl Failure {
    /// Something the command could not act on: bad usage, bad config, a refusal.
    fn usage(message: impl Into<String>) -> Self {
        Self {
            message: Some(message.into()),
            code: 2,
        }
    }

    /// The command said everything it had to say and the run ended badly; only
    /// the code is left to carry.
    fn exit(code: u8) -> Self {
        Self {
            message: None,
            code,
        }
    }

    fn print(self) -> ExitCode {
        if let Some(message) = self.message {
            eprintln!("error: {message}");
        }
        ExitCode::from(self.code)
    }
}

impl From<String> for Failure {
    fn from(message: String) -> Self {
        Self::usage(message)
    }
}

impl From<ConfigError> for Failure {
    fn from(source: ConfigError) -> Self {
        Self::usage(source.to_string())
    }
}

impl From<AgentError> for Failure {
    fn from(error: AgentError) -> Self {
        // A refusal by policy or by a person is exit 2 on its own terms, so the
        // error is asked rather than assumed.
        Self {
            message: Some(error.to_string()),
            code: error.exit_code(),
        }
    }
}

fn resolve_cwd(explicit: Option<&Path>) -> Result<PathBuf, String> {
    match explicit {
        Some(path) => {
            if !path.is_dir() {
                return Err(format!("{} is not a directory", path.display()));
            }
            Ok(path.to_path_buf())
        }
        None => {
            std::env::current_dir().map_err(|source| format!("cannot find the directory: {source}"))
        }
    }
}

/// `smoll init` is the first thing a reader does, so it has to leave them with a
/// configuration that runs and tell them plainly what it will do.
fn cmd_init(cwd: &Path, json: bool, force: bool) -> Result<(), Failure> {
    let path = agent_config::write_starter(cwd, force)?;
    let loaded = agent_config::load(cwd)?;
    if json {
        let report = json!({
            "written": path,
            "approvalMode": loaded.config.agent.approval_mode.to_string(),
            "provider": loaded.config.resolve_provider().map_err(render)?.0,
            "sources": loaded.sources,
            "warnings": loaded.warnings,
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&report).expect("a value built here")
        );
        return Ok(());
    }
    println!("wrote {}", path.display());
    println!();
    println!("  provider       {}", describe_provider(&loaded.config));
    println!("  approval mode  {}", loaded.config.agent.approval_mode);
    println!("  workspace only {}", loaded.config.privacy.workspace_only);
    println!();
    println!(
        "The starter answers from the mock provider, so this configuration runs with no model \
         on disk. Point it at a real one by editing [providers] in {}; until then `smoll task` \
         asks a script that says it has nothing scripted.",
        path.display()
    );
    print_warnings(&loaded.warnings);
    Ok(())
}

fn cmd_config(cwd: &Path, json: bool) -> Result<(), Failure> {
    let loaded = agent_config::load(cwd)?;
    if json {
        let report = json!({
            "config": serde_json::to_value(&loaded.config).expect("a type that serialises"),
            "sources": loaded.sources,
            "warnings": loaded.warnings,
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&report).expect("a value built here")
        );
        return Ok(());
    }
    for source in &loaded.sources {
        eprintln!("# from {source}");
    }
    print!(
        "{}",
        loaded
            .config
            .to_toml_string()
            .map_err(|source| source.to_string())?
    );
    print_warnings(&loaded.warnings);
    Ok(())
}

/// `smoll tools` lists what a run can be given, including the tools this
/// configuration has switched off — a person deciding whether to switch one on
/// needs to see the one that is missing.
fn cmd_tools(cwd: &Path, json: bool) -> Result<(), Failure> {
    let loaded = agent_config::load(cwd)?;
    let policy = Policy::from_config(cwd, &loaded.config);
    let registry = Registry::with_defaults();
    let rows = registry.inventory(&policy);
    let orphaned = orphaned_tool_names(&loaded.config, &registry);
    let offered: usize = rows
        .iter()
        .filter(|row| row.enabled)
        .map(|row| row.token_cost)
        .sum();
    let enabled = rows.iter().filter(|row| row.enabled).count();

    if json {
        let report = json!({
            "tools": rows,
            "offered": enabled,
            "offeredTokensPerStep": offered,
            "unmatchedConfigSections": orphaned,
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&report).expect("a value built here")
        );
        for name in &orphaned {
            eprintln!("warning: {}", orphan_warning(name));
        }
        return Ok(());
    }
    for row in &rows {
        let state = if row.enabled { "on" } else { "off" };
        println!(
            "{state:>3}  {:<16} {:<12} {:>4}s {:>8}B  {}",
            row.name,
            row.permission,
            row.timeout_seconds,
            row.max_output_bytes,
            first_sentence(&row.description)
        );
    }
    println!();
    println!(
        "{enabled} of {} tool(s) offered, which costs about {offered} prompt tokens on every \
         step of a run.",
        rows.len()
    );
    println!("That number is why a small-model agent ships few tools, not many.");
    for name in &orphaned {
        eprintln!("warning: {}", orphan_warning(name));
    }
    Ok(())
}

/// One line of `smoll doctor`: a label, what was found, and whether it is a
/// problem. Nothing here changes anything; a doctor reads.
struct Line {
    label: &'static str,
    answer: String,
    ok: bool,
}

impl Line {
    fn good(label: &'static str, answer: impl Into<String>) -> Self {
        Self {
            label,
            answer: answer.into(),
            ok: true,
        }
    }

    fn bad(label: &'static str, answer: impl Into<String>) -> Self {
        Self {
            label,
            answer: answer.into(),
            ok: false,
        }
    }
}

fn cmd_doctor(cwd: &Path, json: bool) -> Result<(), Failure> {
    let mut lines: Vec<Line> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();

    // A config that will not parse is reported as a bad line and the rest of the
    // answers go on with the defaults, because "your machine has 8 GiB free" is
    // still worth knowing when your TOML is broken.
    let config = match agent_config::load(cwd) {
        Ok(loaded) => {
            warnings = loaded.warnings.clone();
            lines.push(Line::good("workspace", cwd.display().to_string()));
            lines.push(config_line(cwd, &loaded.sources));
            loaded.config
        }
        Err(source) => {
            lines.push(Line::bad("config", source.to_string()));
            Config::default()
        }
    };
    lines.extend(boundaries(cwd, &config));
    lines.extend(what_is_here(cwd, &config));
    lines.push(machine_line());

    // A safety setting that names no tool is the worst kind of typo, so it is
    // said here even though the file parses and the run would start.
    warnings.extend(
        orphaned_tool_names(&config, &Registry::with_defaults())
            .iter()
            .map(|name| orphan_warning(name)),
    );

    let ok = lines.iter().all(|line| line.ok);
    if json {
        let report = json!({
            "ok": ok,
            "checks": lines
                .iter()
                .map(|line| json!({ "check": line.label, "answer": line.answer, "ok": line.ok }))
                .collect::<Vec<_>>(),
            "warnings": warnings,
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&report).expect("a value built here")
        );
    } else {
        println!("smoll doctor · {}", cwd.display());
        for line in &lines {
            println!("{:>11}  {}  {}", mark(line.ok), line.label, line.answer);
        }
        print_warnings(&warnings);
        if !ok {
            eprintln!("doctor: something above needs fixing before a run can work.");
        }
    }
    if ok {
        Ok(())
    } else {
        // A doctor that finds a problem leaves the code a CI job reads as
        // "this workspace is not ready", which is what 2 means here.
        Err(Failure::exit(2))
    }
}

/// Where the run's settings came from.
fn config_line(cwd: &Path, sources: &[String]) -> Line {
    if cwd.join(agent_config::PROJECT_FILE).is_file() {
        Line::good("config", sources.join(", "))
    } else {
        Line::good(
            "config",
            format!(
                "{} (no {} here; `smoll init` writes one)",
                sources.join(", "),
                agent_config::PROJECT_FILE
            ),
        )
    }
}

/// The limits a run is held to, and whether the thing answering it exists.
fn boundaries(cwd: &Path, config: &Config) -> Vec<Line> {
    let mut lines = Vec::new();

    // A provider this build cannot make is a real finding, not a note: it is
    // the difference between `smoll task` working and it refusing.
    lines.push(match config.resolve_provider() {
        Ok((name, provider)) => match agent_providers::build(name, provider) {
            Ok(built) => Line::good(
                "provider",
                format!(
                    "{name} (type {}) answers as {}",
                    provider.kind,
                    built.name()
                ),
            ),
            Err(source) => Line::bad("provider", source.to_string()),
        },
        Err(problems) => Line::bad("provider", render(problems)),
    });

    lines.push(Line::good(
        "approval",
        format!(
            "{} — {}",
            config.agent.approval_mode,
            approval_note(config.agent.approval_mode)
        ),
    ));
    let policy = Policy::from_config(cwd, config);
    lines.push(Line::good(
        "sandbox",
        format!(
            "{} per tool, workspace only {}",
            policy.sandbox_mode(),
            yes_no(config.privacy.workspace_only)
        ),
    ));
    lines.push(Line::good(
        "privacy",
        format!(
            "telemetry {}, secrets {}, workspace {}",
            yes_no(config.privacy.telemetry),
            yes_no(config.privacy.redact_secrets),
            yes_no(config.privacy.workspace_only)
        ),
    ));
    lines.push(Line::good(
        "limits",
        format!(
            "{} steps, {} context tokens, {}s, {} retry(ies)",
            config.agent.max_steps,
            config.agent.max_context_tokens,
            config.agent.timeout_seconds,
            config.agent.retries
        ),
    ));

    let inventory = Registry::with_defaults().inventory(&policy);
    let mut classes: Vec<String> = inventory
        .iter()
        .filter(|row| row.enabled)
        .map(|row| row.permission.to_string())
        .collect();
    classes.sort();
    classes.dedup();
    lines.push(Line::good(
        "tools",
        format!(
            "{} of {} enabled, {} permission class(es) in use",
            inventory.iter().filter(|row| row.enabled).count(),
            inventory.len(),
            classes.len()
        ),
    ));
    lines
}

/// The repository, its validation command, and what has already been recorded
/// in it. None of this opens anything for writing: a doctor that reports on a
/// workspace must not be the reason it has a `.smoll` directory in it.
fn what_is_here(cwd: &Path, config: &Config) -> Vec<Line> {
    let index = agent_repo::Index::walk(cwd);
    let project = agent_repo::Project::detect(&index);
    let path = cwd.join(AUDIT_FILE);
    let runs = count_runs(cwd);
    vec![
        Line::good(
            "repository",
            if index.is_empty() {
                "no files here (an empty directory)".to_owned()
            } else {
                index.describe()
            },
        ),
        Line::good(
            "validation",
            match project.validate() {
                Some(suggestion) => {
                    format!("{} · {}", project.describe(), suggestion.display())
                }
                None => format!("{} · a run cannot check itself", project.describe()),
            },
        ),
        if !config.agent.audit_log {
            Line::good("audit", "off (agent.audit_log = false)")
        } else {
            match read_entries(&path) {
                Err(source) => Line::bad(
                    "audit",
                    format!("unreadable log at {}: {source}", path.display()),
                ),
                Ok(entries) if entries.is_empty() => Line::good(
                    "audit",
                    format!("ready at {}, nothing recorded yet", path.display()),
                ),
                Ok(entries) => Line::good(
                    "audit",
                    format!(
                        "{} recorded decision(s) in {}",
                        entries.len(),
                        path.display()
                    ),
                ),
            }
        },
        Line::good(
            "runs",
            match runs {
                0 => "none recorded here yet".to_owned(),
                1 => "1 recorded run".to_owned(),
                n => format!("{n} recorded runs"),
            },
        ),
        // What the next run will already know. Memory is the one record a rollback
        // deliberately leaves where it put it, so a person can see it here.
        Line::good(
            "memory",
            match agent_tools::notes_kept(&cwd.join(agent_tools::MEMORY_FILE)) {
                0 => "nothing remembered yet".to_owned(),
                1 => "1 note for the next run".to_owned(),
                n => format!("{n} notes for the next run"),
            },
        ),
    ]
}

fn machine_line() -> Line {
    let machine = smollm_hardware::detect();
    Line::good(
        "machine",
        format!(
            "{} {} · {} cores · {:.0} GiB RAM ({:.0} free) · {} · {:.0} GiB free on disk",
            machine.platform.label(),
            machine.arch,
            machine.logical_cores,
            machine.total_ram_gb,
            machine.available_ram_gb,
            machine.gpu_name,
            machine.disk_free_gb,
        ),
    )
}

async fn cmd_task(
    cwd: &Path,
    json: bool,
    prompt: &str,
    yes: bool,
    dry_run: bool,
    approval_mode: Option<ApprovalMode>,
) -> Result<(), Failure> {
    let mut loaded = agent_config::load(cwd)?;
    if let Some(mode) = approval_mode {
        loaded.config.agent.approval_mode = mode;
    }
    if dry_run {
        loaded.config.agent.approval_mode = ApprovalMode::SuggestOnly;
    }
    let config = loaded.config;
    print_warnings(&loaded.warnings);

    let (name, provider_config) = config
        .resolve_provider()
        .map_err(|problems| Failure::usage(render(problems)))?;
    let provider = agent_providers::build(name, provider_config)
        .map_err(|source| Failure::usage(format!("provider {name}: {source}")))?;

    let policy = Policy::from_config(cwd, &config);
    let registry = Registry::with_defaults().enabled_by(&policy);
    // The price of the tool list is what a person trimming it needs to see, so
    // it is measured here rather than left to `smoll tools`.
    let offered = registry.len();
    let tool_tokens = registry.token_cost();
    let mut ctx = Context::from_config(cwd, &config);
    if config.agent.audit_log {
        ctx = ctx
            .with_audit(AuditLog::in_workspace(cwd).map_err(|source| {
                Failure::usage(format!("cannot open the audit log: {source}"))
            })?);
    }

    let mut agent = Agent::configured(provider, registry, &config, ctx);
    // `--yes` replaces the approver; an autonomous-safe mode already came with
    // one, and swapping it for a terminal would start asking questions the
    // configuration has already answered.
    agent = if yes {
        agent.autoapprove()
    } else {
        match config.agent.approval_mode {
            ApprovalMode::AutonomousSafe => agent,
            _ => agent.with_approver(Arc::new(Terminal::new())),
        }
    };
    let mut agent = agent.with_observer(Arc::new(Printer::new(json)));

    let Run { session, error } = agent.run(prompt).await;
    // The record is written whether or not the run finished: a run stopped by
    // its step budget may already have written files, and this file is the only
    // thing that knows what they used to contain.
    let stored = StoredSession::new(session, agent.context().snapshots());
    let path = stored.write(cwd)?;
    let code = code_for(&error, stored.session.finished());

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&answer(&stored, &error, &path))
                .expect("a value built here")
        );
    } else {
        println!("{}", stored.session.recap());
        println!("record    {}", path.display());
    }
    if let Some(error) = &error {
        eprintln!("error: {error}");
        if matches!(error, AgentError::BudgetExhausted(Budget::Tokens { .. })) {
            eprintln!(
                "hint: the {offered} tool definitions cost about {tool_tokens} tokens of every \
                 step's prompt, and they are the part a person can trade away — `smoll tools` \
                 prices each one, and `[tools.<name>] enabled = false` in smoll.toml is what \
                 buys room for a smaller model."
            );
        }
    }
    if code == 0 {
        Ok(())
    } else {
        Err(Failure::exit(code))
    }
}

/// The one document `smoll task --json` leaves on stdout.
fn answer(stored: &StoredSession, error: &Option<AgentError>, path: &Path) -> serde_json::Value {
    let session = &stored.session;
    let outcome: Option<&Outcome> = session.finished();
    let warnings: Vec<String> = session
        .events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::Warning { message } => Some(message.clone()),
            _ => None,
        })
        .collect();
    json!({
        "id": session.id.to_string(),
        "task": session.task,
        "outcome": outcome.map(|outcome| outcome.summary()),
        "kind": outcome.map(outcome_kind),
        "succeeded": outcome.is_some_and(Outcome::succeeded) && error.is_none(),
        "stoppedBy": error.as_ref().map(|error| error.to_string()),
        "files": session.files_involved(),
        "touched": session
            .touched
            .iter()
            .map(|file| file.to_string_lossy().into_owned())
            .collect::<Vec<_>>(),
        "restorable": stored
            .restorable()
            .iter()
            .map(|file| file.to_string_lossy().into_owned())
            .collect::<Vec<_>>(),
        "redactions": session.redactions,
        "usage": session.usage,
        "warnings": warnings,
        "plan": session.plan.as_ref().map(|plan| plan.render()),
        "record": path.display().to_string(),
    })
}

fn cmd_review(cwd: &Path, json: bool, id: Option<&str>) -> Result<(), Failure> {
    let stored = read_stored(cwd, id)?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&stored).expect("a type that serialises")
        );
        return Ok(());
    }

    let session = &stored.session;
    println!("id        {}", session.id);
    println!("task      {}", session.task);
    println!("recorded  {}", ago(stored.stored_at_ms));
    match session.finished() {
        Some(outcome) => println!("outcome   {}: {}", outcome_kind(outcome), outcome.summary()),
        None => println!("outcome   the run never said how it ended"),
    }
    if let Some(plan) = &session.plan {
        for (index, line) in plan.render().lines().enumerate() {
            // The goal wears the label and the steps hang under it, so the plan
            // reads as one block rather than eight unrelated rows.
            println!("{:<10}{line}", if index == 0 { "plan" } else { "" });
        }
    }
    println!("tokens    {}", session.usage.total_tokens);
    if session.redactions > 0 {
        println!("masked    {} secret(s) in tool output", session.redactions);
    }
    let files = session.files_involved();
    println!(
        "files     {}",
        if files.is_empty() {
            "none".to_owned()
        } else {
            listed(&files)
        }
    );
    let restorable = stored.restorable();
    println!(
        "undo      {}",
        if restorable.is_empty() {
            "nothing — this run wrote no files".to_owned()
        } else {
            format!("{} (`smoll rollback`)", listed_strings(&restorable))
        }
    );
    for event in &session.events {
        if let AgentEvent::Warning { message } = event {
            println!("warning   {message}");
        }
    }
    Ok(())
}

/// Put a recorded run's writes back. The record is kept and stamped again, so a
/// person can see that the undo happened rather than that the run vanished.
fn cmd_rollback(cwd: &Path, json: bool, id: Option<&str>) -> Result<(), Failure> {
    let mut stored = read_stored(cwd, id)?;
    if stored.journal.is_empty() {
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "id": stored.session.id.to_string(),
                    "restored": [],
                    "leftAlone": [],
                    "note": "this run wrote no files",
                }))
                .expect("a value built here")
            );
            return Ok(());
        }
        println!(
            "run {} wrote no files, so there is nothing to put back.",
            stored.session.id
        );
        return Ok(());
    }

    let restored = stored.restore(cwd);
    let files: Vec<String> = restored
        .iter()
        .flat_map(|entry| entry.paths.iter())
        .map(|path| relative(cwd, path))
        .collect();
    let left_alone: Vec<String> = restored
        .iter()
        .flat_map(|entry| entry.left_alone.iter())
        .map(|path| path.to_string_lossy().into_owned())
        .collect();

    stored.session.record(AgentEvent::RolledBack {
        files: files.iter().map(PathBuf::from).collect(),
    });
    stored.stored_at_ms = now_ms();
    let path = stored.write(cwd)?;

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "id": stored.session.id.to_string(),
                "restored": files,
                "leftAlone": left_alone,
                "record": path.display().to_string(),
            }))
            .expect("a value built here")
        );
    } else {
        for file in &files {
            println!("restored  {file}");
        }
        for file in &left_alone {
            eprintln!("left alone  {file} — it changed after the run wrote it");
        }
        println!(
            "{} file(s) put back as run {} left them.",
            files.len(),
            stored.session.id
        );
    }
    Ok(())
}

/// `--id` or the newest run in this workspace.
fn read_stored(cwd: &Path, id: Option<&str>) -> Result<StoredSession, Failure> {
    match id {
        Some(raw) => {
            let uuid = Uuid::parse_str(raw)
                .map_err(|source| Failure::usage(format!("{raw} is not a session id: {source}")))?;
            // A record that is not there is a wrong argument, not a failed run,
            // so it leaves the code a shell uses for usage.
            StoredSession::read(cwd, uuid).map_err(|source| {
                Failure::usage(format!("no run {raw} in this workspace: {source}"))
            })
        }
        None => {
            let latest = StoredSession::latest(cwd)?;
            latest.ok_or_else(|| {
                Failure::usage(format!(
                    "no run recorded in {} yet; `smoll task` is what writes one",
                    cwd.display()
                ))
            })
        }
    }
}

fn count_runs(cwd: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(cwd.join(agent_core::SESSIONS_DIR)) else {
        return 0;
    };
    entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
        .count()
}

/// What a run that stopped on its own leaves behind: 0 for a goal reached or a
/// proposal made, 1 for a run that finished and said the task did not work, and
/// whatever the error's own code is.
fn code_for(error: &Option<AgentError>, outcome: Option<&Outcome>) -> u8 {
    match error {
        Some(error) => error.exit_code(),
        None => match outcome {
            Some(outcome) if outcome.succeeded() => 0,
            Some(Outcome::Cancelled) => 1,
            Some(_) => 1,
            None => 1,
        },
    }
}

fn relative(cwd: &Path, path: &Path) -> String {
    path.strip_prefix(cwd)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}

fn describe_provider(config: &Config) -> String {
    match config.resolve_provider() {
        Ok((name, provider)) => format!("{name} (type {})", provider.kind),
        Err(problems) => render(problems),
    }
}

fn render(problems: Vec<agent_config::Problem>) -> String {
    problems
        .iter()
        .map(|problem| problem.to_string())
        .collect::<Vec<_>>()
        .join("; ")
}

/// A `[tools.<name>]` section for a name this build does not register.
///
/// This is the finding that matters most in the file, and the kind a validator
/// cannot make: `agent-config` does not know the tool list, and a section for an
/// unknown tool parses perfectly. It is also not a setting switched off — it is a
/// setting that does not exist, so a denylist written to keep `rm -rf` out of a
/// run can sit in a file doing nothing while its author believes the opposite.
fn orphaned_tool_names(config: &Config, registry: &Registry) -> Vec<String> {
    let known = registry.names();
    config
        .tools
        .keys()
        .filter(|name| !known.iter().any(|tool| tool == *name))
        .cloned()
        .collect()
}

/// The sentence a person reading their own file needs, including why an unused
/// section is worse than a disabled one.
fn orphan_warning(name: &str) -> String {
    format!(
        "[tools.{name}] gates nothing: no tool this build registers has that name. Check the \
         spelling against `smoll tools`, which lists what a run can be given."
    )
}

fn print_warnings(warnings: &[String]) {
    for warning in warnings {
        eprintln!("warning: {warning}");
    }
}

/// What an approval mode does, in the words a person picking one needs.
fn approval_note(mode: ApprovalMode) -> &'static str {
    match mode {
        ApprovalMode::SuggestOnly => "writes nothing, proposes diffs",
        ApprovalMode::ApproveEdits => "writes are asked about, commands run",
        ApprovalMode::ApproveCommands => "commands are asked about, writes run",
        ApprovalMode::AutonomousSafe => {
            "nothing is asked, the sandbox still refuses what it refuses"
        }
    }
}

fn yes_no(value: bool) -> &'static str {
    if value {
        "on"
    } else {
        "off"
    }
}

/// A description's first sentence: `smoll tools` is a table a person scans, and
/// the whole text is what `--json` carries.
fn first_sentence(text: &str) -> &str {
    match text.find(". ") {
        Some(end) => &text[..end + 1],
        None => text,
    }
}

fn outcome_kind(outcome: &Outcome) -> &'static str {
    match outcome {
        Outcome::Completed { .. } => "completed",
        Outcome::Proposed { .. } => "proposed",
        Outcome::Blocked { .. } => "blocked",
        Outcome::Failed { .. } => "failed",
        Outcome::Cancelled => "cancelled",
    }
}

fn listed(values: &[String]) -> String {
    if values.len() <= 6 {
        return values.join(", ");
    }
    format!("{}, +{} more", values[..6].join(", "), values.len() - 6)
}

fn listed_strings(paths: &[PathBuf]) -> String {
    let names: Vec<String> = paths
        .iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect();
    listed(&names)
}

fn mark(ok: bool) -> &'static str {
    if ok {
        "ok"
    } else {
        "!!"
    }
}

/// A session's age in the units a person scanning `smoll review` reads.
fn ago(at_ms: u64) -> String {
    let secs = now_ms().saturating_sub(at_ms) / 1000;
    let stamp = match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m {}s", secs / 60, secs % 60),
        3600..=86399 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86400),
    };
    format!("{stamp} ago")
}

fn print_usage_hint() -> ExitCode {
    eprintln!("smoll does something per subcommand; run `smoll --help` for the list.");
    ExitCode::from(2)
}

fn init_tracing(verbose: bool) {
    use tracing_subscriber::layer::SubscriberExt as _;
    use tracing_subscriber::util::SubscriberInitExt as _;

    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        tracing_subscriber::EnvFilter::new(if verbose { "info" } else { "warn" })
    });
    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .try_init()
        .ok();
}

// TODO(completions): `smoll --generate-completions bash|zsh|fish` needs
// `clap_complete`, which is not in this workspace's lockfile. A dependency
// added without a network here would break the build for a cosmetic flag, so
// the flag is absent rather than half-present.
