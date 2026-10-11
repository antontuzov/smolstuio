//! Reading the repository: one file, or the shape of a directory.
//!
//! Both tools are read-only and both are bounded, because the failure they are
//! designed against is not "cannot see the file" but "saw the file and spent the
//! whole context window on it". A tool that returns a 400 KB generated JSON blob
//! ends a run as surely as one that returns nothing.

use async_trait::async_trait;
use serde_json::json;

use agent_sandbox::Permission;

use crate::call::{ToolCall, ToolOutput, ToolResult};
use crate::context::Context;
use crate::schema;
use crate::tool::{Intent, Tool, ToolDefinition};

/// How many lines a file read costs the model when it does not say.
const DEFAULT_LINES: usize = 200;

pub struct ReadFile;

#[async_trait]
impl Tool for ReadFile {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "read_file",
            "Read a text file: at most `lines` lines from `from_line`, each prefixed with its \
             number. A file that is not text is refused rather than guessed at.",
            schema::object(
                json!({
                    "path": schema::path("the file to read"),
                    "from_line": schema::integer("1-based line to start at; default 1", 1),
                    "lines": schema::integer("how many lines; default 200", 1),
                }),
                &["path"],
            ),
            Permission::ReadOnly,
        )
    }

    fn intent(&self, call: &ToolCall, ctx: &Context) -> Result<Intent, String> {
        let raw = call
            .str_arg("path")
            .ok_or_else(|| "read_file needs a \"path\"".to_owned())?;
        Ok(Intent::reading(vec![ctx.resolve(&raw)?]))
    }

    async fn run(&self, call: &ToolCall, ctx: &Context) -> ToolResult {
        let path = match self.intent(call, ctx) {
            Ok(intent) => intent.paths[0].clone(),
            Err(problem) => {
                return ToolResult::failed(
                    call.clone(),
                    Permission::ReadOnly,
                    problem,
                    ToolOutput::empty(),
                )
            }
        };
        match tokio::fs::read(&path).await {
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(text) => ToolResult::completed(
                    call.clone(),
                    Permission::ReadOnly,
                    ToolOutput::new(windowed(&ctx.relative(&path), &text, call)),
                    0,
                ),
                Err(_) => ToolResult::failed(
                    call.clone(),
                    Permission::ReadOnly,
                    format!(
                        "{} is not a text file, so it cannot be read as one",
                        ctx.relative(&path)
                    ),
                    ToolOutput::empty(),
                ),
            },
            Err(source) => {
                // Asked of the path rather than of the error kind:
                // `ErrorKind::IsADirectory` is newer than this workspace's
                // minimum supported Rust, and a directory that failed to read
                // has the same answer either way.
                let message = if path.is_dir() {
                    format!("{} is a directory; use list_dir", ctx.relative(&path))
                } else if source.kind() == std::io::ErrorKind::NotFound {
                    format!("{} does not exist", ctx.relative(&path))
                } else {
                    format!("cannot read {}: {source}", ctx.relative(&path))
                };
                ToolResult::failed(
                    call.clone(),
                    Permission::ReadOnly,
                    message,
                    ToolOutput::empty(),
                )
            }
        }
    }
}

/// The numbered window a file read returns, with the header that says what was
/// left out.
fn windowed(relative: &str, text: &str, call: &ToolCall) -> String {
    let from = call.usize_arg("from_line").unwrap_or(1).max(1);
    let count = call.usize_arg("lines").unwrap_or(DEFAULT_LINES).max(1);
    let lines: Vec<&str> = text.lines().collect();
    if lines.is_empty() {
        return format!("{relative}: empty file");
    }
    let start = from.min(lines.len());
    let end = start + count - 1;
    let end = end.min(lines.len());
    let mut out = String::new();
    if start > 1 || end < lines.len() {
        out.push_str(&format!(
            "[{relative}: lines {start}-{end} of {}]\n",
            lines.len()
        ));
    }
    for (index, line) in lines.iter().enumerate().take(end).skip(start - 1) {
        out.push_str(&format!("{:>5}\t{line}\n", index + 1));
    }
    if end < lines.len() {
        out.push_str(&format!(
            "[{} lines more; read_file with from_line = {} for the rest]",
            lines.len() - end,
            end + 1
        ));
    }
    out
}

pub struct ListDir;

#[async_trait]
impl Tool for ListDir {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "list_dir",
            "List what is under `path`, up to `depth` levels down, skipping what .gitignore \
             skips; directories end in `/`.",
            schema::object(
                json!({
                    "path": schema::path("the directory to list; default the root"),
                    "depth": schema::integer("levels to descend; default 1", 1),
                }),
                &[],
            ),
            Permission::ReadOnly,
        )
    }

    fn intent(&self, call: &ToolCall, ctx: &Context) -> Result<Intent, String> {
        let raw = call.str_arg("path").unwrap_or_else(|| ".".to_owned());
        Ok(Intent::reading(vec![ctx.resolve(&raw)?]))
    }

    async fn run(&self, call: &ToolCall, ctx: &Context) -> ToolResult {
        let root = match self.intent(call, ctx) {
            Ok(intent) => intent.paths[0].clone(),
            Err(problem) => {
                return ToolResult::failed(
                    call.clone(),
                    Permission::ReadOnly,
                    problem,
                    ToolOutput::empty(),
                )
            }
        };
        let depth = call.usize_arg("depth").unwrap_or(1).max(1);
        let mut entries: Vec<String> = Vec::new();
        let mut walked = 0usize;
        let mut builder = ignore::WalkBuilder::new(&root);
        builder
            .max_depth(Some(depth))
            .hidden(false)
            // The same rule the search tools follow: .gitignore is honoured
            // whether or not this directory is a working tree.
            .require_git(false);
        for entry in builder.build().flatten() {
            // The root itself is skipped by hand rather than with
            // `min_depth`: the walker drops that entry before its ignore rules
            // are read, so a filtered start would list what .gitignore hides.
            if entry.depth() == 0 {
                continue;
            }
            walked += 1;
            let is_dir = entry.file_type().is_some_and(|kind| kind.is_dir());
            let name = entry
                .path()
                .strip_prefix(&root)
                .unwrap_or(entry.path())
                .to_string_lossy()
                .into_owned();
            entries.push(if is_dir { format!("{name}/") } else { name });
            if entries.len() >= MAX_ENTRIES {
                break;
            }
        }
        if walked == 0 && !root.is_dir() {
            return ToolResult::failed(
                call.clone(),
                Permission::ReadOnly,
                format!("{} is not a directory", ctx.relative(&root)),
                ToolOutput::empty(),
            );
        }
        entries.sort();
        let mut text = entries.join("\n");
        if !text.is_empty() {
            text.push('\n');
        }
        if walked > entries.len() {
            text.push_str(&format!(
                "[the listing stops at {MAX_ENTRIES} entries; go deeper with path = …]"
            ));
        }
        if text.is_empty() {
            text = format!("{} is empty", ctx.relative(&root));
        }
        ToolResult::completed(call.clone(), Permission::ReadOnly, ToolOutput::new(text), 0)
    }
}

/// Enough of a tree to plan against; the rest is what search is for.
const MAX_ENTRIES: usize = 400;

#[cfg(test)]
mod tests {
    use super::*;

    fn call(arguments: serde_json::Value) -> ToolCall {
        ToolCall::new("read_file", arguments)
    }

    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("a temp repo");
        std::fs::write(dir.path().join("small.txt"), "one\ntwo\nthree\n").expect("writable");
        std::fs::write(dir.path().join("big.txt"), "x\n".repeat(500)).expect("writable");
        std::fs::create_dir_all(dir.path().join("nested")).expect("writable");
        std::fs::write(dir.path().join("nested/inner.txt"), "deep\n").expect("writable");
        std::fs::write(dir.path().join("binary.bin"), [0u8, 159, 146, 128]).expect("writable");
        dir
    }

    #[tokio::test]
    async fn a_small_file_comes_back_with_line_numbers() {
        let dir = repo();
        let ctx = Context::new(dir.path());
        let result = ReadFile
            .run(&call(json!({"path": "small.txt"})), &ctx)
            .await;
        assert!(result.is_success(), "{}", result.summary());
        assert!(
            result.output.text.contains("1\tone"),
            "{}",
            result.output.text
        );
        assert!(
            !result.output.text.contains("lines 1-3"),
            "a whole file needs no header: {}",
            result.output.text
        );
    }

    #[tokio::test]
    async fn a_long_file_comes_back_in_a_window_that_says_so() {
        let dir = repo();
        let ctx = Context::new(dir.path());
        let result = ReadFile.run(&call(json!({"path": "big.txt"})), &ctx).await;
        let text = result.output.text;
        assert!(text.contains("lines 1-200 of 500"), "{text}");
        assert!(text.contains("300 lines more"), "{text}");
        assert!(text.contains("from_line = 201"), "{text}");
    }

    #[tokio::test]
    async fn a_window_can_be_asked_for() {
        let dir = repo();
        let ctx = Context::new(dir.path());
        let result = ReadFile
            .run(
                &call(json!({"path": "big.txt", "from_line": 400, "lines": 10})),
                &ctx,
            )
            .await;
        let text = result.output.text;
        assert!(text.contains("lines 400-409 of 500"), "{text}");
        assert!(text.contains("400\tx"), "{text}");
    }

    #[tokio::test]
    async fn a_missing_file_says_which_one_and_a_directory_points_elsewhere() {
        let dir = repo();
        let ctx = Context::new(dir.path());
        let result = ReadFile.run(&call(json!({"path": "nope.txt"})), &ctx).await;
        assert!(
            result.summary().contains("does not exist"),
            "{}",
            result.summary()
        );
        let result = ReadFile.run(&call(json!({"path": "nested"})), &ctx).await;
        assert!(
            result.summary().contains("list_dir"),
            "{}",
            result.summary()
        );
    }

    #[tokio::test]
    async fn a_binary_file_is_refused_not_guessed_at() {
        let dir = repo();
        let ctx = Context::new(dir.path());
        let result = ReadFile
            .run(&call(json!({"path": "binary.bin"})), &ctx)
            .await;
        assert!(!result.is_success());
        assert!(
            result.summary().contains("not a text file"),
            "{}",
            result.summary()
        );
    }

    #[tokio::test]
    async fn a_path_outside_the_repository_never_reaches_the_filesystem() {
        let dir = repo();
        let ctx = Context::new(dir.path());
        let problem = ReadFile
            .intent(&call(json!({"path": "../../etc/passwd"})), &ctx)
            .expect_err("climbs out");
        assert!(problem.contains("climbs out"), "{problem}");
    }

    #[tokio::test]
    async fn a_directory_listing_skips_what_git_skips() {
        let dir = repo();
        std::fs::write(dir.path().join(".gitignore"), "ignored/\n").expect("writable");
        std::fs::create_dir_all(dir.path().join("ignored")).expect("writable");
        std::fs::write(dir.path().join("ignored/secret.txt"), "x").expect("writable");
        let ctx = Context::new(dir.path());
        let result = ListDir.run(&call(json!({})), &ctx).await;
        let text = result.output.text;
        assert!(text.contains("small.txt"), "{text}");
        assert!(text.contains("nested/"), "{text}");
        assert!(!text.contains("ignored"), "{text}");
    }

    #[tokio::test]
    async fn depth_two_descends() {
        let dir = repo();
        let ctx = Context::new(dir.path());
        let result = ListDir.run(&call(json!({"depth": 2})), &ctx).await;
        assert!(
            result.output.text.contains("nested/inner.txt"),
            "{}",
            result.output.text
        );
    }

    #[test]
    fn a_window_of_nothing_is_still_a_window() {
        let text = windowed(
            "f",
            "one\ntwo\n",
            &call(json!({"path": "f", "from_line": 99})),
        );
        assert!(text.contains("lines 2-2 of 2"), "{text}");
    }
}
