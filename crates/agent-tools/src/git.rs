//! Asking git what it thinks, what it recorded, and — for the two that are
//! writes — what it stages and commits.
//!
//! These six exist as themselves rather than as `run_command git …` because the
//! agent should not have to be told which git flags are safe: the argv is here,
//! written in full, and the model supplies a path, a limit, a revision or a
//! message. That is what lets the four readers be [`Permission::ReadOnly`] and
//! the two writers [`Permission::Write`] — classes a command-derived intent
//! would otherwise raise to `execute` — and it is why none of them can be talked
//! into `git reset --hard` by a persuasive file name.
//!
//! Nothing here reaches the network and nothing here can be made to: no fetch,
//! no push, no remote is ever named, no branch is created or deleted. Pushing
//! stays a thing a person does in their own terminal, which is the whole
//! difference between an agent that commits a run's work and one that publishes
//! it.
//!
//! All six run with `GIT_OPTIONAL_LOCKS=0`, so a read cannot take a lock another
//! process needs. `git_commit` is the one that is not a read of the object store
//! but a write to it, and the one that runs this repository's own hooks, so it
//! gets [longer to answer](COMMIT_TIMEOUT) than a status call.
//!
//! What `smoll rollback` does not do: undo a commit or unstage a file. Those are
//! git's, and this crate names no git command that discards work.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::json;
use tokio::process::Command;

use agent_sandbox::Permission;

use crate::call::{ToolCall, ToolOutput, ToolResult};
use crate::context::Context;
use crate::schema;
use crate::tool::{Intent, Tool, ToolDefinition, DEFAULT_MAX_OUTPUT_BYTES};

/// How long git gets to answer a read. A status call on a huge tree is slow, but
/// a status call that has not answered in half a minute is not going to.
const GIT_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a commit gets. Recording one runs this repository's own hooks, and a
/// hook that checks the code is slower than git itself by design.
const COMMIT_TIMEOUT: Duration = Duration::from_secs(120);

/// The most history `git_log` will bring back, however many it is asked for: a
/// small context cannot hold a project's whole history, and a model that wants
/// more than a screenful is asking the wrong question.
const MAX_HISTORY: usize = 100;
const DEFAULT_HISTORY: usize = 20;

/// `git` with the flags every tool here needs: no pager, no colour a person's
/// `color.ui = always` would have put into the model's prompt, and file names
/// spelled as they are rather than octal-escaped.
fn base(sub: &[&str]) -> Vec<String> {
    let mut argv = own(&[
        "git",
        "--no-pager",
        "-c",
        "core.quotepath=false",
        "-c",
        "color.ui=false",
    ]);
    argv.extend(sub.iter().map(|word| (*word).to_owned()));
    argv
}

pub struct GitStatus;

#[async_trait]
impl Tool for GitStatus {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "git_status",
            "What git thinks of the working tree: the branch line, then one line per changed \
             path (`M` modified, `??` untracked, `A` added, `D` deleted), staged state in the \
             first column. `path` narrows it to one file or directory. Reads only: nothing is \
             fetched, staged, committed or stashed.",
            schema::object(
                json!({
                    "path": schema::path("file or directory to narrow the listing to"),
                }),
                &[],
            ),
            Permission::ReadOnly,
        )
    }

    fn intent(&self, call: &ToolCall, ctx: &Context) -> Result<Intent, String> {
        Ok(Intent::reading(vec![scope(call, ctx)?]))
    }

    async fn run(&self, call: &ToolCall, ctx: &Context) -> ToolResult {
        let scope = match self.intent(call, ctx) {
            Ok(intent) => intent.paths[0].clone(),
            Err(problem) => return failed(call, problem),
        };
        let mut argv = base(&["status", "--short", "--branch"]);
        if let Some(problem) = narrow(&mut argv, ctx, &scope) {
            return failed(call, problem);
        }
        match git(&argv, &ctx.root, GIT_TIMEOUT).await {
            Ok(text) => ToolResult::completed(
                call.clone(),
                Permission::ReadOnly,
                ToolOutput::new(status_text(&text)),
                0,
            ),
            Err(problem) => failed(call, problem),
        }
    }
}

/// The branch line, and then either the changed paths or the sentence that says
/// there are none.
///
/// `--branch` means git never prints nothing, so "clean" has to be read from
/// the absence of rows rather than from an empty string — and the two porcelain
/// columns are the staged-versus-worktree state, so a row is kept exactly as
/// git wrote it.
fn status_text(text: &str) -> String {
    let branch = text
        .lines()
        .find(|line| line.starts_with("## "))
        .unwrap_or("");
    let rows: Vec<&str> = text
        .lines()
        .filter(|line| !line.starts_with("## ") && !line.trim().is_empty())
        .collect();
    if rows.is_empty() {
        return format!("{branch}\nclean: nothing has changed since the last commit");
    }
    if branch.is_empty() {
        rows.join("\n")
    } else {
        format!("{branch}\n{}", rows.join("\n"))
    }
}

/// The diff itself, which is what the agent proposes and what a human approves.
pub struct GitDiff;

#[async_trait]
impl Tool for GitDiff {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "git_diff",
            "The diff of the working tree against the index, or the staged changes when `staged` \
             is true. `path` narrows it to one file or directory; `context_lines` sets the \
             unchanged lines around each hunk (git's default 3). The answer is the diff, not a \
             summary of it. Reads only: the working tree is not touched and nothing is fetched.",
            schema::object(
                json!({
                    "path": schema::path("file or directory to narrow the diff to"),
                    "staged": schema::boolean("diff what is staged for commit instead"),
                    "context_lines": schema::integer("unchanged lines around each hunk; default 3", 0),
                }),
                &[],
            ),
            Permission::ReadOnly,
        )
    }

    fn intent(&self, call: &ToolCall, ctx: &Context) -> Result<Intent, String> {
        Ok(Intent::reading(vec![scope(call, ctx)?]))
    }

    async fn run(&self, call: &ToolCall, ctx: &Context) -> ToolResult {
        let scope = match self.intent(call, ctx) {
            Ok(intent) => intent.paths[0].clone(),
            Err(problem) => return failed(call, problem),
        };
        let mut argv = base(&["diff", "--no-ext-diff", "--no-textconv"]);
        if let Some(lines) = call.usize_arg("context_lines") {
            argv.push(format!("-U{lines}"));
        }
        if call.bool_arg("staged").unwrap_or(false) {
            argv.push("--staged".to_owned());
        }
        if let Some(problem) = narrow(&mut argv, ctx, &scope) {
            return failed(call, problem);
        }
        let empty = if scope == ctx.root {
            "no changes in the working tree".to_owned()
        } else {
            format!("no changes under {}", ctx.relative(&scope))
        };
        let answer = git(&argv, &ctx.root, GIT_TIMEOUT).await;
        report(call, answer, &empty)
    }
}

/// The history, one line per commit.
pub struct GitLog;

#[async_trait]
impl Tool for GitLog {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "git_log",
            "The commits behind the work, one line each, newest first, as `sha date author: \
             subject`. `limit` says how many (default 20, at most 100); `path` narrows it to \
             what touched one file or directory. Reads only, and a range like `main..HEAD` is \
             not accepted — name one revision to git_show.",
            schema::object(
                json!({
                    "limit": schema::integer("how many commits to list; default 20, max 100", 1),
                    "path": schema::path("file or directory whose history to read"),
                }),
                &[],
            ),
            Permission::ReadOnly,
        )
    }

    fn intent(&self, call: &ToolCall, ctx: &Context) -> Result<Intent, String> {
        Ok(Intent::reading(vec![scope(call, ctx)?]))
    }

    async fn run(&self, call: &ToolCall, ctx: &Context) -> ToolResult {
        let scope = match self.intent(call, ctx) {
            Ok(intent) => intent.paths[0].clone(),
            Err(problem) => return failed(call, problem),
        };
        let limit = call
            .usize_arg("limit")
            .unwrap_or(DEFAULT_HISTORY)
            .clamp(1, MAX_HISTORY);
        let mut argv = base(&[
            "log",
            &format!("-n{limit}"),
            "--date=short",
            "--pretty=format:%h %ad %an: %s",
        ]);
        if let Some(problem) = narrow(&mut argv, ctx, &scope) {
            return failed(call, problem);
        }
        match git(&argv, &ctx.root, GIT_TIMEOUT).await {
            Ok(text) => ToolResult::completed(
                call.clone(),
                Permission::ReadOnly,
                ToolOutput::new(history(&text)),
                0,
            ),
            // A young repository is not a broken one: "no commits yet" is the
            // answer, and a model that reads it as a failure wastes a step.
            Err(problem) if no_commits_yet(&problem) => ToolResult::completed(
                call.clone(),
                Permission::ReadOnly,
                ToolOutput::new("no commits yet: nothing has been recorded in this repository"),
                0,
            ),
            Err(problem) => failed(call, problem),
        }
    }
}

/// The listing, with the count the model can plan against, and a pointer at the
/// tool that turns one of these lines into a diff.
fn history(text: &str) -> String {
    let rows: Vec<&str> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    if rows.is_empty() {
        return "no commits here: nothing in this history matches what was asked for".to_owned();
    }
    format!(
        "{}\n[{} commit(s) — git_show with one of these shas for its message and diff]",
        rows.join("\n"),
        rows.len()
    )
}

/// git's own sentence for a repository with no commits, in the one language this
/// crate asks it for.
fn no_commits_yet(problem: &str) -> bool {
    problem.contains("does not have any commits yet")
}

/// One revision, which is where an agent reads *why* something was done.
pub struct GitShow;

#[async_trait]
impl Tool for GitShow {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "git_show",
            "One revision: who made it, its message, and the diff it introduced. `revision` is a \
             sha, a tag or branch name, or `HEAD` with `~2` or `^1`; anything that could read as \
             a flag is refused before git runs. `stat` gives the files and their line counts \
             instead of the hunks — ask for it when a commit is large. Reads only.",
            schema::object(
                json!({
                    "revision": schema::string("the commit to show: a sha, a tag or branch name, or HEAD, HEAD~1, HEAD^"),
                    "stat": schema::boolean("list the changed files and their counts, without the diff"),
                }),
                &["revision"],
            ),
            Permission::ReadOnly,
        )
    }

    fn intent(&self, call: &ToolCall, ctx: &Context) -> Result<Intent, String> {
        // Checked here rather than only in `run`, so a revision that could be
        // read as a flag fails before a person is asked about it. What the call
        // reads is the object store, which is the repository.
        let _ = revision(call)?;
        Ok(Intent::reading(vec![ctx.root.clone()]))
    }

    async fn run(&self, call: &ToolCall, ctx: &Context) -> ToolResult {
        // The revision is checked twice, once here and once in `intent`, because
        // a value that is not a revision must never reach the argv at all.
        let revision = match revision(call) {
            Ok(revision) => revision,
            Err(problem) => return failed(call, problem),
        };
        let mut argv = base(&["show", "--no-ext-diff", "--no-textconv"]);
        if call.bool_arg("stat").unwrap_or(false) {
            argv.push("--stat".to_owned());
        }
        argv.push(revision);
        match git(&argv, &ctx.root, GIT_TIMEOUT).await {
            Ok(text) => {
                ToolResult::completed(call.clone(), Permission::ReadOnly, ToolOutput::new(text), 0)
            }
            Err(problem) => failed(call, problem),
        }
    }
}

/// Stage named files so a commit can record them.
pub struct GitAdd;

#[async_trait]
impl Tool for GitAdd {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "git_add",
            "Stage the named files or directories so `git_commit` can record them. Each path \
             goes to git after `--`, so a file name is never a flag, and contents are not \
             touched. Nothing unmentioned is staged — `.` is refused, so a run cannot sweep the \
             tree into one commit — and a path `.gitignore` hides is refused rather than forced. \
             This writes git's index: an approval mode that asks before writing asks here, and \
             `smoll rollback` does not undo what this staged.",
            schema::object(
                json!({
                    "paths": schema::string_array("the files or directories to stage, at least one"),
                }),
                &["paths"],
            ),
            Permission::Write,
        )
        .with_output_limit(DEFAULT_MAX_OUTPUT_BYTES * 2)
    }

    fn intent(&self, call: &ToolCall, ctx: &Context) -> Result<Intent, String> {
        Ok(Intent::writing(to_stage(call, ctx)?))
    }

    async fn run(&self, call: &ToolCall, ctx: &Context) -> ToolResult {
        let paths = match to_stage(call, ctx) {
            Ok(paths) => paths,
            Err(problem) => return write_problem(call, problem),
        };
        let mut argv = base(&["add", "--verbose", "--"]);
        for path in &paths {
            argv.push(ctx.relative(path));
        }
        match git(&argv, &ctx.root, GIT_TIMEOUT).await {
            Ok(text) => ToolResult::completed(
                call.clone(),
                Permission::Write,
                ToolOutput::new(staged(&text)),
                0,
            ),
            Err(problem) => write_problem(call, problem),
        }
    }
}

/// Record what is staged as one commit.
pub struct GitCommit;

#[async_trait]
impl Tool for GitCommit {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "git_commit",
            "Record what `git_add` has staged as one commit, with `message` as its message. Only \
             what is already staged goes in: nothing is added on the side, no commit is amended, \
             and no hook is skipped, so this repository's own pre-commit checks get to refuse it. \
             Nothing leaves the machine — there is no push, no remote and no branch here. An \
             approval mode that asks before writing asks here, and `smoll rollback` does not undo \
             a commit.",
            schema::object(
                json!({
                    "message": schema::string("the commit message, one argument: what changed and why"),
                }),
                &["message"],
            ),
            Permission::Write,
        )
        .with_timeout(COMMIT_TIMEOUT)
    }

    fn intent(&self, call: &ToolCall, ctx: &Context) -> Result<Intent, String> {
        let _ = message(call)?;
        // What a commit changes is the repository, not one file, so that is the
        // path the policy gets: a workspace that reaches outside itself has no
        // repository to commit to.
        Ok(Intent::writing(vec![ctx.root.clone()]))
    }

    async fn run(&self, call: &ToolCall, ctx: &Context) -> ToolResult {
        let message = match message(call) {
            Ok(message) => message,
            Err(problem) => return write_problem(call, problem),
        };
        // Checked before git is asked, because an empty commit is git's own
        // silence on stdout: the model needs the name of the tool that fixes it.
        let staged = match git(
            &base(&["diff", "--cached", "--name-only"]),
            &ctx.root,
            GIT_TIMEOUT,
        )
        .await
        {
            Ok(paths) => paths,
            Err(problem) => return write_problem(call, problem),
        };
        let records: Vec<&str> = staged
            .lines()
            .filter(|line| !line.trim().is_empty())
            .collect();
        if records.is_empty() {
            return write_problem(
                call,
                "nothing is staged, so there is nothing to commit. Call git_add with the paths \
                 this run changed first"
                    .to_owned(),
            );
        }
        let argv = base(&["commit", "--message", &message]);
        match git(&argv, &ctx.root, COMMIT_TIMEOUT).await {
            Ok(text) => ToolResult::completed(
                call.clone(),
                Permission::Write,
                ToolOutput::new(format!(
                    "{}\n[{} file(s) committed: {}]",
                    text.trim_end(),
                    records.len(),
                    records.join(", ")
                )),
                0,
            ),
            Err(problem) => write_problem(call, problem),
        }
    }
}

/// The paths a model asked to have staged, each one resolved inside the
/// workspace and none of them the workspace itself.
fn to_stage(call: &ToolCall, ctx: &Context) -> Result<Vec<PathBuf>, String> {
    let given = call.str_list_arg("paths").unwrap_or_default();
    if given.is_empty() {
        return Err("git_add needs a non-empty \"paths\" list of what to stage".to_owned());
    }
    let mut paths = Vec::with_capacity(given.len());
    for raw in given {
        let path = ctx.resolve(&raw)?;
        // `git add .` is what a person types when they mean it. A run that has
        // changed three files and stages everything would commit whatever else
        // was lying in the tree, so the sweep is refused rather than trusted.
        if path == ctx.root {
            return Err(format!(
                "git_add names what it stages: {raw} would stage the whole repository. Name the \
                 files this run changed."
            ));
        }
        if path.starts_with(ctx.root.join(".git")) {
            return Err(format!(
                "{} is inside git's own record of the repository, which this tool does not stage",
                ctx.relative(&path)
            ));
        }
        paths.push(path);
    }
    Ok(paths)
}

/// git's own words for what it staged: `add 'file'` per path, and a count the
/// model can hold against the commit it is about to make.
fn staged(text: &str) -> String {
    let rows: Vec<&str> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    if rows.is_empty() {
        return "[nothing new was staged: these paths already match what is in the index]"
            .to_owned();
    }
    format!(
        "{}\n[{} file(s) staged — git_diff with staged = true shows what they change, \
         git_commit records them]",
        rows.join("\n"),
        rows.len()
    )
}

/// One revision, checked to be a revision rather than a flag.
///
/// It has to go to git as its own positional argument — `git show -- <rev>`
/// would make it a pathspec — so the only thing between the model and the argv
/// is this check. A sha, a tag or branch name, and the `~`/`^` steps off `HEAD`
/// are all this will pass; anything beginning with `-`, anything with an `=` in
/// it, and anything with a space are refused.
fn revision(call: &ToolCall) -> Result<String, String> {
    let given = call.str_arg("revision").unwrap_or_default();
    let given = given.trim();
    if given.is_empty() {
        return Err(
            "git_show needs a \"revision\": a commit sha, a tag or branch name, or HEAD".to_owned(),
        );
    }
    let shape = given.len() <= 64
        && !given.starts_with('-')
        && given.chars().all(|character| {
            character.is_ascii_alphanumeric()
                || matches!(character, '.' | '_' | '/' | '^' | '~' | '@' | '-')
        });
    if !shape {
        return Err(format!(
            "{given} is not a revision this tool will hand to git. Name one commit — a sha, a \
             tag or branch name, or HEAD, HEAD~1, HEAD^ — rather than a range or an option."
        ));
    }
    Ok(given.to_owned())
}

/// The message, which is the whole of what a model can put into a commit.
fn message(call: &ToolCall) -> Result<String, String> {
    let given = call.str_arg("message").unwrap_or_default();
    if given.trim().is_empty() {
        return Err("git_commit needs a \"message\" that says what the commit records".to_owned());
    }
    Ok(given)
}

/// The directory or file the model named, defaulting to the whole repository.
fn scope(call: &ToolCall, ctx: &Context) -> Result<PathBuf, String> {
    let raw = call.str_arg("path").unwrap_or_else(|| ".".to_owned());
    ctx.resolve(&raw)
}

/// Append `-- <relative path>` unless the scope is the whole repository.
///
/// The path goes after `--` and as its own argument, so a file named
/// `--help` or `; rm -rf` is a file name rather than a flag or a command line.
fn narrow(argv: &mut Vec<String>, ctx: &Context, scope: &PathBuf) -> Option<String> {
    if scope == &ctx.root {
        return None;
    }
    match scope.strip_prefix(&ctx.root) {
        Ok(relative) if !relative.as_os_str().is_empty() => {
            argv.push("--".to_owned());
            argv.push(relative.to_string_lossy().into_owned());
            None
        }
        _ => Some(format!(
            "{} is not inside the repository at {}",
            scope.display(),
            ctx.root.display()
        )),
    }
}

/// An empty answer means "nothing changed", which is a result rather than a
/// failure — the difference between a clean tree and a tool that did not work.
fn report(call: &ToolCall, answer: Result<String, String>, when_empty: &str) -> ToolResult {
    match answer {
        Ok(text) if text.trim().is_empty() => ToolResult::completed(
            call.clone(),
            Permission::ReadOnly,
            ToolOutput::new(when_empty),
            0,
        ),
        Ok(text) => {
            ToolResult::completed(call.clone(), Permission::ReadOnly, ToolOutput::new(text), 0)
        }
        Err(problem) => failed(call, problem),
    }
}

/// Run one git command, with no shell, no stdin and no network, from the
/// workspace root. A non-zero exit is not a crash: it becomes the message the
/// model reads, in git's own words.
async fn git(argv: &[String], cwd: &PathBuf, limit: Duration) -> Result<String, String> {
    let program = argv.first().ok_or_else(|| "no program to run".to_owned())?;
    let spawned = Command::new(program)
        .args(&argv[1..])
        .current_dir(cwd)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        // One language for the messages, whatever the terminal's is: a model
        // that reads "не найден git репозиторий" cannot act on it, and this
        // crate's own tests assert on those sentences. `core.quotepath=false`
        // keeps a non-ASCII file name readable instead of octal-escaped.
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn();
    let output = match spawned {
        Ok(child) => tokio::time::timeout(limit, child.wait_with_output())
            .await
            .map_err(|_| format!("{} did not answer within {}s", program, limit.as_secs()))?
            .map_err(|source| format!("cannot run {}: {source}", argv.join(" ")))?,
        Err(source) => {
            return Err(format!(
                "cannot run {}: {source}. Is git installed and on PATH?",
                argv.join(" ")
            ));
        }
    };
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    if output.status.success() {
        return Ok(stdout);
    }
    let complaint = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    if complaint.contains("not a git repository") {
        return Err(format!(
            "{} is not a git repository, so git has nothing to report",
            cwd.display()
        ));
    }
    let code = output
        .status
        .code()
        .map(|code| code.to_string())
        .unwrap_or_else(|| "on a signal".to_owned());
    Err(format!(
        "{} exited {code}{}",
        argv.join(" "),
        if complaint.is_empty() {
            String::new()
        } else {
            format!(": {complaint}")
        }
    ))
}

fn own(words: &[&str]) -> Vec<String> {
    words.iter().map(|word| (*word).to_owned()).collect()
}

fn failed(call: &ToolCall, message: String) -> ToolResult {
    ToolResult::failed(
        call.clone(),
        Permission::ReadOnly,
        message,
        ToolOutput::empty(),
    )
}

/// The same for the two tools that change the repository: a failure they report
/// carries the write class, because that is what the run was trying to do.
fn write_problem(call: &ToolCall, message: String) -> ToolResult {
    ToolResult::failed(
        call.clone(),
        Permission::Write,
        message,
        ToolOutput::empty(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git_in(dir: &std::path::Path, args: &[&str]) -> bool {
        std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }

    /// A real repository, because a git tool tested against a mock git proves
    /// nothing about the flags it passes. `None` means this machine has no git,
    /// which is a reason to skip rather than to fail.
    fn repo(files: &[(&str, &str)]) -> Option<tempfile::TempDir> {
        let dir = tempfile::tempdir().expect("a temp dir");
        if !git_in(dir.path(), &["--version"]) {
            return None;
        }
        assert!(git_in(dir.path(), &["init", "-q", "."]));
        assert!(git_in(
            dir.path(),
            &["config", "user.email", "agent@example.invalid"]
        ));
        assert!(git_in(dir.path(), &["config", "user.name", "Agent"]));
        for (name, contents) in files {
            std::fs::write(dir.path().join(name), contents).expect("writable");
            // After a `--`, so a file called `--help` is a file name.
            assert!(git_in(dir.path(), &["add", "--", name]));
        }
        assert!(git_in(dir.path(), &["commit", "-q", "-m", "start"]));
        Some(dir)
    }

    fn context(dir: &tempfile::TempDir) -> Context {
        Context::new(dir.path().to_path_buf())
    }

    #[tokio::test]
    async fn a_modified_and_an_untracked_file_are_both_listed() {
        let Some(dir) = repo(&[("tracked.txt", "one\n"), ("second.txt", "same\n")]) else {
            return;
        };
        std::fs::write(dir.path().join("tracked.txt"), "two\n").expect("writable");
        std::fs::write(dir.path().join("untracked.txt"), "new\n").expect("writable");
        let ctx = context(&dir);
        let result = GitStatus
            .run(&ToolCall::new("git_status", json!({})), &ctx)
            .await;
        assert!(result.is_success(), "{}", result.summary());
        let text = result.output.text;
        assert!(
            text.contains("M  tracked.txt") || text.contains(" M tracked.txt"),
            "{text}"
        );
        assert!(text.contains("?? untracked.txt"), "{text}");
        assert!(text.starts_with("## "), "{text}");
    }

    #[tokio::test]
    async fn a_clean_tree_is_an_answer_not_a_failure() {
        let Some(dir) = repo(&[("tracked.txt", "one\n")]) else {
            return;
        };
        let ctx = context(&dir);
        let result = GitStatus
            .run(&ToolCall::new("git_status", json!({})), &ctx)
            .await;
        assert!(result.is_success(), "{}", result.summary());
        assert!(
            result.output.text.contains("clean"),
            "{}",
            result.output.text
        );
    }

    #[tokio::test]
    async fn a_scope_limits_the_listing_to_one_path() {
        let Some(dir) = repo(&[("tracked.txt", "one\n")]) else {
            return;
        };
        std::fs::write(dir.path().join("tracked.txt"), "two\n").expect("writable");
        std::fs::write(dir.path().join("other.txt"), "new\n").expect("writable");
        let ctx = context(&dir);
        let result = GitStatus
            .run(
                &ToolCall::new("git_status", json!({"path": "tracked.txt"})),
                &ctx,
            )
            .await;
        let text = result.output.text;
        assert!(text.contains("tracked.txt"), "{text}");
        assert!(!text.contains("other.txt"), "{text}");
    }

    #[tokio::test]
    async fn a_diff_shows_the_change_that_was_made() {
        let Some(dir) = repo(&[("tracked.txt", "one\n")]) else {
            return;
        };
        std::fs::write(dir.path().join("tracked.txt"), "two\n").expect("writable");
        let ctx = context(&dir);
        let result = GitDiff
            .run(
                &ToolCall::new("git_diff", json!({"path": "tracked.txt"})),
                &ctx,
            )
            .await;
        assert!(result.is_success(), "{}", result.summary());
        let text = result.output.text;
        assert!(text.contains("-one"), "{text}");
        assert!(text.contains("+two"), "{text}");
        assert!(text.contains("b/tracked.txt"), "{text}");
    }

    #[tokio::test]
    async fn staged_and_unstaged_are_separate_questions() {
        let Some(dir) = repo(&[("tracked.txt", "one\n")]) else {
            return;
        };
        std::fs::write(dir.path().join("tracked.txt"), "two\n").expect("writable");
        assert!(git_in(dir.path(), &["add", "tracked.txt"]));
        let ctx = context(&dir);

        let staged = GitDiff
            .run(&ToolCall::new("git_diff", json!({"staged": true})), &ctx)
            .await;
        assert!(
            staged.output.text.contains("+two"),
            "{}",
            staged.output.text
        );

        let unstaged = GitDiff
            .run(&ToolCall::new("git_diff", json!({})), &ctx)
            .await;
        assert!(
            unstaged.output.text.contains("no changes"),
            "{}",
            unstaged.output.text
        );
    }

    #[tokio::test]
    async fn context_lines_are_the_models_to_ask_for() {
        let lines = "a\nb\nc\nd\nE\nf\ng\nh\ni\n";
        let Some(dir) = repo(&[("many.txt", lines)]) else {
            return;
        };
        std::fs::write(dir.path().join("many.txt"), lines.replace('E', "e")).expect("writable");
        let ctx = context(&dir);

        let tight = GitDiff
            .run(
                &ToolCall::new("git_diff", json!({"path": "many.txt", "context_lines": 1})),
                &ctx,
            )
            .await;
        let text = tight.output.text;
        assert!(text.contains("-E"), "{text}");
        assert!(text.contains("+e"), "{text}");
        // Only the body rows carry context; a `@@ … @@ c` suffix is git's own
        // hint about where the hunk lives, not a line of the file.
        let shown: Vec<&str> = text.lines().filter(|line| line.starts_with(' ')).collect();
        assert!(shown.contains(&" d"), "one context line is d: {text}");
        assert!(shown.contains(&" f"), "{text}");
        assert!(
            !shown.contains(&" c"),
            "one context line does not reach c: {text}"
        );

        let wide = GitDiff
            .run(
                &ToolCall::new("git_diff", json!({"path": "many.txt", "context_lines": 3})),
                &ctx,
            )
            .await;
        let shown: Vec<&str> = wide
            .output
            .text
            .lines()
            .filter(|line| line.starts_with(' '))
            .collect();
        assert!(shown.contains(&" c"), "{}", wide.output.text);
    }

    #[tokio::test]
    async fn a_directory_that_is_not_a_repository_says_so() {
        let dir = tempfile::tempdir().expect("a temp dir");
        std::fs::write(dir.path().join("loose.txt"), "x").expect("writable");
        let ctx = context(&dir);
        let result = GitStatus
            .run(&ToolCall::new("git_status", json!({})), &ctx)
            .await;
        let summary = result.summary();
        if git_in(dir.path(), &["--version"]) {
            assert!(summary.contains("not a git repository"), "{summary}");
        } else {
            assert!(summary.contains("cannot run git"), "{summary}");
        }
    }

    #[tokio::test]
    async fn a_file_name_cannot_become_a_flag() {
        let Some(dir) = repo(&[("--help", "one\n")]) else {
            return;
        };
        std::fs::write(dir.path().join("--help"), "two\n").expect("writable");
        let ctx = context(&dir);
        let result = GitDiff
            .run(&ToolCall::new("git_diff", json!({"path": "--help"})), &ctx)
            .await;
        assert!(result.is_success(), "{}", result.summary());
        assert!(
            result.output.text.contains("+two"),
            "{}",
            result.output.text
        );
    }

    #[test]
    fn the_four_read_tools_are_reads_and_never_carry_a_command_in_their_intent() {
        let ctx = Context::new(PathBuf::from("/repo"));
        for tool in [
            Box::new(GitStatus) as Box<dyn Tool>,
            Box::new(GitDiff),
            Box::new(GitLog),
            Box::new(GitShow),
        ] {
            let definition = tool.definition();
            assert_eq!(definition.permission, Permission::ReadOnly);
            assert!(
                definition.description.contains("Reads only"),
                "{} says what it does not do",
                definition.name
            );
            let mut arguments = json!({});
            if definition.name == "git_show" {
                arguments["revision"] = json!("HEAD");
            }
            let intent = tool
                .intent(&ToolCall::new(definition.name.clone(), arguments), &ctx)
                .expect("a read of the whole repository needs no argument");
            assert!(
                intent.command.is_none(),
                "the argv is not the model's to write"
            );
            assert_eq!(intent.paths, vec![PathBuf::from("/repo")]);
            assert_eq!(
                intent.permission(Permission::ReadOnly),
                Permission::ReadOnly,
                "a git read stays a read"
            );
        }
    }

    /// The two tools that change what git has recorded declare themselves as
    /// writes rather than as commands, which is what puts them in front of a
    /// person in `approve-edits` and outside a run in `suggest-only`.
    #[test]
    fn the_two_write_tools_are_writes_that_ask_as_writes_do() {
        let ctx = Context::new(PathBuf::from("/repo"));
        for (tool, arguments) in [
            (
                Box::new(GitAdd) as Box<dyn Tool>,
                json!({"paths": ["src/lib.rs"]}),
            ),
            (Box::new(GitCommit), json!({"message": "record the change"})),
        ] {
            let definition = tool.definition();
            assert_eq!(definition.permission, Permission::Write);
            assert!(
                definition.description.contains("approval mode"),
                "{} says who has to say yes",
                definition.name
            );
            assert!(
                definition.description.contains("does not undo"),
                "{} says that a rollback is not the way back",
                definition.name
            );
            let intent = tool
                .intent(&ToolCall::new(definition.name.clone(), arguments), &ctx)
                .expect("a write names its paths");
            assert!(
                intent.command.is_none(),
                "a git write stays a write, not a command an approval mode waves through"
            );
            assert_eq!(intent.permission(Permission::Write), Permission::Write);
        }
    }

    /// Nothing in this crate may reach a remote, and the description is the only
    /// place a model reads that promise, so a word about one has to arrive with
    /// the refusal attached.
    #[test]
    fn no_git_tool_can_be_talked_into_the_network() {
        for tool in [
            Box::new(GitStatus) as Box<dyn Tool>,
            Box::new(GitDiff),
            Box::new(GitLog),
            Box::new(GitShow),
            Box::new(GitAdd),
            Box::new(GitCommit),
        ] {
            let definition = tool.definition();
            let words = definition.description.to_lowercase();
            for forbidden in ["push", "remote", "clone"] {
                if words.contains(forbidden) {
                    assert!(
                        words.contains(&format!("no {forbidden}")),
                        "{} mentions {forbidden} without ruling it out",
                        definition.name
                    );
                }
            }
            assert!(
                !words.contains("--hard") && !words.contains("reset"),
                "{} names a command that discards work",
                definition.name
            );
        }
    }

    #[tokio::test]
    async fn a_recent_history_comes_back_newest_first() {
        let Some(dir) = repo(&[("tracked.txt", "one\n")]) else {
            return;
        };
        for (text, message) in [("two\n", "second"), ("three\n", "third")] {
            std::fs::write(dir.path().join("tracked.txt"), text).expect("writable");
            assert!(git_in(dir.path(), &["add", "tracked.txt"]));
            assert!(git_in(dir.path(), &["commit", "-q", "-m", message]));
        }
        let ctx = context(&dir);
        let result = GitLog.run(&ToolCall::new("git_log", json!({})), &ctx).await;
        assert!(result.is_success(), "{}", result.summary());
        let text = result.output.text;
        let third = text.find("third").expect("the newest subject");
        let second = text.find("second").expect("the one before it");
        assert!(third < second, "newest first: {text}");
        assert!(
            text.contains("start"),
            "and the commit that made it: {text}"
        );
        assert!(text.contains("[3 commit(s)"), "{text}");
    }

    #[tokio::test]
    async fn a_history_that_is_asked_for_shortly_is_short() {
        let Some(dir) = repo(&[("tracked.txt", "one\n")]) else {
            return;
        };
        for (text, message) in [("two\n", "second"), ("three\n", "third")] {
            std::fs::write(dir.path().join("tracked.txt"), text).expect("writable");
            assert!(git_in(dir.path(), &["add", "tracked.txt"]));
            assert!(git_in(dir.path(), &["commit", "-q", "-m", message]));
        }
        let ctx = context(&dir);
        let result = GitLog
            .run(&ToolCall::new("git_log", json!({"limit": 1})), &ctx)
            .await;
        let text = result.output.text;
        assert!(text.contains("third"), "{text}");
        assert!(!text.contains("second"), "one commit means one: {text}");
    }

    #[tokio::test]
    async fn a_history_of_one_file_narrows_to_that_file() {
        let Some(dir) = repo(&[("first.txt", "one\n")]) else {
            return;
        };
        std::fs::write(dir.path().join("second.txt"), "new\n").expect("writable");
        assert!(git_in(dir.path(), &["add", "second.txt"]));
        assert!(git_in(
            dir.path(),
            &["commit", "-q", "-m", "added the second"]
        ));
        let ctx = context(&dir);
        let result = GitLog
            .run(
                &ToolCall::new("git_log", json!({"path": "first.txt"})),
                &ctx,
            )
            .await;
        let text = result.output.text;
        assert!(text.contains("start"), "{text}");
        assert!(!text.contains("added the second"), "{text}");
    }

    #[tokio::test]
    async fn a_repository_with_no_commits_answers_rather_than_failing() {
        let empty = tempfile::tempdir().expect("a temp dir");
        if !git_in(empty.path(), &["--version"]) {
            return;
        }
        assert!(git_in(empty.path(), &["init", "-q", "."]));
        let ctx = Context::new(empty.path().to_path_buf());
        let result = GitLog.run(&ToolCall::new("git_log", json!({})), &ctx).await;
        assert!(
            result.is_success(),
            "no history is an answer: {}",
            result.summary()
        );
        assert!(
            result.output.text.contains("no commits yet"),
            "{}",
            result.output.text
        );
    }

    #[tokio::test]
    async fn a_commit_is_shown_with_its_message_and_its_change() {
        let Some(dir) = repo(&[("tracked.txt", "one\n")]) else {
            return;
        };
        let ctx = context(&dir);
        let result = GitShow
            .run(
                &ToolCall::new("git_show", json!({"revision": "HEAD"})),
                &ctx,
            )
            .await;
        assert!(result.is_success(), "{}", result.summary());
        let text = result.output.text;
        assert!(text.contains("start"), "the message: {text}");
        assert!(text.contains("+one"), "the diff: {text}");
        assert!(text.contains("Author:"), "who: {text}");
    }

    #[tokio::test]
    async fn a_show_can_list_the_files_without_their_hunks() {
        let Some(dir) = repo(&[("tracked.txt", "one\n")]) else {
            return;
        };
        let ctx = context(&dir);
        let result = GitShow
            .run(
                &ToolCall::new("git_show", json!({"revision": "HEAD", "stat": true})),
                &ctx,
            )
            .await;
        assert!(result.is_success(), "{}", result.summary());
        let text = result.output.text;
        assert!(text.contains("tracked.txt"), "{text}");
        assert!(text.contains("file changed"), "{text}");
        assert!(!text.contains("+one"), "no hunks asked for: {text}");
    }

    #[tokio::test]
    async fn a_revision_that_could_be_read_as_a_flag_is_refused() {
        let Some(dir) = repo(&[("tracked.txt", "one\n")]) else {
            return;
        };
        let ctx = context(&dir);
        for bad in ["--help", "-p", "--pretty=format:%H", "HEAD --stat", "a=1"] {
            let result = GitShow
                .run(&ToolCall::new("git_show", json!({"revision": bad})), &ctx)
                .await;
            assert!(!result.is_success(), "{bad} was accepted");
            assert!(
                result.summary().contains("not a revision"),
                "{bad}: {}",
                result.summary()
            );
        }
        // The steps a person actually types are still allowed.
        for good in ["HEAD", "HEAD~1", "HEAD^", "start-tag_1.0"] {
            let intent = GitShow
                .intent(&ToolCall::new("git_show", json!({"revision": good})), &ctx)
                .unwrap_or_else(|problem| panic!("{good} refused: {problem}"));
            assert_eq!(intent.paths, vec![dir.path().to_path_buf()]);
        }
    }

    #[tokio::test]
    async fn staging_then_committing_records_the_run() {
        let Some(dir) = repo(&[("tracked.txt", "one\n")]) else {
            return;
        };
        std::fs::write(dir.path().join("tracked.txt"), "two\n").expect("writable");
        std::fs::write(dir.path().join("new.rs"), "fn new() {}\n").expect("writable");
        let ctx = context(&dir);

        let added = GitAdd
            .run(
                &ToolCall::new("git_add", json!({"paths": ["tracked.txt", "new.rs"]})),
                &ctx,
            )
            .await;
        assert!(added.is_success(), "{}", added.summary());
        assert!(
            added.output.text.contains("new.rs"),
            "{}",
            added.output.text
        );
        assert!(
            added.output.text.contains("2 file(s) staged"),
            "{}",
            added.output.text
        );

        let committed = GitCommit
            .run(
                &ToolCall::new("git_commit", json!({"message": "record both"})),
                &ctx,
            )
            .await;
        assert!(committed.is_success(), "{}", committed.summary());
        assert!(
            committed.output.text.contains("2 file(s) committed"),
            "{}",
            committed.output.text
        );

        let history = GitLog
            .run(&ToolCall::new("git_log", json!({"limit": 1})), &ctx)
            .await;
        assert!(
            history.output.text.contains("record both"),
            "{}",
            history.output.text
        );
    }

    #[tokio::test]
    async fn a_commit_of_nothing_names_the_tool_that_fixes_it() {
        let Some(dir) = repo(&[("tracked.txt", "one\n")]) else {
            return;
        };
        std::fs::write(dir.path().join("tracked.txt"), "two\n").expect("writable");
        let ctx = context(&dir);
        let result = GitCommit
            .run(
                &ToolCall::new("git_commit", json!({"message": "premature"})),
                &ctx,
            )
            .await;
        assert!(!result.is_success(), "{result:?}");
        let summary = result.summary();
        assert!(summary.contains("nothing is staged"), "{summary}");
        assert!(summary.contains("git_add"), "{summary}");
        // And the work is still there, unstaged, for the run to put right.
        assert!(
            !git_in(
                dir.path(),
                &["diff", "--quiet", "HEAD", "--", "tracked.txt"]
            ),
            "the change should still be in the tree"
        );
    }

    /// The sweep is the failure mode this tool exists to prevent: a run that
    /// changed three files and stages everything commits whatever else was
    /// lying in the tree, including a person's unfinished work.
    #[tokio::test]
    async fn the_whole_repository_is_not_a_path_to_stage() {
        let Some(dir) = repo(&[("tracked.txt", "one\n")]) else {
            return;
        };
        let ctx = context(&dir);
        for spelling in [".", "./", "crates/.."] {
            let result = GitAdd
                .run(
                    &ToolCall::new("git_add", json!({"paths": [spelling]})),
                    &ctx,
                )
                .await;
            assert!(!result.is_success(), "{spelling:?} staged the tree");
            assert!(
                result.summary().contains("the whole repository"),
                "{spelling:?}: {}",
                result.summary()
            );
        }
        for arguments in [json!([]), json!({"paths": []}), json!({"paths": null})] {
            let result = GitAdd
                .run(&ToolCall::new("git_add", arguments.clone()), &ctx)
                .await;
            assert!(!result.is_success(), "{arguments:?} was accepted");
            assert!(
                result.summary().contains("non-empty"),
                "{arguments:?}: {}",
                result.summary()
            );
        }
    }

    #[tokio::test]
    async fn nothing_inside_the_repositories_own_record_is_staged() {
        let Some(dir) = repo(&[("tracked.txt", "one\n")]) else {
            return;
        };
        let ctx = context(&dir);
        let result = GitAdd
            .run(
                &ToolCall::new("git_add", json!({"paths": [".git/config"]})),
                &ctx,
            )
            .await;
        assert!(!result.is_success(), "{result:?}");
        assert!(
            result.summary().contains("git's own record"),
            "{}",
            result.summary()
        );
        // A file whose name merely starts the same way is a normal file.
        std::fs::write(dir.path().join(".gitignore"), "build/\n").expect("writable");
        let ignored = GitAdd
            .run(
                &ToolCall::new("git_add", json!({"paths": [".gitignore"]})),
                &ctx,
            )
            .await;
        assert!(ignored.is_success(), "{}", ignored.summary());
    }

    /// A path `.gitignore` hides is git's own refusal, and this tool does not
    /// have the `--force` that would talk it out of one.
    #[tokio::test]
    async fn an_ignored_path_is_refused_rather_than_forced() {
        let Some(dir) = repo(&[("tracked.txt", "one\n")]) else {
            return;
        };
        std::fs::write(dir.path().join(".gitignore"), "hidden.txt\n").expect("writable");
        assert!(git_in(dir.path(), &["add", ".gitignore"]));
        assert!(git_in(
            dir.path(),
            &["commit", "-q", "-m", "ignore the scratch"]
        ));
        std::fs::write(dir.path().join("hidden.txt"), "scratch\n").expect("writable");
        let ctx = context(&dir);
        let result = GitAdd
            .run(
                &ToolCall::new("git_add", json!({"paths": ["hidden.txt"]})),
                &ctx,
            )
            .await;
        assert!(!result.is_success(), "{result:?}");
        assert!(
            result.summary().contains("ignored"),
            "git's own words: {}",
            result.summary()
        );
        assert!(!result.summary().contains("--force"), "no hint to force it");
    }

    #[tokio::test]
    async fn a_file_name_cannot_become_a_stage_flag() {
        let Some(dir) = repo(&[("--help", "one\n")]) else {
            return;
        };
        std::fs::write(dir.path().join("--help"), "two\n").expect("writable");
        let ctx = context(&dir);
        let result = GitAdd
            .run(
                &ToolCall::new("git_add", json!({"paths": ["--help"]})),
                &ctx,
            )
            .await;
        assert!(result.is_success(), "{}", result.summary());
        assert!(
            result.output.text.contains("--help"),
            "{}",
            result.output.text
        );
    }

    /// A message is one argument, so a message that reads like an option is
    /// still only a message.
    #[tokio::test]
    async fn a_message_is_a_message_and_not_a_flag() {
        let Some(dir) = repo(&[("tracked.txt", "one\n")]) else {
            return;
        };
        std::fs::write(dir.path().join("tracked.txt"), "two\n").expect("writable");
        assert!(git_in(dir.path(), &["add", "tracked.txt"]));
        let ctx = context(&dir);
        let result = GitCommit
            .run(
                &ToolCall::new("git_commit", json!({"message": "--amend -m injected"})),
                &ctx,
            )
            .await;
        assert!(result.is_success(), "{}", result.summary());
        let subject = GitLog
            .run(&ToolCall::new("git_log", json!({"limit": 1})), &ctx)
            .await
            .output
            .text;
        assert!(subject.contains("--amend -m injected"), "{subject}");
    }

    #[tokio::test]
    async fn a_commit_without_a_message_is_refused_before_git_runs() {
        let Some(dir) = repo(&[("tracked.txt", "one\n")]) else {
            return;
        };
        std::fs::write(dir.path().join("tracked.txt"), "two\n").expect("writable");
        assert!(git_in(dir.path(), &["add", "tracked.txt"]));
        let ctx = context(&dir);
        for message in [json!(""), json!("   "), json!(null)] {
            let result = GitCommit
                .run(
                    &ToolCall::new("git_commit", json!({"message": message})),
                    &ctx,
                )
                .await;
            assert!(!result.is_success(), "{message:?} committed");
            assert!(
                result.summary().contains("needs a"),
                "{message:?}: {}",
                result.summary()
            );
        }
        // Still nothing recorded, so the refusal cost the run no history.
        let history = GitLog
            .run(&ToolCall::new("git_log", json!({"limit": 1})), &ctx)
            .await;
        assert!(
            history.output.text.contains("start"),
            "{}",
            history.output.text
        );
    }

    #[test]
    fn a_path_outside_the_repository_is_refused_before_git_runs() {
        let ctx = Context::new(PathBuf::from("/repo"));
        let call = ToolCall::new("git_diff", json!({"path": "../outside"}));
        let problem = GitDiff.intent(&call, &ctx).expect_err("climbs out");
        assert!(problem.contains("climbs out"), "{problem}");
    }

    #[test]
    fn a_scope_becomes_an_argument_after_the_double_dash() {
        let ctx = Context::new(PathBuf::from("/repo"));
        let mut argv = own(&["git", "diff"]);
        let scope = PathBuf::from("/repo/src/lib.rs");
        assert_eq!(narrow(&mut argv, &ctx, &scope), None);
        assert_eq!(argv, vec!["git", "diff", "--", "src/lib.rs"]);

        let mut argv = own(&["git", "diff"]);
        assert_eq!(narrow(&mut argv, &ctx, &PathBuf::from("/repo")), None);
        assert_eq!(
            argv,
            vec!["git", "diff"],
            "the whole repository needs no scope"
        );
    }
}
