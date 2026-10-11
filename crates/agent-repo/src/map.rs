//! The repository as a table of contents, and the pages a task is about.
//!
//! A 1B model given a tree of 3,000 files will spend its whole window on paths
//! and still not know where the logic lives. So the map is deliberately not a
//! file list: it is one line per file, grouped by directory, naming what each
//! file declares rather than what it contains, and it stops when the budget is
//! gone and says how much it left out.
//!
//! The ranking is the other half. It never claims to understand a task; it
//! scores shape — the words in a path, the names a file declares — and hands
//! back the few files that look like the ones asked for, with the reason each
//! was chosen, because when the wrong file is picked the reason is what tells
//! you whether the ranking was wrong or the model was.

use std::collections::BTreeSet;
use std::io::Read;

use serde::{Deserialize, Serialize};
use smollm_core::chat::approx_token_count;

use crate::index::{FileEntry, Index, Language};
use crate::project::Project;

/// How much of a file is read when the point is to name its declarations.
///
/// The interesting declarations of a source file are near its top, and reading
/// a megabyte to find out what a file is would cost the context it saves.
pub const HEAD_BYTES: u64 = 8 * 1024;

/// The most files one ranking will read, whatever the tree says.
///
/// A ranking that reads every file to decide which files to read is not a
/// ranking, it is a build. Past this many the remaining files are scored on
/// their path alone, which is cheaper and says so by scoring less.
pub const MAX_SCANS: usize = 500;

/// What the map knows about one file, ready to print.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RepoMap {
    pub text: String,
    pub listed: usize,
    pub omitted: usize,
    /// The tokens the text cost, which is never more than the budget asked for.
    pub tokens: usize,
}

impl RepoMap {
    /// Group the tree by directory, name what each file declares, stop at the
    /// budget.
    ///
    /// With a task, the directories that hold the files the task points at come
    /// first, because a map that runs out of budget should run out after the
    /// part that matters. Without one everything is alphabetical, which is the
    /// order a reader can predict.
    pub fn build(index: &Index, project: &Project, task: Option<&str>, budget: usize) -> Self {
        let scores = task.map(|task| rank(index, task, index.len()));
        let header = format!("{} — {}", project.describe(), index.describe());
        let mut tokens = count(&header);
        if tokens > budget {
            return Self {
                text: String::new(),
                listed: 0,
                omitted: index.len(),
                tokens: 0,
            };
        }
        let mut text = header + "\n";

        let mut groups: Vec<(String, Vec<&FileEntry>)> = Vec::new();
        for entry in &index.files {
            let directory = entry.directory().to_owned();
            match groups.iter_mut().find(|(known, _)| *known == directory) {
                Some((_, entries)) => entries.push(entry),
                None => groups.push((directory, vec![entry])),
            }
        }
        groups.sort_by(|left, right| {
            group_score(right, scores.as_ref())
                .cmp(&group_score(left, scores.as_ref()))
                .then_with(|| depth(&left.0).cmp(&depth(&right.0)))
                .then_with(|| left.0.cmp(&right.0))
        });

        let mut listed = 0usize;
        let mut omitted = 0usize;
        let mut scans = 0usize;
        let mut stopped = false;
        // The note about what was left out costs tokens too, so the blocks are
        // measured against the budget minus the note rather than the whole of it.
        let room = budget.saturating_sub(NOTE_COST);
        for (directory, entries) in &groups {
            if stopped {
                omitted += entries.len();
                continue;
            }
            let mut lines: Vec<String> = Vec::new();
            for entry in entries {
                let names = if scans < MAX_SCANS && declares_things(entry.language()) {
                    scans += 1;
                    head(index, entry)
                        .map(|text| declarations(&text, entry.language(), SYMBOLS_PER_FILE))
                        .unwrap_or_default()
                } else {
                    Vec::new()
                };
                lines.push(format!(
                    "  {} ({})",
                    entry.file_name(),
                    summary(entry, names)
                ));
            }
            let mut title = if directory.is_empty() {
                "./".to_owned()
            } else {
                format!("{directory}/")
            };
            if entries.iter().all(|entry| is_test(&entry.path)) {
                title.push_str(" [where the tests are]");
            }
            let block = std::iter::once(title)
                .chain(lines)
                .collect::<Vec<_>>()
                .join("\n");
            let cost = count(&block);
            if tokens + cost > room {
                stopped = true;
                omitted += entries.len();
                continue;
            }
            tokens += cost;
            listed += entries.len();
            text.push_str(&block);
            text.push('\n');
        }
        if omitted > 0 {
            let note = format!("+ {omitted} file(s) not shown: the map budget is {budget} tokens");
            text.push_str(&note);
            text.push('\n');
            tokens += count(&note);
        }
        Self {
            text,
            listed,
            omitted,
            tokens,
        }
    }
}

/// The room one note about omitted files takes, counted generously.
const NOTE_COST: usize = 24;

/// Whether a file of this kind says what it holds by its own shape.
///
/// Prose and manifests are not code, but a heading and a table header are still
/// a table of contents, and a lockfile or an image has neither.
fn declares_things(language: Language) -> bool {
    matches!(
        language,
        Language::Rust
            | Language::Python
            | Language::JavaScript
            | Language::TypeScript
            | Language::Shell
            | Language::Markdown
            | Language::Toml
    )
}

/// A file's line in the map: what it declares, or how big it is.
fn summary(entry: &FileEntry, names: Vec<String>) -> String {
    if is_test(&entry.path) {
        return "test".to_owned();
    }
    if !names.is_empty() {
        return names.join(", ");
    }
    if entry.size > HEAD_BYTES {
        format!("{} kB", entry.size / 1024 + 1)
    } else {
        format!("{} B", entry.size)
    }
}

/// The declarations one file's line is allowed to name.
const SYMBOLS_PER_FILE: usize = 6;

fn count(text: &str) -> usize {
    approx_token_count(text) as usize
}

fn depth(directory: &str) -> usize {
    directory
        .split('/')
        .filter(|part| !part.is_empty() && *part != ".")
        .count()
}

fn group_score(group: &(String, Vec<&FileEntry>), scores: Option<&Vec<Scored>>) -> usize {
    let Some(scores) = scores else { return 0 };
    let mut top = 0usize;
    for entry in &group.1 {
        if let Some(scored) = scores.iter().find(|scored| scored.path == entry.path) {
            top = top.max(scored.score);
        }
    }
    top
}

/// The first bytes of a file as text, or nothing at all.
fn head(index: &Index, entry: &FileEntry) -> Option<String> {
    let limit = usize::try_from(HEAD_BYTES.max(1)).ok()?;
    let mut file = std::fs::File::open(index.root.join(&entry.path)).ok()?;
    let mut buffer = vec![0u8; limit];
    let read = file.read(&mut buffer).ok()?;
    buffer.truncate(read);
    String::from_utf8(buffer).ok()
}

/// Whether a path is test code rather than the thing under test.
pub fn is_test(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    path.contains("/tests/")
        || path.starts_with("tests/")
        || name.starts_with("test_")
        || name.ends_with("_test.rs")
        || name.ends_with("_test.py")
        || name.ends_with(".test.ts")
        || name.ends_with(".test.tsx")
        || name.ends_with(".test.js")
        || name.ends_with(".spec.ts")
        || name.ends_with(".spec.js")
}

/// The names a file declares, in the order they appear.
///
/// Only column-zero lines count, which is what makes this cheap and also what
/// makes it miss a method: a table of contents lists chapters, not paragraphs.
pub fn declarations(text: &str, language: Language, limit: usize) -> Vec<String> {
    declarations_at(text, language, limit)
        .into_iter()
        .map(|declaration| declaration.name)
        .collect()
}

/// One declaration, with enough of it to be useful in a listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Declaration {
    pub name: String,
    /// 1-based, the same count `read_file` and `search_text` print.
    pub line: usize,
    /// The whole line it was found on, trimmed: for code this is the signature.
    pub text: String,
}

/// [`declarations`] with the line each name lives on, which is what a tool that
/// points a model at a definition needs and a repo map that only names files
/// does not.
pub fn declarations_at(text: &str, language: Language, limit: usize) -> Vec<Declaration> {
    let prefixes: &[&str] = match language {
        Language::Rust => &[
            "pub fn ",
            "pub struct ",
            "pub enum ",
            "pub trait ",
            "pub mod ",
            "pub const ",
            "pub use ",
            "fn ",
            "struct ",
            "enum ",
            "trait ",
            "mod ",
            "const ",
            "static ",
            "type ",
        ],
        Language::Python => &["def ", "async def ", "class "],
        Language::JavaScript | Language::TypeScript => &[
            "export function ",
            "export const ",
            "export default ",
            "export class ",
            "function ",
            "class ",
            "const ",
        ],
        Language::Shell => &["function "],
        _ => &[],
    };
    if prefixes.is_empty() {
        // Prose and data get their own treatment: a heading is a name, a TOML
        // table header is a name, and neither looks like a declaration.
        return match language {
            Language::Markdown => headings_at(text, limit),
            Language::Toml => tables_at(text, limit),
            _ => Vec::new(),
        };
    }
    let mut names: Vec<Declaration> = Vec::new();
    for (index, line) in text.lines().enumerate() {
        if names.len() >= limit {
            break;
        }
        if line.starts_with([' ', '\t', '#', '/', '*', '[', ']', '}', ')']) {
            continue;
        }
        for prefix in prefixes {
            if let Some(rest) = line.strip_prefix(prefix) {
                if let Some(name) = identifier(rest) {
                    // `mod tests` is not surface; the file and directory are
                    // already marked as tests by `is_test`.
                    if name != "tests" && !names.iter().any(|known| known.name == name) {
                        names.push(Declaration {
                            name,
                            line: index + 1,
                            text: line.trim_end().to_owned(),
                        });
                    }
                }
                break;
            }
        }
    }
    names
}

fn identifier(text: &str) -> Option<String> {
    let name: String = text
        .trim_start()
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '-')
        .collect();
    if name.is_empty() || name.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        return None;
    }
    Some(name)
}

fn headings_at(text: &str, limit: usize) -> Vec<Declaration> {
    text.lines()
        .enumerate()
        .filter_map(|(index, line)| {
            let title = line.strip_prefix('#')?;
            Some((index + 1, line, title.trim_start_matches(['#', ' ']).trim()))
        })
        .filter(|(_, _, title)| !title.is_empty())
        .take(limit)
        .map(|(line, text, title)| Declaration {
            name: title.to_owned(),
            line,
            // The line as the file spells it, so a listing of a Markdown file
            // reads `# Title` and a model can see which kind of line it found.
            text: text.trim_end().to_owned(),
        })
        .collect()
}

fn tables_at(text: &str, limit: usize) -> Vec<Declaration> {
    text.lines()
        .enumerate()
        .filter_map(|(index, line)| {
            line.trim()
                .strip_prefix('[')
                .and_then(|rest| rest.strip_suffix(']'))
                .map(|name| (index + 1, line.trim(), name.trim()))
        })
        .filter(|(_, _, name)| !name.contains(['[', ']', '=', '.']))
        .take(limit)
        .map(|(line, text, name)| Declaration {
            name: name.to_owned(),
            line,
            text: text.to_owned(),
        })
        .collect()
}

/// One file's claim on the task's attention.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Scored {
    pub path: String,
    pub score: usize,
    /// Why this file, in the few words a model gets.
    pub reason: String,
}

/// The words of a task that are worth matching, lowercased and without the
/// grammar around them.
pub fn task_words(task: &str) -> Vec<String> {
    let mut seen = BTreeSet::new();
    for word in task
        .to_lowercase()
        .split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '-'))
    {
        if word.len() < 3 || STOP_WORDS.iter().any(|stop| stop == &word) {
            continue;
        }
        seen.insert(word.to_owned());
    }
    seen.into_iter().collect()
}

/// Words that appear in every task and say nothing about which file it means.
/// The verbs a task opens with — add, fix, change — stay in the list on
/// purpose: they are how a model says "code", and code is what ranks highest.
const STOP_WORDS: [&str; 24] = [
    "the", "and", "for", "with", "that", "this", "from", "have", "has", "are", "was", "will",
    "would", "could", "should", "about", "into", "please", "them", "their", "they", "you", "your",
    "not",
];

/// Rank the files by how much they look like the ones the task means.
///
/// Nothing here understands the task. A word in a file's name is a strong
/// signal, a word in a declaration it makes is a strong one too, and a word in
/// a directory is weak. Prose files pay for being full of every word.
pub fn rank(index: &Index, task: &str, limit: usize) -> Vec<Scored> {
    let words = task_words(task);
    if words.is_empty() {
        return Vec::new();
    }
    let mut scored: Vec<Scored> = Vec::new();
    let mut scans = 0usize;
    for entry in &index.files {
        let path = entry.path.to_lowercase();
        let name = entry.file_name().to_lowercase();
        let mut score = 0usize;
        let mut in_name = 0usize;
        let mut in_path = 0usize;
        for word in &words {
            if name.contains(word.as_str()) {
                score += 50;
                in_name += 1;
            } else if path.contains(word.as_str()) {
                score += 10;
                in_path += 1;
            }
        }
        if entry.language().is_code() {
            score += 8;
        } else {
            // A README, a lockfile or a fixture matches the words of almost any
            // task, so it pays for being likely to be the noise.
            score = score.saturating_sub(12);
        }
        let mut in_declarations = 0usize;
        if scans < MAX_SCANS && entry.language().is_code() {
            scans += 1;
            if let Some(text) = head(index, entry) {
                let names = declarations(&text, entry.language(), 40);
                for word in &words {
                    if names
                        .iter()
                        .any(|declared| declared.to_lowercase().contains(word.as_str()))
                    {
                        score += 25;
                        in_declarations += 1;
                    }
                }
            }
        }
        if score == 0 {
            continue;
        }
        let reason = if in_name > 0 {
            format!("{in_name} of the task's words are in its name")
        } else if in_declarations > 0 {
            format!("it declares {} of the task's words", in_declarations)
        } else if in_path > 0 {
            format!("{in_path} of the task's words are in its directory")
        } else {
            "code, which is what the task asks for".to_owned()
        };
        scored.push(Scored {
            path: entry.path.clone(),
            score,
            reason,
        });
    }
    scored.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.path.cmp(&b.path)));
    scored.truncate(limit);
    scored
}

/// A file worth quoting, with the reason it was chosen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Candidate {
    pub path: String,
    pub content: String,
    pub reason: String,
    pub tokens: usize,
}

/// The files to put in a context bundle, best first, cut at the budget.
///
/// A file whose body does not fit is left out rather than truncated: half a file
/// is worse than none, because a model cannot tell where it stopped.
pub fn context(index: &Index, task: &str, budget: usize, limit: usize) -> Vec<Candidate> {
    let mut chosen: Vec<Candidate> = Vec::new();
    let mut spent = 0usize;
    for scored in rank(index, task, limit.max(1) * 4) {
        if chosen.len() >= limit || spent >= budget {
            break;
        }
        let Some(content) = index.read(&scored.path) else {
            continue;
        };
        let tokens = count(&content);
        if tokens > budget - spent {
            continue;
        }
        spent += tokens;
        chosen.push(Candidate {
            path: scored.path,
            content,
            reason: scored.reason,
            tokens,
        });
    }
    chosen
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::Index;
    use crate::project::Project;
    use std::path::Path;

    fn write(dir: &Path, path: &str, contents: &str) {
        let full = dir.join(path);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).expect("a writable parent");
        }
        std::fs::write(full, contents).expect("writable");
    }

    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("a temp repo");
        write(
            dir.path(),
            "Cargo.toml",
            "[workspace]\nmembers = [\"crates/*\"]\n",
        );
        write(
            dir.path(),
            "crates/one/Cargo.toml",
            "[package]\nname = \"one\"\n",
        );
        write(
            dir.path(),
            "crates/one/src/lib.rs",
            "pub struct Session { pub id: u64 }\npub fn load() -> Session { Session { id: 1 } }\n\tfn hidden() {}\n#[cfg(test)]\nmod tests {\n  #[test]\n  fn t() {}\n}\n",
        );
        write(
            dir.path(),
            "crates/one/tests/one.rs",
            "fn check_session() {}",
        );
        write(
            dir.path(),
            "crates/two/Cargo.toml",
            "[package]\nname = \"two\"\n",
        );
        write(dir.path(), "crates/two/src/main.rs", "fn main() {}");
        write(
            dir.path(),
            "docs/notes.md",
            "# Getting started\n\n## Build\n",
        );
        write(dir.path(), "README.md", "# one\n");
        dir
    }

    fn map_at(dir: &Path, task: Option<&str>, budget: usize) -> RepoMap {
        let index = Index::walk(dir);
        let project = Project::detect(&index);
        RepoMap::build(&index, &project, task, budget)
    }

    #[test]
    fn a_map_names_what_files_declare_rather_than_what_they_contain() {
        let dir = repo();
        let index = Index::walk(dir.path());
        let map = map_at(dir.path(), None, 4096);
        assert!(map.text.contains("crates/one/src/"), "{}", map.text);
        assert!(map.text.contains("Session"), "{}", map.text);
        assert!(map.text.contains("load"), "{}", map.text);
        assert!(
            !map.text.contains("hidden"),
            "an indented declaration is inside something else: {}",
            map.text
        );
        assert!(
            !map.text.contains("mod tests"),
            "a test module is not surface: {}",
            map.text
        );
        assert!(map.text.contains("one.rs (test)"), "{}", map.text);
        assert!(map.text.contains("where the tests are"), "{}", map.text);
        assert!(map.text.contains("(cargo, 2 crates)"), "{}", map.text);
        assert!(
            map.text.contains("validate: cargo test --workspace"),
            "{}",
            map.text
        );
        assert_eq!(
            map.listed,
            index.len(),
            "a generous budget lists everything"
        );
        assert_eq!(map.omitted, 0);
        assert!(map.tokens <= 4096, "{}", map.tokens);
    }

    #[test]
    fn a_tight_budget_stops_and_says_what_it_left_out() {
        let dir = repo();
        let index = Index::walk(dir.path());
        let map = map_at(dir.path(), None, 90);
        assert!(
            map.listed < index.len(),
            "{} listed of {}, the budget was 90",
            map.listed,
            index.len()
        );
        assert!(
            map.omitted > 0 && map.text.contains("not shown"),
            "the reader must know the map is partial: {}",
            map.text
        );
        assert!(map.tokens <= 90, "{} of 90", map.tokens);
        assert_eq!(map.listed + map.omitted, index.len());
    }

    #[test]
    fn a_budget_that_fits_nothing_admits_it_instead_of_lying() {
        let dir = repo();
        let map = map_at(dir.path(), None, 4);
        assert!(map.text.is_empty(), "{}", map.text);
        assert_eq!(map.listed, 0);
        assert_eq!(map.omitted, 8, "every file is unlisted, and it says so");
    }

    #[test]
    fn a_task_moves_the_directories_it_points_at_to_the_front() {
        let dir = repo();
        let index = Index::walk(dir.path());
        let plain = map_at(dir.path(), None, 4096);
        let focused = map_at(dir.path(), Some("change the Session struct"), 4096);
        let position = |text: &str| {
            text.find("crates/one/src/")
                .unwrap_or_else(|| panic!("no crates/one/src in {text}"))
        };
        assert!(
            position(&focused.text) < position(&plain.text),
            "focused:\n{}\nplain:\n{}",
            focused.text,
            plain.text
        );
        assert_eq!(focused.listed + focused.omitted, index.len());
    }

    #[test]
    fn the_ranking_prefers_the_file_the_task_names() {
        let dir = repo();
        let index = Index::walk(dir.path());
        let ranked = rank(&index, "session the load of id from disk", 10);
        assert!(!ranked.is_empty());
        assert_eq!(
            ranked[0].path,
            "crates/one/src/lib.rs",
            "{:?}",
            ranked
                .iter()
                .map(|scored| (&scored.path, scored.score))
                .collect::<Vec<_>>()
        );
        assert!(
            ranked[0].reason.contains("it declares"),
            "{}",
            ranked[0].reason
        );
        assert!(
            ranked
                .iter()
                .all(|scored| !scored.path.ends_with("Cargo.toml")),
            "a manifest is not what the task means: {ranked:?}"
        );
    }

    #[test]
    fn a_name_is_worth_more_than_a_directory() {
        let dir = repo();
        let index = Index::walk(dir.path());
        let ranked = rank(&index, "the two crate's main", 10);
        let top = &ranked[0];
        assert_eq!(top.path, "crates/two/src/main.rs", "{ranked:?}");
        assert!(top.reason.contains("in its name"), "{}", top.reason);
    }

    #[test]
    fn a_task_that_names_nothing_ranks_nothing() {
        let dir = repo();
        let index = Index::walk(dir.path());
        assert!(rank(&index, "do it", 10).is_empty());
        assert!(rank(&index, "", 10).is_empty());
        assert!(rank(&index, "the and for", 10).is_empty());
    }

    #[test]
    fn stop_words_do_not_make_every_file_relevant() {
        assert_eq!(task_words("the and for"), Vec::<String>::new());
        assert_eq!(
            task_words("Fix the session load, please."),
            vec!["fix", "load", "session"],
            "short words and grammar go, the rest stays in order"
        );
    }

    #[test]
    fn context_files_are_whole_and_within_budget() {
        let dir = repo();
        let index = Index::walk(dir.path());
        let chosen = context(&index, "change the Session struct id", 400, 3);
        assert!(!chosen.is_empty(), "the task names a real file");
        assert_eq!(chosen[0].path, "crates/one/src/lib.rs");
        assert!(chosen[0].content.contains("pub struct Session"));
        assert!(!chosen[0].reason.is_empty());
        assert!(
            chosen
                .iter()
                .map(|candidate| candidate.tokens)
                .sum::<usize>()
                <= 400,
            "{:?}",
            chosen
        );

        let nothing = context(&index, "change the Session struct id", 1, 3);
        assert!(nothing.is_empty(), "one token fits no file");
    }

    #[test]
    fn declarations_are_a_language_question() {
        let rust = "pub fn one() {}\nstruct Two;\n\tfn hidden() {}\n// pub fn commented() {}\n";
        assert_eq!(declarations(rust, Language::Rust, 10), vec!["one", "Two"]);
        let python = "class Thing:\n    def method(self):\n        pass\ndef top():\n    pass\n";
        assert_eq!(
            declarations(python, Language::Python, 10),
            vec!["Thing", "top"]
        );
        let typescript = "export function run() {}\nconst value = 1;\n  indented();\n";
        assert_eq!(
            declarations(typescript, Language::TypeScript, 10),
            vec!["run", "value"]
        );
        assert_eq!(
            declarations("# Title\n## Sub\n\ntext\n", Language::Markdown, 10),
            vec!["Title", "Sub"]
        );
        assert_eq!(
            declarations(
                "[package]\nname = \"x\"\n[dependencies]\n[[bin]]\n",
                Language::Toml,
                10
            ),
            vec!["package", "dependencies"]
        );
        assert_eq!(
            declarations("nothing here", Language::Other, 10),
            Vec::<String>::new()
        );
        assert_eq!(
            declarations("pub fn a() {}\npub fn b() {}", Language::Rust, 1),
            vec!["a"]
        );
    }

    /// The line number is the whole reason a tool can call this "where X lives":
    /// a name with no address is a second search away from being useless.
    #[test]
    fn a_declaration_carries_the_line_and_the_line_itself() {
        let text = "use std::fmt;\n\npub struct Plan {\n    pub goal: String,\n}\n\npub fn render(plan: &Plan) -> String {\n    plan.goal.clone()\n}\n";
        let found = declarations_at(text, Language::Rust, 10);
        assert_eq!(
            found
                .iter()
                .map(|declaration| (declaration.name.as_str(), declaration.line))
                .collect::<Vec<_>>(),
            vec![("Plan", 3), ("render", 7)],
            "{found:?}"
        );
        assert_eq!(
            found[1].text, "pub fn render(plan: &Plan) -> String {",
            "the signature is what a model reads to decide whether this is the one"
        );

        // Prose gets a line too, and the heading keeps its `##` in the text.
        let notes = declarations_at("# Title\n\n## Sub\n\ntext\n", Language::Markdown, 10);
        assert_eq!(
            notes
                .iter()
                .map(|declaration| (declaration.name.as_str(), declaration.line))
                .collect::<Vec<_>>(),
            vec![("Title", 1), ("Sub", 3)]
        );

        let tables = declarations_at(
            "[package]\nname = \"x\"\n[dependencies]\n",
            Language::Toml,
            10,
        );
        assert_eq!(
            tables
                .iter()
                .map(|declaration| (
                    declaration.name.as_str(),
                    declaration.line,
                    declaration.text.as_str()
                ))
                .collect::<Vec<_>>(),
            vec![
                ("package", 1, "[package]"),
                ("dependencies", 3, "[dependencies]")
            ],
            "the text is the table header as the file spells it"
        );
    }

    #[test]
    fn a_test_file_is_recognised_by_more_than_its_folder() {
        assert!(is_test("crates/one/tests/one.rs"));
        assert!(is_test("tests/one.rs"));
        assert!(is_test("src/test_thing.py"));
        assert!(is_test("src/thing_test.rs"));
        assert!(is_test("web/app.test.ts"));
        assert!(is_test("web/app.spec.ts"));
        assert!(!is_test("src/lib.rs"));
        assert!(!is_test("src/latest.rs"));
        assert!(!is_test("src/session.rs"));
    }

    #[test]
    fn the_head_of_a_file_is_all_that_is_read() {
        let dir = tempfile::tempdir().expect("a temp repo");
        let long = "pub fn a() {}\n".to_owned() + &"// filler line\n".repeat(4000);
        write(dir.path(), "big.rs", &long);
        let index = Index::walk(dir.path());
        let entry = index.get("big.rs").expect("indexed");
        let text = head(&index, entry).expect("a head");
        assert!(text.len() as u64 <= HEAD_BYTES, "{} bytes", text.len());
        assert!(text.contains("pub fn a"));
        assert_eq!(declarations(&text, Language::Rust, 3), vec!["a"]);
    }
}
