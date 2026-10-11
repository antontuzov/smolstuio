//! A change as text a human can read and a small model can write.
//!
//! Diffs are the format this agent proposes work in. A model that has to say
//! "here are the three lines around the line I mean" is a model that has to look
//! at the file, and a reviewer who sees a hunk sees the neighbourhood of the
//! change rather than only its result. So generation is `similar`'s unified diff
//! at three lines of context, and application is this crate's own parser,
//! because the parser is where a model's damaged patch shows up and where the
//! answer to "which line did you mean" has to come from.
//!
//! Application is tolerant of *position* and strict about *content*: a hunk whose
//! header says line 40 is applied where its context actually is, which is what
//! happens after an earlier edit moved the file. It is never applied where its
//! context is not, because a patch that lands in the wrong function is worse
//! than one that fails.
//!
//! No git here. A repository without a `.git` directory still needs its changes
//! described and put back, and the diff of two texts on disk is enough to say it.

use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// How much neighbourhood a hunk carries.
pub const CONTEXT_RADIUS: usize = 3;

/// A file in a patch does not exist on that side.
const DEV_NULL: &str = "/dev/null";

#[derive(Debug, Error, PartialEq, Eq)]
pub enum PatchError {
    #[error("this is not a unified diff: {0}")]
    Malformed(String),
    #[error("{path}: the patch expects `{expected}` at line {line}, which is not there")]
    ContextMismatch {
        path: String,
        expected: String,
        line: usize,
    },
    #[error("{0} is not inside the repository, so it is not this agent's to change")]
    OutsideRepository(String),
    #[error("cannot read {path}: {reason}")]
    Unreadable { path: String, reason: String },
    #[error("cannot write {path}: {reason}")]
    Unwritable { path: String, reason: String },
}

/// What a patch does to a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Kind {
    Added,
    Modified,
    Deleted,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Added => "added",
            Self::Modified => "modified",
            Self::Deleted => "deleted",
        }
    }
}

/// One line of a hunk.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Line {
    Context(String),
    Removed(String),
    Added(String),
}

impl Line {
    fn text(&self) -> &str {
        match self {
            Self::Context(text) | Self::Removed(text) | Self::Added(text) => text,
        }
    }
}

/// One `@@` block.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Hunk {
    old_start: usize,
    lines: Vec<Line>,
    /// Set by the `\ No newline at end of file` marker on the new side, which is
    /// the only side this crate has to reproduce.
    ends_without_newline: bool,
}

/// One file's part of a patch.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Section {
    path: String,
    kind: Kind,
    hunks: Vec<Hunk>,
}

/// The line count and paths a change touches, ready to print.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stat {
    pub path: String,
    pub kind: Kind,
    pub added: usize,
    pub removed: usize,
}

/// A change, described before it is made.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Planned {
    pub path: PathBuf,
    pub kind: Kind,
    /// The bytes the file will hold, or `None` when the patch removes it.
    pub content: Option<String>,
    pub stat: Stat,
}

/// The unified diff of two texts, in the shape `git diff` and `patch -p1` use.
///
/// Texts that are equal produce an empty string, which is the honest answer:
/// there is nothing to review, and an empty diff is not a change.
pub fn diff(path: &str, before: &str, after: &str) -> String {
    use similar::TextDiff;
    if before == after {
        return String::new();
    }
    let (old_side, new_side) = match (before.is_empty(), after.is_empty()) {
        (true, false) => (DEV_NULL.to_owned(), format!("b/{path}")),
        (false, true) => (format!("a/{path}"), DEV_NULL.to_owned()),
        _ => (format!("a/{path}"), format!("b/{path}")),
    };
    TextDiff::from_lines(before, after)
        .unified_diff()
        .context_radius(CONTEXT_RADIUS)
        .header(&old_side, &new_side)
        .to_string()
}

/// The diff between what is on disk now and what the model wants there.
pub fn diff_against_disk(root: &Path, path: &str, after: &str) -> Result<String, PatchError> {
    let target = inside(root, path)?;
    let before = read(&target, path)?;
    Ok(diff(path, &before, after))
}

/// A one-line summary of a patch, for a report or an approval prompt.
pub fn stats(patch: &str) -> Result<Vec<Stat>, PatchError> {
    parse(patch).map(|sections| sections.iter().map(stat_of).collect())
}

/// The paths a patch names, in the order it names them.
pub fn touched_paths(patch: &str) -> Result<Vec<String>, PatchError> {
    parse(patch).map(|sections| sections.into_iter().map(|section| section.path).collect())
}

/// Apply a single-file patch to the text it was written against.
///
/// A patch naming more than one file is refused rather than half-applied: this
/// function has no way to say which file a model meant, and quietly ignoring
/// the rest of a patch is how a change gets reviewed as something smaller than
/// it is. Use [`plan`] for those.
pub fn apply(before: &str, patch: &str) -> Result<String, PatchError> {
    let sections = parse(patch)?;
    if sections.len() > 1 {
        let names = sections
            .iter()
            .map(|section| section.path.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        return Err(PatchError::Malformed(format!(
            "it changes {} files ({names}); plan a patch file by file",
            sections.len()
        )));
    }
    let one = sections
        .first()
        .ok_or_else(|| PatchError::Malformed("it names no file".to_owned()))?;
    apply_section(one, before)
}

/// Read the tree and say what a patch would do to it, without touching anything.
///
/// This is the dry run: the same answer the write would produce, computed first,
/// so a reviewer sees the result and a run can be refused before it costs a file.
pub fn plan(root: &Path, patch: &str) -> Result<Vec<Planned>, PatchError> {
    let mut planned = Vec::new();
    for section in parse(patch)? {
        if section.kind == Kind::Deleted {
            let target = inside(root, &section.path)?;
            if !target.is_file() {
                return Err(PatchError::Malformed(format!(
                    "{} is deleted by the patch but is not there to delete",
                    section.path
                )));
            }
            planned.push(Planned {
                path: target,
                kind: section.kind,
                content: None,
                stat: stat_of(&section),
            });
            continue;
        }
        let before = if section.kind == Kind::Added {
            String::new()
        } else {
            let target = inside(root, &section.path)?;
            read(&target, &section.path)?
        };
        let content = apply_section(&section, &before)?;
        planned.push(Planned {
            path: inside(root, &section.path)?,
            kind: section.kind,
            stat: stat_of(&section),
            content: Some(content),
        });
    }
    Ok(planned)
}

fn stat_of(section: &Section) -> Stat {
    Stat {
        path: section.path.clone(),
        kind: section.kind,
        added: section
            .hunks
            .iter()
            .flat_map(|hunk| hunk.lines.iter())
            .filter(|line| matches!(line, Line::Added(_)))
            .count(),
        removed: section
            .hunks
            .iter()
            .flat_map(|hunk| hunk.lines.iter())
            .filter(|line| matches!(line, Line::Removed(_)))
            .count(),
    }
}

/// Write the planned files, keeping what was there so it can be put back.
pub fn write(root: &Path, planned: &[Planned]) -> Result<Snapshot, PatchError> {
    let mut snapshot = Snapshot::default();
    for change in planned {
        let relative = deny_outside(root, &change.path)?;
        snapshot.capture(root, &relative);
        match &change.content {
            Some(content) => {
                if let Some(parent) = change.path.parent() {
                    std::fs::create_dir_all(parent).map_err(|source| PatchError::Unwritable {
                        path: relative.clone(),
                        reason: source.to_string(),
                    })?;
                }
                std::fs::write(&change.path, content).map_err(|source| PatchError::Unwritable {
                    path: relative.clone(),
                    reason: source.to_string(),
                })?;
                snapshot.written.insert(relative, content.clone());
            }
            None => {
                std::fs::remove_file(&change.path).map_err(|source| PatchError::Unwritable {
                    path: relative.clone(),
                    reason: source.to_string(),
                })?;
            }
        }
    }
    Ok(snapshot)
}

/// What was on disk before a write, kept so it can be undone.
///
/// Both fields serialise, because a snapshot's point is to outlive the write it
/// guards: a rollback from a session file left by an earlier process still has
/// to know what the agent put into a file it created, or it cannot tell an
/// untouched creation from work a person added afterwards.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    /// The path as the repository names it, and its contents beforehand; `None`
    /// is the record that the file did not exist and so must be removed again.
    prior: Vec<(String, Option<String>)>,
    written: std::collections::BTreeMap<String, String>,
}

impl Snapshot {
    /// The files this snapshot can put back, in the order they were taken.
    pub fn files(&self) -> Vec<PathBuf> {
        self.prior
            .iter()
            .map(|(path, _)| PathBuf::from(path))
            .collect()
    }

    pub fn len(&self) -> usize {
        self.prior.len()
    }

    pub fn is_empty(&self) -> bool {
        self.prior.is_empty()
    }

    fn capture(&mut self, root: &Path, relative: &str) {
        if self.prior.iter().any(|(known, _)| known == relative) {
            return;
        }
        let prior = std::fs::read_to_string(root.join(relative)).ok();
        self.prior.push((relative.to_owned(), prior));
    }

    /// Put the tree back.
    ///
    /// A file this snapshot created is only removed while it still holds what
    /// the agent wrote: if a person has edited it since, that work is not the
    /// agent's to delete, and the answer is to say so rather than to remove it.
    pub fn restore(&self, root: &Path) -> Result<Restored, PatchError> {
        let mut restored = Restored::default();
        for (relative, prior) in &self.prior {
            let target = inside(root, relative)?;
            let outcome = match (prior, self.written.get(relative)) {
                (Some(text), _) => write_back(&target, relative, Some(text)),
                (None, Some(what_we_wrote)) => {
                    let now = std::fs::read_to_string(&target).ok();
                    if now.as_deref() == Some(what_we_wrote.as_str()) {
                        std::fs::remove_file(&target)
                            .map(|()| true)
                            .map_err(|source| PatchError::Unwritable {
                                path: relative.clone(),
                                reason: source.to_string(),
                            })
                    } else {
                        restored.left_alone.push(PathBuf::from(relative));
                        Ok(false)
                    }
                }
                (None, None) => Ok(false),
            }?;
            if outcome {
                restored.paths.push(target);
            }
        }
        Ok(restored)
    }
}

/// Put a journal's worth of snapshots back, newest first.
///
/// A run undoes from the end because the last write is the one that assumed
/// everything before it. A snapshot that cannot be restored is reported and
/// skipped: the rest of the tree still goes back.
pub fn restore_all(root: &Path, snapshots: &[Snapshot]) -> Vec<Restored> {
    snapshots
        .iter()
        .rev()
        .filter_map(|snapshot| match snapshot.restore(root) {
            Ok(restored) => Some(restored),
            Err(source) => {
                tracing::warn!(%source, "a write could not be rolled back");
                None
            }
        })
        .collect()
}

fn write_back(target: &Path, relative: &str, text: Option<&String>) -> Result<bool, PatchError> {
    match text {
        Some(text) => std::fs::write(target, text)
            .map(|()| true)
            .map_err(|source| PatchError::Unwritable {
                path: relative.to_owned(),
                reason: source.to_string(),
            }),
        None => Ok(false),
    }
}

/// What a rollback did, and what it refused to do.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Restored {
    pub paths: Vec<PathBuf>,
    pub left_alone: Vec<PathBuf>,
}

/// A path the repository owns: relative, no parent escapes, inside the root.
fn inside(root: &Path, path: &str) -> Result<PathBuf, PatchError> {
    if path.is_empty() {
        return Err(PatchError::Malformed(
            "a patch names an empty path".to_owned(),
        ));
    }
    let candidate = Path::new(path);
    if candidate.is_absolute() {
        return Err(PatchError::OutsideRepository(path.to_owned()));
    }
    for component in candidate.components() {
        if matches!(component, Component::ParentDir | Component::RootDir) {
            return Err(PatchError::OutsideRepository(path.to_owned()));
        }
    }
    Ok(root.join(candidate))
}

/// The same answer for a path that has already been joined to a root.
fn deny_outside(root: &Path, target: &Path) -> Result<String, PatchError> {
    target
        .strip_prefix(root)
        .map(|relative| relative.to_string_lossy().replace('\\', "/"))
        .map_err(|_| PatchError::OutsideRepository(target.display().to_string()))
}

fn read(target: &Path, path: &str) -> Result<String, PatchError> {
    if target.is_dir() {
        return Err(PatchError::Unreadable {
            path: path.to_owned(),
            reason: "it is a directory".to_owned(),
        });
    }
    std::fs::read_to_string(target).map_err(|source| PatchError::Unreadable {
        path: path.to_owned(),
        reason: source.to_string(),
    })
}

/// Read a patch into files and their hunks.
fn parse(patch: &str) -> Result<Vec<Section>, PatchError> {
    let mut sections: Vec<Section> = Vec::new();
    let mut old_side: Option<String> = None;
    let mut line_of = 0usize;
    let mut inside_hunk = false;
    for raw in patch.lines() {
        line_of += 1;
        if raw.is_empty() {
            // A blank line inside a hunk is a context line whose content is
            // empty — editors that strip trailing whitespace turn ` ` into ``.
            // Anywhere else it is formatting.
            if inside_hunk {
                push_line(&mut sections, line_of, raw, Line::Context(String::new()))?;
            }
            continue;
        }
        if raw.starts_with("@@") {
            let section = sections.last_mut().ok_or(PatchError::Malformed(format!(
                "line {line_of}: a hunk before any file names it"
            )))?;
            section.hunks.push(Hunk {
                old_start: header(raw)?,
                lines: Vec::new(),
                ends_without_newline: false,
            });
            inside_hunk = true;
            continue;
        }
        if let Some(rest) = raw.strip_prefix("--- ") {
            old_side = Some(strip_side(rest));
            inside_hunk = false;
            continue;
        }
        if let Some(rest) = raw.strip_prefix("+++ ") {
            let new_side = strip_side(rest);
            let kind = match old_side.as_deref() {
                Some(DEV_NULL) => Kind::Added,
                _ if new_side == DEV_NULL => Kind::Deleted,
                _ => Kind::Modified,
            };
            let path = if new_side == DEV_NULL {
                old_side.clone().unwrap_or_default()
            } else {
                new_side
            };
            if path.is_empty() || path == DEV_NULL {
                return Err(PatchError::Malformed(format!(
                    "line {line_of}: a patch of a file with no name"
                )));
            }
            sections.push(Section {
                path,
                kind,
                hunks: Vec::new(),
            });
            inside_hunk = false;
            continue;
        }
        if raw.starts_with("diff --git ")
            || raw.starts_with("index ")
            || raw.starts_with("similarity ")
            || raw.starts_with("rename ")
            || raw.starts_with("new file ")
            || raw.starts_with("deleted file ")
        {
            // git's own bookkeeping says nothing this crate applies.
            continue;
        }
        if raw.starts_with('\\') {
            if let Some(section) = sections.last_mut() {
                if let Some(hunk) = section.hunks.last_mut() {
                    hunk.ends_without_newline = true;
                }
            }
            continue;
        }
        let line = if let Some(rest) = raw.strip_prefix(' ') {
            Line::Context(rest.to_owned())
        } else if let Some(rest) = raw.strip_prefix('+') {
            Line::Added(rest.to_owned())
        } else if let Some(rest) = raw.strip_prefix('-') {
            Line::Removed(rest.to_owned())
        } else {
            return Err(PatchError::Malformed(format!(
                "line {line_of} is neither a header, a hunk line nor a path: `{raw}`"
            )));
        };
        push_line(&mut sections, line_of, raw, line)?;
    }
    if sections.is_empty() {
        return Err(PatchError::Malformed(
            "it names no file, so there is nothing to apply".to_owned(),
        ));
    }
    for section in &sections {
        if section.hunks.is_empty() {
            return Err(PatchError::Malformed(format!(
                "{} has no hunk to apply",
                section.path
            )));
        }
    }
    Ok(sections)
}

fn push_line(
    sections: &mut [Section],
    line_of: usize,
    raw: &str,
    line: Line,
) -> Result<(), PatchError> {
    let message = format!("line {line_of}: `{raw}` is outside any hunk");
    let section = sections
        .last_mut()
        .ok_or_else(|| PatchError::Malformed(message.clone()))?;
    let hunk = section
        .hunks
        .last_mut()
        .ok_or(PatchError::Malformed(message))?;
    hunk.lines.push(line);
    Ok(())
}

/// `@@ -3,7 +4,9 @@` — the line the old side starts at, counting from 1.
///
/// The declared lengths are not checked. A hunk that runs short is caught by
/// the content match, which is the check that matters; refusing a patch because
/// a model miscounted would only teach it to stop using diffs.
fn header(rest: &str) -> Result<usize, PatchError> {
    let ranges = rest
        .trim_start_matches('@')
        .split('@')
        .next()
        .unwrap_or_default()
        .trim();
    let old = ranges
        .split_whitespace()
        .next()
        .ok_or_else(|| PatchError::Malformed(format!("`{rest}` is not a hunk header")))?;
    let start = old.trim_start_matches('-').split(',').next().unwrap_or("0");
    let number: usize = start
        .parse()
        .map_err(|_| PatchError::Malformed(format!("`{old}` is not a hunk range")))?;
    Ok(number.max(1))
}

/// One side of a `---`/`+++` pair, with the conventional `a/` and `b/` stripped
/// the way `patch -p1` does.
fn strip_side(raw: &str) -> String {
    let raw = raw.split('\t').next().unwrap_or(raw).trim();
    if raw == DEV_NULL {
        return raw.to_owned();
    }
    match raw.split_once('/') {
        Some((prefix, rest)) if (prefix == "a" || prefix == "b") && !rest.is_empty() => {
            rest.to_owned()
        }
        _ => raw.to_owned(),
    }
}

/// The text a section produces when it is applied to `before`.
fn apply_section(section: &Section, before: &str) -> Result<String, PatchError> {
    // A new file has no last byte to copy, so only the patch's own
    // `\ No newline` marker can say otherwise.
    let mut ends_with_newline = before.ends_with('\n') || section.kind == Kind::Added;
    let mut lines: Vec<String> = before.lines().map(ToOwned::to_owned).collect();
    let mut shift = 0isize;
    for hunk in &section.hunks {
        let pattern: Vec<&str> = hunk
            .lines
            .iter()
            .filter(|line| !matches!(line, Line::Added(_)))
            .map(Line::text)
            .collect();
        let wanted = expected_line(hunk, &pattern);
        let hint = if pattern.is_empty() {
            (hunk.old_start as isize - 1 + shift).max(0) as usize
        } else {
            locate(
                &lines,
                &pattern,
                hunk.old_start,
                shift,
                &section.path,
                &wanted,
            )?
        };
        let mut replacement: Vec<String> = Vec::new();
        let mut consumed = 0usize;
        for line in &hunk.lines {
            match line {
                Line::Context(_) => {
                    replacement.push(lines[hint + consumed].clone());
                    consumed += 1;
                }
                Line::Removed(_) => {
                    consumed += 1;
                }
                Line::Added(text) => replacement.push(text.clone()),
            }
        }
        if !pattern.is_empty() && consumed != pattern.len() {
            return Err(PatchError::Malformed(format!(
                "{}: a hunk consumes {consumed} line(s) of {} named",
                section.path,
                pattern.len()
            )));
        }
        if consumed > lines.len() - hint {
            return Err(PatchError::ContextMismatch {
                path: section.path.clone(),
                expected: wanted.to_owned(),
                line: hunk.old_start,
            });
        }
        let produced = replacement.len() as isize;
        lines.splice(hint..hint + consumed, replacement);
        shift += produced - consumed as isize;
        if hunk.ends_without_newline {
            ends_with_newline = false;
        }
    }
    let mut text = lines.join("\n");
    if ends_with_newline {
        text.push('\n');
    }
    Ok(text)
}

/// The line a refusal should name: what the hunk changes if it changes one,
/// otherwise the first line it expects to find. "the context did not match" is
/// not an answer a model can act on.
fn expected_line(hunk: &Hunk, pattern: &[&str]) -> String {
    hunk.lines
        .iter()
        .find_map(|line| match line {
            Line::Removed(text) => Some(text.clone()),
            _ => None,
        })
        .or_else(|| pattern.first().map(|text| (*text).to_owned()))
        .unwrap_or_default()
}

/// Where a hunk's lines actually are, which is not always where its header says.
fn locate(
    lines: &[String],
    pattern: &[&str],
    old_start: usize,
    shift: isize,
    path: &str,
    wanted: &str,
) -> Result<usize, PatchError> {
    if pattern.len() > lines.len() {
        return Err(PatchError::ContextMismatch {
            path: path.to_owned(),
            expected: wanted.to_owned(),
            line: old_start,
        });
    }
    let hint = (old_start as isize - 1 + shift).max(0) as usize;
    for start in hint..=lines.len() - pattern.len() {
        if matches(lines, pattern, start, false) {
            return Ok(start);
        }
    }
    for start in 0..hint.min(lines.len() - pattern.len() + 1) {
        if matches(lines, pattern, start, false) {
            return Ok(start);
        }
    }
    for start in 0..=lines.len() - pattern.len() {
        if matches(lines, pattern, start, true) {
            return Ok(start);
        }
    }
    Err(PatchError::ContextMismatch {
        path: path.to_owned(),
        expected: wanted.to_owned(),
        line: old_start,
    })
}

fn matches(lines: &[String], pattern: &[&str], at: usize, ignore_trailing_space: bool) -> bool {
    pattern.iter().enumerate().all(|(offset, want)| {
        let have = &lines[at + offset];
        if ignore_trailing_space {
            have.trim_end() == want.trim_end()
        } else {
            have == want
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> String {
        "use std::fmt;\n\npub struct Plan {\n    pub goal: String,\n    pub steps: Vec<String>,\n}\n\nimpl Plan {\n    pub fn new(goal: &str) -> Self {\n        Self { goal: goal.to_owned(), steps: Vec::new() }\n    }\n}\n\npub fn render(plan: &Plan) -> String {\n    format!(\"{}\", plan.goal)\n}\n"
            .to_owned()
    }

    #[test]
    fn a_diff_round_trips_through_the_parser_it_writes() {
        let before = sample();
        let after = before.replace("pub goal: String", "pub goal: String,\n    pub id: u64");
        let patch = diff("src/plan.rs", &before, &after);
        assert!(patch.contains("--- a/src/plan.rs"), "{patch}");
        assert!(patch.contains("+++ b/src/plan.rs"), "{patch}");
        assert!(patch.contains("@@ -2,6 +2,7 @@"), "{patch}");
        assert_eq!(apply(&before, &patch).expect("applies"), after);
    }

    #[test]
    fn a_patch_naming_several_files_is_not_applied_to_only_one_of_them() {
        let before = sample();
        let patch = diff(
            "plan.rs",
            &before,
            &before.replace("pub fn render", "pub fn draw"),
        ) + diff("other.rs", "", "fn other() {}\n").as_str();
        let error = apply(&before, &patch).expect_err("which file did it mean?");
        assert!(
            matches!(error, PatchError::Malformed(ref text) if text.contains("2 files")),
            "{error}"
        );
    }

    #[test]
    fn an_unchanged_file_produces_no_diff_and_no_change() {
        let text = sample();
        assert_eq!(diff("plan.rs", &text, &text), "");
        assert!(apply(&text, "").is_err(), "an empty patch is not a change");
    }

    #[test]
    fn a_hunk_is_applied_where_its_context_is_not_where_its_header_says() {
        let before = sample();
        let patch = diff(
            "plan.rs",
            &before,
            &before.replace("pub fn render", "pub fn draw"),
        );
        // Five lines of unrelated work above the hunk, so the header lies.
        let moved = format!("// one\n// two\n// three\n// four\n// five\n{before}");
        let applied = apply(&moved, &patch).expect("a moved hunk still lands");
        assert!(applied.starts_with("// one\n// two"), "{applied}");
        assert!(applied.contains("pub fn draw(plan: &Plan)"), "{applied}");
        assert!(!applied.contains("pub fn render"), "{applied}");
    }

    #[test]
    fn a_hunk_whose_context_is_absent_is_refused_naming_the_line_it_wanted() {
        let before = sample();
        let patch = diff(
            "plan.rs",
            &before,
            &before.replace("pub fn render", "pub fn draw"),
        );
        let other = "totally different file\nwith nothing in common\n".to_owned();
        let error = apply(&other, &patch).expect_err("no context, no apply");
        let text = error.to_string();
        assert!(text.contains("plan.rs"), "{text}");
        assert!(
            text.contains("pub fn render"),
            "it says what it looked for: {text}"
        );
    }

    #[test]
    fn a_patch_that_is_not_a_diff_says_so() {
        for patch in [
            "just some prose\nabout the change\n",
            "@@ -1,2 +1,2 @@\n a\n b\n",
            "--- a/x\n+++ b/x\n",
            "--- a/x\n+++ b/x\n@@ nope @@\n x\n",
            "--- a/x\n+++ b/x\n@@ -1 +1 @@\nthe line lost its leading marker\n",
        ] {
            assert!(
                matches!(apply("x\n", patch), Err(PatchError::Malformed(_))),
                "{patch:?} should be malformed, got {:?}",
                apply("x\n", patch)
            );
        }
    }

    #[test]
    fn a_well_formed_patch_against_a_changed_file_reports_a_mismatch_not_a_parse_error() {
        let error = apply("x\n", "--- a/x\n+++ b/x\n@@ -1 +1 @@\n-gone\n+y\n")
            .expect_err("the file moved on");
        assert!(
            matches!(error, PatchError::ContextMismatch { ref path, ref expected, .. }
                if path == "x" && expected == "gone"),
            "{error}"
        );
    }

    #[test]
    fn trailing_space_only_differences_are_still_matched() {
        let patch = "--- a/x.rs\n+++ b/x.rs\n@@ -1,2 +1,2 @@\n use a;\n-old \n+new\n";
        let before = "use a;\nold\nsecond\n";
        let applied = apply(before, patch).expect("trailing space is not content");
        assert_eq!(applied, "use a;\nnew\nsecond\n", "{applied}");
    }

    #[test]
    fn a_blank_context_line_that_lost_its_space_is_still_counted_as_a_line() {
        // Editors strip trailing whitespace, which turns the ` ` of an empty
        // context line into nothing. Dropping it would shift the whole hunk.
        let patch = "--- a/x.md\n+++ b/x.md\n@@ -1,3 +1,3 @@\n # one\n\n-two\n+three\n";
        let applied = apply("# one\n\ntwo\n", patch).expect("the blank line counts");
        assert_eq!(applied, "# one\n\nthree\n", "{applied}");
    }

    #[test]
    fn an_added_file_is_read_as_empty_and_a_deleted_one_as_gone() {
        let dir = tempfile::tempdir().expect("a temp repo");
        std::fs::write(dir.path().join("old.rs"), "fn gone() {}\n").expect("writable");
        let patch =
            "--- /dev/null\n+++ b/new.rs\n@@ -0,0 +1,2 @@\n+pub fn added() {}\n+// second\n";
        let planned = plan(dir.path(), patch).expect("a plan");
        assert_eq!(planned.len(), 1);
        assert_eq!(planned[0].kind, Kind::Added);
        assert_eq!(planned[0].stat.added, 2);
        assert_eq!(
            planned[0].content.as_deref(),
            Some("pub fn added() {}\n// second\n")
        );

        let removal = "--- a/old.rs\n+++ /dev/null\n@@ -1,1 +0,0 @@\n-fn gone() {}\n";
        let planned = plan(dir.path(), removal).expect("a plan");
        assert_eq!(planned[0].kind, Kind::Deleted);
        assert_eq!(planned[0].content, None);
        assert!(plan(
            dir.path(),
            "--- a/nope.rs\n+++ /dev/null\n@@ -1 +0,0 @@\n-x\n"
        )
        .is_err());
    }

    #[test]
    fn a_patch_naming_a_path_outside_the_repository_is_refused() {
        let dir = tempfile::tempdir().expect("a temp repo");
        let patch = "--- a/../escape.rs\n+++ b/../escape.rs\n@@ -1 +1 @@\n-x\n+y\n";
        let error = plan(dir.path(), patch).expect_err("no escape");
        assert!(
            matches!(error, PatchError::OutsideRepository(ref path) if path == "../escape.rs"),
            "{error}"
        );
        assert!(
            !dir.path().join("../escape.rs").exists(),
            "nothing was written"
        );
    }

    #[test]
    fn a_write_can_be_put_back_including_the_files_it_created() {
        let dir = tempfile::tempdir().expect("a temp repo");
        let before = sample();
        std::fs::create_dir_all(dir.path().join("src")).expect("writable");
        std::fs::write(dir.path().join("src/plan.rs"), &before).expect("writable");

        let patch = diff(
            "src/plan.rs",
            &before,
            &before.replace("pub fn render", "pub fn draw"),
        ) + diff("src/new.rs", "", "grown\nfrom a patch\n").as_str();
        let planned = plan(dir.path(), &patch).expect("a plan");
        assert_eq!(planned[1].kind, Kind::Added);
        let snapshot = write(dir.path(), &planned).expect("written");
        assert_eq!(
            snapshot.files(),
            vec![PathBuf::from("src/plan.rs"), PathBuf::from("src/new.rs")],
            "{:?}",
            snapshot.files()
        );
        assert!(std::fs::read_to_string(dir.path().join("src/plan.rs"))
            .expect("readable")
            .contains("pub fn draw"));

        let restored = snapshot.restore(dir.path()).expect("restored");
        assert_eq!(restored.paths.len(), 2, "{restored:?}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("src/plan.rs")).expect("readable"),
            before
        );
        assert!(
            !dir.path().join("src/new.rs").exists(),
            "a created file goes back to absent"
        );

        // A file a person has written since the agent created it is not the
        // agent's to remove.
        std::fs::write(dir.path().join("src/new.rs"), "mine now\n").expect("writable");
        let left = snapshot.restore(dir.path()).expect("still restores");
        assert_eq!(
            left.left_alone,
            vec![PathBuf::from("src/new.rs")],
            "{left:?}"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("src/new.rs")).expect("readable"),
            "mine now\n"
        );
    }

    #[test]
    fn a_snapshot_read_back_from_a_file_still_protects_a_persons_edit() {
        let dir = tempfile::tempdir().expect("a temp repo");
        let planned = plan(dir.path(), &diff("notes.md", "", "first\n")).expect("a plan");
        let snapshot = write(dir.path(), &planned).expect("written");

        // The journal a session file carries, decoded by a later process.
        let json = serde_json::to_string(&snapshot).expect("serialisable");
        let kept: Snapshot = serde_json::from_str(&json).expect("deserialisable");
        assert_eq!(kept, snapshot, "nothing a rollback needs is dropped");

        std::fs::write(dir.path().join("notes.md"), "someone else's work\n").expect("writable");
        let restored = restore_all(dir.path(), std::slice::from_ref(&kept));
        assert_eq!(
            restored[0].left_alone,
            vec![PathBuf::from("notes.md")],
            "a created file is only removed while it still holds what the agent wrote"
        );
        assert!(dir.path().join("notes.md").exists());

        std::fs::write(dir.path().join("notes.md"), "first\n").expect("writable");
        let restored = restore_all(dir.path(), &[kept]);
        assert_eq!(restored[0].paths.len(), 1, "{restored:?}");
        assert!(
            !dir.path().join("notes.md").exists(),
            "untouched, it goes back to absent"
        );
    }

    #[test]
    fn a_journal_undoes_from_the_newest_write_back() {
        let dir = tempfile::tempdir().expect("a temp repo");
        let mut journal = Vec::new();
        for name in ["one.md", "two.md"] {
            let planned = plan(
                dir.path(),
                &diff(name, "", format!("{name} body\n").as_str()),
            )
            .expect("a plan");
            journal.push(write(dir.path(), &planned).expect("written"));
        }
        assert!(dir.path().join("two.md").is_file());

        let restored = restore_all(dir.path(), &journal);
        assert_eq!(restored.len(), 2);
        assert!(
            restored[0].paths[0].ends_with("two.md"),
            "the last write is undone first: {:?}",
            restored[0].paths
        );
        assert!(
            !dir.path().join("one.md").exists() && !dir.path().join("two.md").exists(),
            "everything the journal created is gone"
        );
    }

    #[test]
    fn a_diff_of_a_file_on_disk_against_new_text_is_a_patch_of_it() {
        let dir = tempfile::tempdir().expect("a temp repo");
        std::fs::write(dir.path().join("notes.md"), "# one\n\ntwo\n").expect("writable");
        let patch =
            diff_against_disk(dir.path(), "notes.md", "# one\n\ntwo and a half\n").expect("a diff");
        assert!(patch.contains("--- a/notes.md"), "{patch}");
        let planned = plan(dir.path(), &patch).expect("planned");
        assert_eq!(planned[0].kind, Kind::Modified);
        write(dir.path(), &planned).expect("written");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("notes.md")).expect("readable"),
            "# one\n\ntwo and a half\n"
        );
    }

    #[test]
    fn stats_count_the_lines_a_patch_moves() {
        let before = sample();
        let after = before
            .replace("pub fn render", "pub fn draw")
            .replace("impl Plan {", "impl Plan {\n    pub fn mark(&self) {}\n");
        let stats = stats(&diff("plan.rs", &before, &after)).expect("two hunks");
        assert_eq!(stats.len(), 1);
        assert_eq!(stats[0].path, "plan.rs");
        assert_eq!(stats[0].kind, Kind::Modified);
        assert_eq!(stats[0].added, 3, "{stats:?}");
        assert_eq!(stats[0].removed, 1, "{stats:?}");
        assert_eq!(
            touched_paths(&diff("x.rs", "a\n", "b\n")).expect("one path"),
            vec!["x.rs"]
        );
    }

    #[test]
    fn a_multi_file_patch_plans_every_file_in_it() {
        let dir = tempfile::tempdir().expect("a temp repo");
        std::fs::write(dir.path().join("one.rs"), "fn one() {}\n").expect("writable");
        std::fs::write(dir.path().join("two.rs"), "fn two() {}\n").expect("writable");
        let patch = diff("one.rs", "fn one() {}\n", "fn one() { }\n")
            + &diff("two.rs", "fn two() {}\n", "fn three() {}\n");
        let planned = plan(dir.path(), &patch).expect("both files");
        assert_eq!(planned.len(), 2);
        assert_eq!(planned[1].content.as_deref(), Some("fn three() {}\n"));
        assert_eq!(
            planned[0].content.as_deref(),
            Some("fn one() { }\n"),
            "the first file keeps its place"
        );
        let snapshot = write(dir.path(), &planned).expect("written");
        assert_eq!(
            snapshot.files(),
            vec![PathBuf::from("one.rs"), PathBuf::from("two.rs")]
        );
    }

    #[test]
    fn a_file_without_a_final_newline_stays_without_one() {
        let patch = "--- a/loose.rs\n+++ b/loose.rs\n@@ -1,1 +1,2 @@\n-first\n+first\n+second\n\\ No newline at end of file\n";
        let applied = apply("first", patch).expect("applied");
        assert_eq!(applied, "first\nsecond", "{applied}");
    }

    #[test]
    fn paths_are_joined_to_the_root_only_when_they_stay_inside_it() {
        let dir = Path::new("/repo");
        assert_eq!(
            inside(dir, "src/lib.rs").expect("inside"),
            PathBuf::from("/repo/src/lib.rs")
        );
        assert_eq!(
            inside(dir, "./src/lib.rs").expect("inside"),
            PathBuf::from("/repo/src/lib.rs")
        );
        assert!(matches!(
            inside(dir, "../x"),
            Err(PatchError::OutsideRepository(_))
        ));
        assert!(matches!(
            inside(dir, "/etc/passwd"),
            Err(PatchError::OutsideRepository(_))
        ));
        assert!(matches!(inside(dir, ""), Err(PatchError::Malformed(_))));
        assert_eq!(
            deny_outside(dir, Path::new("/repo/src/lib.rs")).expect("relative"),
            "src/lib.rs"
        );
        assert!(matches!(
            deny_outside(dir, Path::new("/somewhere/else")),
            Err(PatchError::OutsideRepository(_))
        ));
    }
}
