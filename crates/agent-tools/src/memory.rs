//! What a run keeps for the next one.
//!
//! A small model cannot hold a repository in its head, so the useful thing to
//! remember is short: `the tests need a fixture in /tmp`, `this crate is the one
//! that fails to build`. One JSON object per line in `.smoll/memory.jsonl`, the
//! directory this workspace already keeps its audit log and its sessions in, and
//! the next run reads the newest few back into its first turn.
//!
//! Two properties are load-bearing:
//!
//! - **Append-only, like the audit log.** `memory_store` adds a line and rewrites
//!   nothing, so a run cannot lose the record of what an earlier one learned, and
//!   a crash leaves the lines it did write.
//! - **Redacted on the way in.** A note is text a model chose, and a model that
//!   has just read a `.env` file writes what it saw into a note as readily as
//!   anywhere else. The mask happens here rather than at read time so the secret
//!   never exists on disk, which is the only version of this that survives the
//!   file being read by something that is not this agent.
//!
//! A note is not a write to the repository's own files, so it is not journaled:
//! `smoll rollback` puts the tree back and leaves the memory alone, exactly as it
//! leaves a git commit alone. The file is small, local, and the only thing in it
//! that a later run sees is what a person or a model chose to say.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::json;

use agent_sandbox::{redact, Permission};

use crate::call::{ToolCall, ToolOutput, ToolResult};
use crate::context::Context;
use crate::schema;
use crate::tool::{Intent, Tool, ToolDefinition};

/// Where a workspace keeps what it has learned, next to its audit log.
pub const MEMORY_FILE: &str = ".smoll/memory.jsonl";

/// Long enough to hold a fact, short enough that ten of them do not take the
/// window a small model needs for the file it is about to read.
pub const MAX_NOTE_CHARS: usize = 200;

/// What `memory_recall` answers with when the call does not say how many.
const DEFAULT_RECALL: usize = 10;

/// The most a model may ask for, because the answer goes into a prompt that is
/// already paying for a repository map.
const MAX_RECALL: usize = 50;

/// One remembered line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Note {
    pub note: String,
    /// Milliseconds since the Unix epoch, so a reader can order notes as well as
    /// lines.
    pub at_ms: u64,
}

/// The path a workspace's memory lives at, resolved through the same policy that
/// guards every other path so a configuration that keeps writes inside the
/// workspace keeps notes inside it too.
pub fn path(ctx: &Context) -> Result<PathBuf, String> {
    ctx.resolve(MEMORY_FILE)
}

pub struct MemoryStore;

#[async_trait]
impl Tool for MemoryStore {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "memory_store",
            "Keep one short sentence for the runs after this one: a decision nobody wrote down, \
             which file is the awkward one, what the tests need. It is stored in this repository \
             with any secret in it masked, not in the model.",
            schema::object(
                json!({"note": schema::string("one short sentence to remember")}),
                &["note"],
            ),
            Permission::Write,
        )
    }

    fn intent(&self, call: &ToolCall, ctx: &Context) -> Result<Intent, String> {
        let _ = call
            .str_arg("note")
            .ok_or_else(|| "memory_store needs a \"note\"".to_owned())?;
        Ok(Intent::writing(vec![path(ctx)?]))
    }

    async fn run(&self, call: &ToolCall, ctx: &Context) -> ToolResult {
        let failed = |message: String| {
            ToolResult::failed(
                call.clone(),
                Permission::Write,
                message,
                ToolOutput::empty(),
            )
        };
        let Some(raw) = call.str_arg("note") else {
            return failed(
                "memory_store needs \"note\" as a string: the one sentence to keep".to_owned(),
            );
        };
        let kept = one_line(&raw);
        if kept.is_empty() {
            return failed(
                "memory_store was given nothing to remember; write the sentence you would want \
                 on the first turn of the next run"
                    .to_owned(),
            );
        }
        let (clean, report) = redact(&kept);
        let (note, shortened) = trim(&clean);
        let at = match path(ctx) {
            Ok(at) => at,
            Err(problem) => return failed(problem),
        };
        let entry = Note {
            note,
            at_ms: smollm_core::session::now_ms(),
        };
        if let Err(problem) = append(&at, &entry) {
            return failed(format!(
                "the note could not be kept in {}: {problem}",
                ctx.relative(&at)
            ));
        }
        let mut text = format!("remembered: {}", entry.note);
        if shortened {
            text.push_str(&format!(
                " [kept the first {MAX_NOTE_CHARS} characters — say less, or store two notes]"
            ));
        }
        if report.masked > 0 {
            // The model wrote the secret and the file did not receive it; the
            // answer has to say so, or the next run is told a tidier story than
            // the one that happened.
            text.push_str(" [a credential-shaped word in it was masked before storing]");
        }
        ToolResult::completed(call.clone(), Permission::Write, ToolOutput::new(text), 0)
    }
}

pub struct MemoryRecall;

#[async_trait]
impl Tool for MemoryRecall {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "memory_recall",
            "Read back what `memory_store` kept, newest last. The first turn of a run already \
             shows the most recent few, so ask when you need more of them.",
            schema::object(
                json!({"limit": schema::integer("how many to return; default 10", 1)}),
                &[],
            ),
            Permission::ReadOnly,
        )
    }

    fn intent(&self, call: &ToolCall, ctx: &Context) -> Result<Intent, String> {
        let _ = call;
        Ok(Intent::reading(vec![path(ctx)?]))
    }

    async fn run(&self, call: &ToolCall, ctx: &Context) -> ToolResult {
        let limit = call
            .usize_arg("limit")
            .unwrap_or(DEFAULT_RECALL)
            .clamp(1, MAX_RECALL);
        let at = match path(ctx) {
            Ok(at) => at,
            Err(problem) => {
                return ToolResult::failed(
                    call.clone(),
                    Permission::ReadOnly,
                    problem,
                    ToolOutput::empty(),
                )
            }
        };
        let notes = recent(&at, limit);
        if notes.is_empty() {
            return ToolResult::completed(
                call.clone(),
                Permission::ReadOnly,
                ToolOutput::new(format!(
                    "{} holds nothing yet; memory_store is what puts something in it",
                    ctx.relative(&at)
                )),
                0,
            );
        }
        let mut text = format!("{} remembered note(s), oldest first:", notes.len());
        for note in &notes {
            text.push_str(&format!("\n- {note}"));
        }
        ToolResult::completed(call.clone(), Permission::ReadOnly, ToolOutput::new(text), 0)
    }
}

/// The newest `limit` notes in this file, oldest first — which is the order a
/// prompt reads them in.
///
/// A line that is not a note is skipped rather than fatal: the file is plain text
/// a person may have edited, and one unreadable line must not cost a run the
/// memory of everything else.
pub fn recent(file: &Path, limit: usize) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(file) else {
        return Vec::new();
    };
    let mut notes: Vec<String> = text
        .lines()
        .filter_map(|line| serde_json::from_str::<Note>(line).ok())
        .filter(|entry| !entry.note.trim().is_empty())
        .map(|entry| entry.note)
        .collect();
    if notes.len() > limit {
        notes.drain(0..notes.len() - limit);
    }
    notes
}

/// How many notes a workspace has kept, which `smoll doctor` can report without
/// reading their contents.
pub fn count(file: &Path) -> usize {
    let Ok(text) = std::fs::read_to_string(file) else {
        return 0;
    };
    text.lines()
        .filter(|line| serde_json::from_str::<Note>(line).is_ok())
        .count()
}

fn append(file: &Path, entry: &Note) -> Result<(), String> {
    use std::io::Write;
    let line = serde_json::to_string(entry).map_err(|source| source.to_string())?;
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir).map_err(|source| format!("{}: {source}", dir.display()))?;
    }
    let mut open = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(file)
        .map_err(|source| format!("{}: {source}", file.display()))?;
    writeln!(open, "{line}").map_err(|source| format!("{}: {source}", file.display()))
}

/// A note as one sentence: inner whitespace collapsed, so the line it is stored
/// on stays a line and the prompt it is read back into stays a list.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The note to store and whether it had to be cut to fit.
fn trim(text: &str) -> (String, bool) {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= MAX_NOTE_CHARS {
        return (text.to_owned(), false);
    }
    (chars[..MAX_NOTE_CHARS].iter().collect(), true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(arguments: serde_json::Value) -> ToolCall {
        ToolCall::new("memory_store", arguments)
    }

    fn stored(dir: &Path) -> String {
        std::fs::read_to_string(dir.join(MEMORY_FILE)).expect("the file the tool wrote")
    }

    #[tokio::test]
    async fn a_note_survives_the_run_that_wrote_it() {
        let dir = tempfile::tempdir().expect("a temp repo");
        let ctx = Context::new(dir.path());
        let result = MemoryStore
            .run(
                &call(json!({"note": "the tests need a fixture in /tmp"})),
                &ctx,
            )
            .await;
        assert!(result.is_success(), "{}", result.summary());
        assert!(
            result.output.text.contains("the tests need a fixture"),
            "the model is told what was kept: {}",
            result.output.text
        );

        // A later process — which is what a second run is — reads it back.
        assert_eq!(
            recent(&dir.path().join(MEMORY_FILE), 10),
            vec!["the tests need a fixture in /tmp"]
        );
    }

    #[tokio::test]
    async fn notes_come_back_oldest_first_and_only_the_newest_of_them() {
        let dir = tempfile::tempdir().expect("a temp repo");
        let ctx = Context::new(dir.path());
        for note in ["first", "second", "third"] {
            MemoryStore.run(&call(json!({"note": note})), &ctx).await;
        }
        let all = recent(&dir.path().join(MEMORY_FILE), 10);
        assert_eq!(all, vec!["first", "second", "third"], "{all:?}");
        assert_eq!(
            recent(&dir.path().join(MEMORY_FILE), 2),
            vec!["second", "third"],
            "the newest, still oldest first"
        );
        assert_eq!(count(&dir.path().join(MEMORY_FILE)), 3);
    }

    /// The reason redaction happens before the write and not at read time: a file
    /// a person opens in an editor, or a backup that leaves the machine, should
    /// not contain the credential the model copied out of a `.env`.
    #[tokio::test]
    async fn a_secret_in_a_note_never_reaches_the_disk() {
        let dir = tempfile::tempdir().expect("a temp repo");
        let ctx = Context::new(dir.path());
        let result = MemoryStore
            .run(
                &call(json!({"note": "deploy with api_token = \"hf_AAAAAAAAAAAAAAAAAAAAAAAA\""})),
                &ctx,
            )
            .await;
        assert!(result.is_success(), "{}", result.summary());
        let file = stored(dir.path());
        assert!(!file.contains("AAAA"), "the token is not on disk: {file}");
        assert!(file.contains("api_token"), "and the note is still readable");
        assert!(
            result.output.text.contains("masked"),
            "the run says it hid something: {}",
            result.output.text
        );
    }

    #[tokio::test]
    async fn a_note_is_kept_as_one_line_however_it_was_written() {
        let dir = tempfile::tempdir().expect("a temp repo");
        let ctx = Context::new(dir.path());
        MemoryStore
            .run(
                &call(json!({"note": "the loop is\n  in agent.rs\nand the map is in map.rs"})),
                &ctx,
            )
            .await;
        let file = stored(dir.path());
        assert_eq!(file.lines().count(), 1, "{file}");
        assert_eq!(
            recent(&dir.path().join(MEMORY_FILE), 1)[0],
            "the loop is in agent.rs and the map is in map.rs"
        );
    }

    #[tokio::test]
    async fn a_note_too_long_for_its_own_good_is_cut_and_says_so() {
        let dir = tempfile::tempdir().expect("a temp repo");
        let ctx = Context::new(dir.path());
        let long = "x".repeat(MAX_NOTE_CHARS + 80);
        let result = MemoryStore.run(&call(json!({"note": long})), &ctx).await;
        assert!(result.is_success(), "{}", result.summary());
        assert!(
            result
                .output
                .text
                .contains(&format!("first {MAX_NOTE_CHARS}")),
            "{}",
            result.output.text
        );
        assert_eq!(
            recent(&dir.path().join(MEMORY_FILE), 1)[0].chars().count(),
            MAX_NOTE_CHARS
        );
    }

    #[tokio::test]
    async fn nothing_to_remember_is_refused_rather_than_stored_as_an_empty_line() {
        let dir = tempfile::tempdir().expect("a temp repo");
        let ctx = Context::new(dir.path());
        for arguments in [json!({}), json!({"note": "   \n  "}), json!({"note": ""})] {
            let result = MemoryStore.run(&call(arguments), &ctx).await;
            assert!(!result.is_success(), "{:?}", result.summary());
        }
        assert!(
            !dir.path().join(MEMORY_FILE).exists(),
            "a refusal creates no file"
        );
    }

    #[tokio::test]
    async fn recalling_from_a_repository_that_has_remembered_nothing_says_so() {
        let dir = tempfile::tempdir().expect("a temp repo");
        let ctx = Context::new(dir.path());
        let result = MemoryRecall
            .run(&ToolCall::new("memory_recall", json!({})), &ctx)
            .await;
        assert!(result.is_success(), "{}", result.summary());
        assert!(
            result.output.text.contains("holds nothing"),
            "{}",
            result.output.text
        );
    }

    #[tokio::test]
    async fn recall_answers_with_what_it_was_asked_for() {
        let dir = tempfile::tempdir().expect("a temp repo");
        let ctx = Context::new(dir.path());
        for note in ["one", "two", "three"] {
            MemoryStore.run(&call(json!({"note": note})), &ctx).await;
        }
        let result = MemoryRecall
            .run(&ToolCall::new("memory_recall", json!({"limit": 2})), &ctx)
            .await;
        assert!(
            result.output.text.contains("- two\n- three"),
            "{}",
            result.output.text
        );
        assert!(
            !result.output.text.contains("- one"),
            "the limit was honoured: {}",
            result.output.text
        );
        // A limit outside any sensible range asks for room rather than a crash.
        let greedy = MemoryRecall
            .run(
                &ToolCall::new("memory_recall", json!({"limit": 10_000})),
                &ctx,
            )
            .await;
        assert!(greedy.is_success(), "{}", greedy.summary());
    }

    /// A note is not a change to the repository's own files, so it is not
    /// journaled: undoing a run puts its edits back and leaves what the run
    /// learned learned. Same rule as a git commit.
    #[tokio::test]
    async fn rolling_back_a_run_forgets_nothing() {
        let dir = tempfile::tempdir().expect("a temp repo");
        let ctx = Context::new(dir.path());
        MemoryStore
            .run(&call(json!({"note": "the map lives in map.rs"})), &ctx)
            .await;
        assert!(ctx.changes().is_empty(), "a note journals no snapshot");
        assert!(ctx.rollback().is_empty());
        assert_eq!(
            count(&dir.path().join(MEMORY_FILE)),
            1,
            "and the note stays"
        );
    }

    #[test]
    fn a_note_is_a_write_as_far_as_the_policy_is_concerned() {
        let dir = tempfile::tempdir().expect("a temp repo");
        let ctx = Context::new(dir.path());
        let intent = MemoryStore
            .intent(&call(json!({"note": "something"})), &ctx)
            .expect("the memory file is inside the workspace");
        assert_eq!(intent.permission(Permission::Write), Permission::Write);
        assert_eq!(
            intent.subject().as_deref(),
            Some(dir.path().join(MEMORY_FILE).to_str().expect("utf-8"))
        );
        assert_eq!(
            MemoryRecall
                .intent(&ToolCall::new("memory_recall", json!({})), &ctx)
                .expect("a read")
                .permission(Permission::ReadOnly),
            Permission::ReadOnly
        );
    }

    #[test]
    fn an_unreadable_line_costs_the_run_nothing_else_it_remembered() {
        let dir = tempfile::tempdir().expect("a temp repo");
        std::fs::create_dir_all(dir.path().join(".smoll")).expect("writable");
        std::fs::write(
            dir.path().join(MEMORY_FILE),
            "{\"note\":\"kept\",\"at_ms\":1}\nnot json at all\n{\"note\":\"also kept\",\"at_ms\":2}\n",
        )
        .expect("writable");
        assert_eq!(
            recent(&dir.path().join(MEMORY_FILE), 10),
            vec!["kept", "also kept"]
        );
    }

    #[test]
    fn a_note_is_one_sentence_before_it_is_anything() {
        assert_eq!(one_line("  a \n b\t c  "), "a b c");
        let (kept, shortened) = trim(&"y".repeat(MAX_NOTE_CHARS));
        assert_eq!(kept.chars().count(), MAX_NOTE_CHARS);
        assert!(!shortened, "exactly at the limit is not over it");
        assert!(trim("y".repeat(MAX_NOTE_CHARS + 1).as_str()).1);
    }

    /// Multibyte text must not be cut in the middle of a character, which is what
    /// a byte-length limit would do to a note written in another alphabet.
    #[test]
    fn trimming_is_counted_in_characters_not_bytes() {
        let text = "é".repeat(MAX_NOTE_CHARS + 10);
        let (kept, shortened) = trim(&text);
        assert!(shortened);
        assert_eq!(kept.chars().count(), MAX_NOTE_CHARS);
        assert_eq!(kept.len(), MAX_NOTE_CHARS * 2, "two bytes apiece, uncut");
    }
}
