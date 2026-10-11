//! Changing the repository, in the three shapes a small model can be trusted with.
//!
//! `write_file` replaces a whole file; `patch_file` applies a unified diff;
//! `delete_file` removes one. The first two answer with the diff they applied
//! rather than with "ok", because a model that cannot see its own change cannot
//! check it, and a reviewer who sees the diff sees the neighbourhood of the
//! change instead of only its result.
//!
//! All three go through `agent_repo::patch`, so the path rules, the context
//! matching and the snapshot are the same code the rest of the agent uses — and
//! all three leave what they were about to overwrite in [`Context::rollback`],
//! which is how a run that went wrong can be put back. For `delete_file` that is
//! the whole design: it refuses anything the journal could not restore.
//!
//! None of them decides whether it should have run. The registry asks the policy,
//! and in `suggest-only` the policy says no before any of these is entered.

use std::path::PathBuf;

use async_trait::async_trait;
use serde_json::json;

use agent_repo::patch::{self, Kind, PatchError, Planned, Stat};
use agent_sandbox::Permission;

use crate::call::{ToolCall, ToolOutput, ToolResult};
use crate::context::Context;
use crate::schema;
use crate::tool::{Intent, Tool, ToolDefinition};

pub struct WriteFile;

#[async_trait]
impl Tool for WriteFile {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "write_file",
            "Write `content` as the whole of the file at `path`, creating it and its parent \
             directories when they are missing. It replaces what is there, so use it for a new \
             file or one you have read in full; `patch_file` is cheaper and safer for a small \
             change to a file you have only partly seen. The answer is the diff of what changed.",
            schema::object(
                json!({
                    "path": schema::path("the file to write"),
                    "content": schema::string("the whole new contents of the file"),
                }),
                &["path", "content"],
            ),
            Permission::Write,
        )
    }

    fn intent(&self, call: &ToolCall, ctx: &Context) -> Result<Intent, String> {
        let raw = call
            .str_arg("path")
            .ok_or_else(|| "write_file needs a \"path\"".to_owned())?;
        Ok(Intent::writing(vec![ctx.resolve(&raw)?]))
    }

    async fn run(&self, call: &ToolCall, ctx: &Context) -> ToolResult {
        let problem = |message: String| {
            ToolResult::failed(
                call.clone(),
                Permission::Write,
                message,
                ToolOutput::empty(),
            )
        };
        let path = match self.intent(call, ctx) {
            Ok(intent) => intent.paths[0].clone(),
            Err(message) => return problem(message),
        };
        let Some(content) = call
            .arguments
            .get("content")
            .and_then(|value| value.as_str())
        else {
            return problem(
                "write_file needs \"content\" as a string; the whole file, not a description of it"
                    .to_owned(),
            );
        };
        let relative = ctx.relative(&path);
        let prior = match prior_text(&path, &relative) {
            Ok(prior) => prior,
            Err(message) => return problem(message),
        };
        if content.is_empty() && prior.as_deref().is_some_and(|text| !text.trim().is_empty()) {
            // An empty string where a file's contents belong is the most common
            // way a small model destroys work it meant to edit.
            return problem(format!(
                "{relative} holds {} line(s) and the content asked for is empty, so this would \
                 throw away its whole file rather than change it",
                prior.unwrap_or_default().lines().count()
            ));
        }
        let diff = patch::diff(&relative, prior.as_deref().unwrap_or(""), content);
        if diff.is_empty() {
            return ToolResult::completed(
                call.clone(),
                Permission::Write,
                ToolOutput::new(format!(
                    "{relative} already holds exactly that; nothing changed"
                )),
                0,
            );
        }
        let planned = Planned {
            kind: if prior.is_none() {
                Kind::Added
            } else {
                Kind::Modified
            },
            path: path.clone(),
            content: Some(content.to_owned()),
            stat: stat_of(&diff, &relative),
        };
        match apply(ctx, std::slice::from_ref(&planned)) {
            Ok(summary) => ToolResult::completed(
                call.clone(),
                Permission::Write,
                // The model wrote the file blind; the diff is how it sees what the
                // workspace now holds, and the registry trims it to the ceiling.
                ToolOutput::new(format!("{summary}\n{diff}")),
                0,
            ),
            Err(source) => problem(patch_problem(&source)),
        }
    }
}

pub struct PatchFile;

#[async_trait]
impl Tool for PatchFile {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "patch_file",
            "Apply a unified diff, the format `read_file` and `git_diff` show. Give three lines \
             of unchanged context around every change and copy them exactly; the patch is \
             applied where its context is, not where its line numbers say, and is refused if the \
             context is not there. `path` is only needed when the diff has no --- and +++ lines \
             of its own. Nothing is written unless the whole patch applies.",
            schema::object(
                json!({
                    "patch": schema::string("the unified diff, --- and +++ lines included"),
                    "path": schema::path("the file the hunks belong to, when the patch does not name it"),
                }),
                &["patch"],
            ),
            Permission::Write,
        )
    }

    fn intent(&self, call: &ToolCall, ctx: &Context) -> Result<Intent, String> {
        let text = patched(call);
        let names = patch::touched_paths(&text).unwrap_or_default();
        let mut paths = Vec::new();
        for name in names {
            paths.push(ctx.resolve(&name)?);
        }
        if paths.is_empty() {
            let raw = call
                .str_arg("path")
                .ok_or_else(|| "patch_file needs a \"patch\" that names its file".to_owned())?;
            paths.push(ctx.resolve(&raw)?);
        }
        Ok(Intent::writing(paths))
    }

    async fn run(&self, call: &ToolCall, ctx: &Context) -> ToolResult {
        let text = patched(call);
        if text.trim().is_empty() {
            return ToolResult::failed(
                call.clone(),
                Permission::Write,
                "patch_file was given an empty patch".to_owned(),
                ToolOutput::empty(),
            );
        }
        match patch::plan(&ctx.root, &text) {
            Ok(planned) => match apply(ctx, &planned) {
                Ok(summary) => ToolResult::completed(
                    call.clone(),
                    Permission::Write,
                    ToolOutput::new(summary),
                    0,
                ),
                Err(source) => ToolResult::failed(
                    call.clone(),
                    Permission::Write,
                    patch_problem(&source),
                    ToolOutput::empty(),
                ),
            },
            Err(source) => ToolResult::failed(
                call.clone(),
                Permission::Write,
                patch_problem(&source),
                ToolOutput::empty(),
            ),
        }
    }
}

/// The patch as the model meant it: with headers, or with the ones its `path`
/// argument implies.
///
/// A small model asked for a diff often writes the hunk and nothing else, because
/// the file is in the other argument. Naming it here is cheaper than refusing the
/// call and spending a step on the correction.
fn patched(call: &ToolCall) -> String {
    let Some(body) = call.str_arg("patch") else {
        return String::new();
    };
    if body.contains("--- ") || body.contains("diff --git") {
        return body;
    }
    match call.str_arg("path") {
        Some(path) => format!("--- a/{path}\n+++ b/{path}\n{body}"),
        None => body,
    }
}

/// A refusal a model can act on, in the class this tool is.
fn refusal(call: &ToolCall, message: String) -> ToolResult {
    ToolResult::failed(
        call.clone(),
        Permission::Destructive,
        message,
        ToolOutput::empty(),
    )
}

pub struct DeleteFile;

#[async_trait]
impl Tool for DeleteFile {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "delete_file",
            "Remove one file from the repository. It always asks a person first, in every \
             approval mode except the ones that write nothing at all. What it removes has to be \
             a text file the run can hold onto, because `smoll rollback` puts it back from what \
             was recorded here; a directory, a binary and a file over the read limit are refused \
             rather than removed past the point of being recoverable.",
            schema::object(
                json!({
                    "path": schema::path("the file to remove"),
                }),
                &["path"],
            ),
            Permission::Destructive,
        )
    }

    fn intent(&self, call: &ToolCall, ctx: &Context) -> Result<Intent, String> {
        let raw = call
            .str_arg("path")
            .ok_or_else(|| "delete_file needs a \"path\"".to_owned())?;
        Ok(Intent::writing(vec![ctx.resolve(&raw)?]))
    }

    async fn run(&self, call: &ToolCall, ctx: &Context) -> ToolResult {
        let path = match self.intent(call, ctx) {
            Ok(intent) => intent.paths[0].clone(),
            Err(problem) => return refusal(call, problem),
        };
        let relative = ctx.relative(&path);
        if path == ctx.root || relative.is_empty() {
            // The one path the resolver hands back that must never be removed:
            // `path = "."` is a legal workspace-relative path, and deleting it is
            // deleting the repository the run was pointed at.
            return refusal(
                call,
                "delete_file removes one file, not the repository itself; name a file inside it"
                    .to_owned(),
            );
        }
        if path.is_dir() {
            return refusal(
                call,
                format!("{relative} is a directory, and delete_file takes one file at a time"),
            );
        }
        if !path.exists() {
            return refusal(
                call,
                format!("{relative} does not exist, so there is nothing to remove"),
            );
        }
        let metadata = match std::fs::metadata(&path) {
            Ok(metadata) => metadata,
            Err(source) => return refusal(call, format!("cannot look at {relative}: {source}")),
        };
        if metadata.len() > agent_repo::MAX_READ_BYTES {
            return refusal(
                call,
                format!(
                    "{relative} is {} bytes, more than this run keeps for a rollback, so \
                     delete_file refuses it rather than removing something it could not put back",
                    metadata.len()
                ),
            );
        }
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(_) => {
                return refusal(
                    call,
                    format!(
                        "{relative} is not a text file, so `smoll rollback` could not restore it; \
                         delete_file leaves it alone"
                    ),
                )
            }
        };
        let planned = Planned {
            kind: Kind::Deleted,
            path: path.clone(),
            content: None,
            stat: Stat {
                path: relative.clone(),
                kind: Kind::Deleted,
                added: 0,
                removed: text.lines().count(),
            },
        };
        match apply(ctx, std::slice::from_ref(&planned)) {
            Ok(summary) => ToolResult::completed(
                call.clone(),
                Permission::Destructive,
                ToolOutput::new(format!(
                    "{summary}, and `smoll rollback` puts {relative} back"
                )),
                0,
            ),
            Err(source) => refusal(call, patch_problem(&source)),
        }
    }
}

/// What the file holds now, as text, or `Err` for a reason the model can act on.
fn prior_text(path: &PathBuf, relative: &str) -> Result<Option<String>, String> {
    if path.is_dir() {
        return Err(format!("{relative} is a directory, not a file"));
    }
    if !path.exists() {
        return Ok(None);
    }
    let metadata = std::fs::metadata(path)
        .map_err(|source| format!("cannot read {relative} before overwriting it: {source}"))?;
    if metadata.len() > agent_repo::MAX_READ_BYTES {
        return Err(format!(
            "{relative} is {} bytes and write_file replaces the whole file, so it refuses one \
             over {} bytes; change it with patch_file instead",
            metadata.len(),
            agent_repo::MAX_READ_BYTES
        ));
    }
    let bytes =
        std::fs::read(path).map_err(|source| format!("cannot read {relative}: {source}"))?;
    String::from_utf8(bytes).map(Some).map_err(|_| {
        format!("{relative} is not a text file, so writing text over it would destroy it")
    })
}

/// Write the planned files and keep what was there, so the answer is both the
/// diff and something that can be undone.
fn apply(ctx: &Context, planned: &[Planned]) -> Result<String, PatchError> {
    let snapshot = patch::write(&ctx.root, planned)?;
    ctx.record(snapshot);
    let files = planned
        .iter()
        .map(|change| {
            format!(
                "{} ({}: +{} -{})",
                ctx.relative(&change.path),
                change.kind.as_str(),
                change.stat.added,
                change.stat.removed
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    Ok(format!("applied to {files}"))
}

/// The one Stat a whole-file rewrite deserves, from the diff that describes it.
fn stat_of(diff: &str, relative: &str) -> Stat {
    patch::stats(diff)
        .ok()
        .and_then(|mut stats| stats.pop())
        .unwrap_or_else(|| Stat {
            path: relative.to_owned(),
            kind: Kind::Modified,
            added: diff.lines().filter(|line| line.starts_with('+')).count(),
            removed: diff.lines().filter(|line| line.starts_with('-')).count(),
        })
}

/// A patch that will not apply, in the words a model needs to write a better one.
fn patch_problem(source: &PatchError) -> String {
    match source {
        PatchError::ContextMismatch {
            path,
            expected,
            line,
        } => format!(
            "{path}: no line `{expected}` was found, so the hunk headed `-{line}` does not match \
             the file. Read the file again and copy its context exactly."
        ),
        PatchError::Malformed(message) => format!(
            "this is not a patch I can read ({message}). A hunk's lines start with a space, a \
             plus or a minus."
        ),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(name: &str, arguments: serde_json::Value) -> ToolCall {
        ToolCall::new(name, arguments)
    }

    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("a temp repo");
        std::fs::write(
            dir.path().join("plan.rs"),
            "pub struct Plan {\n    pub goal: String,\n}\n\npub fn render(plan: &Plan) -> String {\n    plan.goal.clone()\n}\n",
        )
        .expect("writable");
        std::fs::write(dir.path().join("binary.bin"), [0u8, 159, 146, 128]).expect("writable");
        dir
    }

    #[tokio::test]
    async fn a_new_file_is_created_with_its_directories_and_answers_with_a_diff() {
        let dir = repo();
        let ctx = Context::new(dir.path());
        let result = WriteFile
            .run(
                &call(
                    "write_file",
                    json!({"path": "src/deep/new.rs", "content": "fn new() {}\n"}),
                ),
                &ctx,
            )
            .await;
        assert!(result.is_success(), "{}", result.summary());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("src/deep/new.rs")).expect("written"),
            "fn new() {}\n"
        );
        assert!(
            result.output.text.contains("+fn new() {}"),
            "{}",
            result.output.text
        );
        assert!(
            result.output.text.contains("added"),
            "{}",
            result.output.text
        );
    }

    #[tokio::test]
    async fn replacing_a_file_answers_with_the_diff_of_the_replacement() {
        let dir = repo();
        let ctx = Context::new(dir.path());
        let before = std::fs::read_to_string(dir.path().join("plan.rs")).expect("readable");
        let after = before.replace("plan.goal.clone()", "plan.goal.to_uppercase()");
        let result = WriteFile
            .run(
                &call("write_file", json!({"path": "plan.rs", "content": after})),
                &ctx,
            )
            .await;
        assert!(result.is_success(), "{}", result.summary());
        assert!(
            result.output.text.contains("-    plan.goal.clone()"),
            "{}",
            result.output.text
        );
        assert!(
            result.output.text.contains("+    plan.goal.to_uppercase()"),
            "{}",
            result.output.text
        );
    }

    #[tokio::test]
    async fn writing_what_is_already_there_changes_nothing_and_says_so() {
        let dir = repo();
        let ctx = Context::new(dir.path());
        let same = std::fs::read_to_string(dir.path().join("plan.rs")).expect("readable");
        let result = WriteFile
            .run(
                &call("write_file", json!({"path": "plan.rs", "content": same})),
                &ctx,
            )
            .await;
        assert!(
            result.output.text.contains("nothing changed"),
            "{}",
            result.output.text
        );
        assert_eq!(
            ctx.changes().len(),
            0,
            "an unchanged file is not a touched one"
        );
    }

    #[tokio::test]
    async fn an_empty_replacement_is_refused_because_it_is_a_deletion() {
        let dir = repo();
        let ctx = Context::new(dir.path());
        let result = WriteFile
            .run(
                &call("write_file", json!({"path": "plan.rs", "content": ""})),
                &ctx,
            )
            .await;
        assert!(!result.is_success());
        assert!(
            result.summary().contains("throw away"),
            "{}",
            result.summary()
        );
        assert!(
            dir.path().join("plan.rs").is_file(),
            "the file is still there"
        );
    }

    #[tokio::test]
    async fn a_binary_file_is_not_overwritten_with_text() {
        let dir = repo();
        let ctx = Context::new(dir.path());
        let result = WriteFile
            .run(
                &call(
                    "write_file",
                    json!({"path": "binary.bin", "content": "text\n"}),
                ),
                &ctx,
            )
            .await;
        assert!(
            result.summary().contains("not a text file"),
            "{}",
            result.summary()
        );
        assert_eq!(
            std::fs::read(dir.path().join("binary.bin")).expect("readable"),
            vec![0u8, 159, 146, 128]
        );
    }

    #[tokio::test]
    async fn a_patch_is_applied_and_the_previous_contents_can_be_put_back() {
        let dir = repo();
        let ctx = Context::new(dir.path());
        let before = std::fs::read_to_string(dir.path().join("plan.rs")).expect("readable");
        let patch = "--- a/plan.rs\n+++ b/plan.rs\n@@ -1,3 +1,4 @@\n pub struct Plan {\n     pub goal: String,\n+    pub id: u64,\n }\n";
        let result = PatchFile
            .run(&call("patch_file", json!({"patch": patch})), &ctx)
            .await;
        assert!(result.is_success(), "{}", result.summary());
        assert!(std::fs::read_to_string(dir.path().join("plan.rs"))
            .expect("readable")
            .contains("pub id"),);

        let restored = ctx.rollback();
        assert_eq!(restored.len(), 1, "one write, one snapshot");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("plan.rs")).expect("readable"),
            before,
            "the run is undone"
        );
    }

    #[tokio::test]
    async fn a_patch_without_headers_still_knows_which_file_it_means() {
        let dir = repo();
        let ctx = Context::new(dir.path());
        let result = PatchFile
            .run(
                &call(
                    "patch_file",
                    json!({
                        "path": "plan.rs",
                        "patch": "@@ -1,3 +1,3 @@\n pub struct Plan {\n-    pub goal: String,\n+    pub wish: String,\n }\n",
                    }),
                ),
                &ctx,
            )
            .await;
        assert!(result.is_success(), "{}", result.summary());
        assert!(std::fs::read_to_string(dir.path().join("plan.rs"))
            .expect("readable")
            .contains("pub wish"),);
    }

    #[tokio::test]
    async fn a_patch_whose_context_is_not_there_says_which_line_it_wanted() {
        let dir = repo();
        let ctx = Context::new(dir.path());
        let result = PatchFile
            .run(
                &call(
                    "patch_file",
                    json!({"patch": "--- a/plan.rs\n+++ b/plan.rs\n@@ -1,2 +1,2 @@\n-pub struct Absent {}\n+pub struct Plan {\n"}),
                ),
                &ctx,
            )
            .await;
        assert!(!result.is_success());
        let text = result.summary();
        assert!(text.contains("pub struct Absent"), "{text}");
        assert!(text.contains("copy its context exactly"), "{text}");
        assert_eq!(ctx.changes().len(), 0, "a refused patch writes nothing");
    }

    #[tokio::test]
    async fn a_patch_naming_a_path_outside_the_workspace_never_reaches_the_filesystem() {
        let dir = repo();
        let ctx = Context::new(dir.path());
        let outside = dir.path().join("../outside.rs");
        let result = PatchFile
            .run(
                &call(
                    "patch_file",
                    json!({"patch": "--- a/../outside.rs\n+++ b/../outside.rs\n@@ -0,0 +1 @@\n+written\n"}),
                ),
                &ctx,
            )
            .await;
        assert!(!result.is_success(), "{}", result.summary());
        assert!(!outside.exists(), "nothing was written outside");
    }

    #[tokio::test]
    async fn a_write_is_a_write_as_far_as_the_policy_is_concerned() {
        let dir = repo();
        let ctx = Context::new(dir.path());
        let intent = WriteFile
            .intent(
                &call("write_file", json!({"path": "plan.rs", "content": "x"})),
                &ctx,
            )
            .expect("a path");
        assert_eq!(intent.permission(Permission::Write), Permission::Write);
        assert_eq!(
            intent.subject().expect("a subject"),
            ctx.resolve("plan.rs")
                .expect("inside")
                .display()
                .to_string()
        );
    }

    #[tokio::test]
    async fn a_deleted_file_is_put_back_by_the_rollback() {
        let dir = repo();
        let ctx = Context::new(dir.path());
        let before = std::fs::read_to_string(dir.path().join("plan.rs")).expect("readable");
        let result = DeleteFile
            .run(&call("delete_file", json!({"path": "plan.rs"})), &ctx)
            .await;
        assert!(result.is_success(), "{}", result.summary());
        assert!(!dir.path().join("plan.rs").exists());
        assert!(
            result.output.text.contains("rollback"),
            "{}",
            result.output.text
        );

        let restored = ctx.rollback();
        assert_eq!(restored.len(), 1, "one deletion, one snapshot");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("plan.rs")).expect("restored"),
            before,
            "a deletion the run can undo"
        );
    }

    /// `path = "."` resolves to a legal workspace path, and that path is the
    /// repository. The resolver cannot be the guard there because it has nothing
    /// to object to, so the tool checks the root before it checks anything.
    #[tokio::test]
    async fn the_repository_itself_is_never_a_file_to_delete() {
        let dir = repo();
        let ctx = Context::new(dir.path());
        let result = DeleteFile
            .run(&call("delete_file", json!({"path": "."})), &ctx)
            .await;
        assert!(
            result.summary().contains("not the repository"),
            "{}",
            result.summary()
        );
        assert!(dir.path().join("plan.rs").is_file());

        // The two spellings that mean the same thing are refused one layer
        // earlier, where a path is worked out at all.
        for spelling in ["", "/"] {
            let problem = DeleteFile
                .intent(&call("delete_file", json!({"path": spelling})), &ctx)
                .expect_err("not a file in the repository");
            assert!(!problem.is_empty(), "{spelling}");
            assert!(dir.path().join("plan.rs").is_file(), "{spelling}");
        }
    }

    #[tokio::test]
    async fn a_directory_an_absent_file_and_a_path_that_climbs_out_are_all_refused() {
        let dir = repo();
        std::fs::create_dir(dir.path().join("sub")).expect("writable");
        let ctx = Context::new(dir.path());

        let result = DeleteFile
            .run(&call("delete_file", json!({"path": "sub"})), &ctx)
            .await;
        assert!(
            result.summary().contains("is a directory"),
            "{}",
            result.summary()
        );
        assert!(dir.path().join("sub").is_dir());

        let result = DeleteFile
            .run(&call("delete_file", json!({"path": "gone.rs"})), &ctx)
            .await;
        assert!(
            result.summary().contains("does not exist"),
            "{}",
            result.summary()
        );

        let problem = DeleteFile
            .intent(&call("delete_file", json!({"path": "../outside.rs"})), &ctx)
            .expect_err("climbs out");
        assert!(problem.contains("climbs out"), "{problem}");
        assert!(dir.path().join("plan.rs").is_file(), "nothing was removed");
    }

    /// The reason this tool refuses a binary is not caution for its own sake:
    /// the journal holds text, so a removed binary is gone for good and the tool
    /// would be undoing nothing while claiming to.
    #[tokio::test]
    async fn a_binary_is_left_alone_because_rollback_could_not_restore_it() {
        let dir = repo();
        let ctx = Context::new(dir.path());
        let result = DeleteFile
            .run(&call("delete_file", json!({"path": "binary.bin"})), &ctx)
            .await;
        assert!(
            result.summary().contains("not a text file"),
            "{}",
            result.summary()
        );
        assert_eq!(
            std::fs::read(dir.path().join("binary.bin")).expect("still there"),
            vec![0u8, 159, 146, 128]
        );
        assert!(ctx.changes().is_empty(), "a refusal journals nothing");
    }

    #[test]
    fn a_deletion_is_destructive_however_the_intent_is_written() {
        let dir = repo();
        let ctx = Context::new(dir.path());
        let definition = DeleteFile.definition();
        assert_eq!(definition.permission, Permission::Destructive);
        let intent = DeleteFile
            .intent(&call("delete_file", json!({"path": "plan.rs"})), &ctx)
            .expect("a path");
        assert_eq!(
            intent.permission(definition.permission),
            Permission::Destructive,
            "declaring a write cannot make a deletion look like one"
        );
        assert_eq!(
            intent.subject().as_deref(),
            Some(dir.path().join("plan.rs").to_str().expect("utf-8"))
        );
    }

    #[test]
    fn a_headless_patch_gets_the_headers_its_path_argument_implies() {
        let headless = call(
            "patch_file",
            json!({"path": "a/b.rs", "patch": "@@ -1 +1 @@\n-x\n+y\n"}),
        );
        assert_eq!(
            patched(&headless),
            "--- a/a/b.rs\n+++ b/a/b.rs\n@@ -1 +1 @@\n-x\n+y\n"
        );
        let named = call(
            "patch_file",
            json!({"path": "a/b.rs", "patch": "--- a/other.rs\n+++ b/other.rs\n@@ -1 +1 @@\n-x\n+y\n"}),
        );
        assert!(
            patched(&named).starts_with("--- a/other.rs"),
            "the patch wins"
        );
        assert_eq!(patched(&call("patch_file", json!({"path": "x"}))), "");
    }
}
