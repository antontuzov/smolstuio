//! Finding things: a string inside files, files by name, or where a name is
//! defined.
//!
//! All three are literal, not regular. A 1.5B model that writes `foo(.*)` means
//! something and gets a message about an unusable pattern; one that writes `foo`
//! means `foo` and gets results. Substring and glob matching cover almost every
//! question an agent asks of a repository, and they are the two that cannot be
//! mis-transcribed by a model that has seen regex in its pretraining.
//!
//! All three walk with the `ignore` crate, so a repository's `.gitignore` is
//! honoured before any file is opened: `target/` holds a copy of every string in
//! the crate, and a search that returned it would be a search that returned
//! nothing useful.

use std::borrow::Cow;
use std::time::Duration;

use async_trait::async_trait;
use globset::{Glob, GlobSet, GlobSetBuilder};
use serde_json::json;

use agent_sandbox::Permission;

use crate::call::{ToolCall, ToolOutput, ToolResult};
use crate::context::Context;
use crate::schema;
use crate::tool::{Intent, Tool, ToolDefinition, DEFAULT_TIMEOUT_SECONDS};

/// Enough hits to choose a file from; the model narrows rather than scrolls.
const MAX_MATCHES: usize = 60;
/// How many paths one listing may carry.
const MAX_PATHS: usize = 200;
/// Files past this size are reported as skipped instead of read line by line,
/// because a generated bundle is not where a definition lives.
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
/// A file with more top-level names than this is a generated one, and its
/// declarations are not the answer a model is looking for.
const DECLARATIONS_PER_FILE: usize = 80;

pub struct SearchText;

#[async_trait]
impl Tool for SearchText {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "search_text",
            "Find a literal string in the files under `path` and return `file:line:text` for \
             each hit, at most 60 of them. Case-insensitive unless `case_sensitive` is true; \
             `glob` narrows which files are opened (for example \"*.rs\"). Not a regular \
             expression: every character matches itself. Skips what .gitignore skips, binary \
             files and files over 2 MB.",
            schema::object(
                json!({
                    "pattern": schema::string("the text to find, exactly as written"),
                    "path": schema::path("file or directory to search; default the whole repository"),
                    "glob": schema::string("only search files whose relative path matches this pattern, e.g. \"*.rs\""),
                    "case_sensitive": schema::boolean("match case exactly; default false"),
                }),
                &["pattern"],
            ),
            Permission::ReadOnly,
        )
        .with_timeout(Duration::from_secs(DEFAULT_TIMEOUT_SECONDS * 3))
    }

    fn intent(&self, call: &ToolCall, ctx: &Context) -> Result<Intent, String> {
        required(call, "search_text")?;
        let raw = call.str_arg("path").unwrap_or_else(|| ".".to_owned());
        Ok(Intent::reading(vec![ctx.resolve(&raw)?]))
    }

    async fn run(&self, call: &ToolCall, ctx: &Context) -> ToolResult {
        let scope = match self.intent(call, ctx) {
            Ok(intent) => intent.paths[0].clone(),
            Err(problem) => return failure(call, problem),
        };
        let Some(pattern) = call.str_arg("pattern") else {
            return failure(call, "search_text needs a \"pattern\"".to_owned());
        };
        let case_sensitive = call.bool_arg("case_sensitive").unwrap_or(false);
        let needle = if case_sensitive {
            Cow::Borrowed(pattern.as_str())
        } else {
            Cow::Owned(pattern.to_lowercase())
        };
        let filter = match call.str_arg("glob") {
            Some(glob) => match compile_glob(&glob) {
                Some(set) => Some(set),
                None => {
                    return failure(
                        call,
                        format!("{glob} is not a usable file pattern, so nothing was searched"),
                    )
                }
            },
            None => None,
        };

        let mut hits: Vec<String> = Vec::new();
        let mut searched = 0usize;
        let mut skipped = 0usize;
        let walk = walker(&scope);
        for entry in walk.build().flatten() {
            if !entry.file_type().is_some_and(|kind| kind.is_file()) {
                continue;
            }
            let relative = ctx.relative(entry.path());
            if let Some(filter) = &filter {
                if !filter.is_match(PathLike(&relative)) {
                    continue;
                }
            }
            let oversized = entry
                .metadata()
                .map(|metadata| metadata.len() > MAX_FILE_BYTES)
                .unwrap_or(false);
            let text = if oversized {
                None
            } else {
                std::fs::read(entry.path())
                    .ok()
                    .and_then(|bytes| String::from_utf8(bytes).ok())
            };
            let Some(text) = text else {
                skipped += 1;
                continue;
            };
            searched += 1;
            for (number, line) in text.lines().enumerate() {
                let haystack = if case_sensitive {
                    Cow::Borrowed(line)
                } else {
                    Cow::Owned(line.to_lowercase())
                };
                if haystack.contains(needle.as_ref()) {
                    hits.push(format!("{relative}:{}:{}", number + 1, line.trim_end()));
                    if hits.len() >= MAX_MATCHES {
                        break;
                    }
                }
            }
            if hits.len() >= MAX_MATCHES {
                break;
            }
        }

        let mut text = hits.join("\n");
        if !text.is_empty() {
            text.push('\n');
        }
        let omitted = if skipped > 0 {
            format!(", {skipped} skipped as binary or too large")
        } else {
            String::new()
        };
        text.push_str(&format!(
            "[{} match(es) in {searched} file(s){omitted}{}]",
            hits.len(),
            if hits.len() >= MAX_MATCHES {
                "; narrow the search"
            } else {
                ""
            }
        ));
        ToolResult::completed(call.clone(), Permission::ReadOnly, ToolOutput::new(text), 0)
    }
}

/// Find files by name, which is how an agent locates the module it has only
/// been told the subject of.
pub struct SearchFiles;

#[async_trait]
impl Tool for SearchFiles {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "search_files",
            "List the files under `path` whose relative path matches the glob `pattern` \
             (for example \"**/*.rs\" or \"src/**/test*\"), at most 200 of them. A pattern \
             without a slash also matches on its own name, so \"sandbox.rs\" works. Says what \
             is there without reading any of it.",
            schema::object(
                json!({
                    "pattern": schema::string("glob the relative path must match"),
                    "path": schema::path("directory to search; default the repository root"),
                }),
                &["pattern"],
            ),
            Permission::ReadOnly,
        )
    }

    fn intent(&self, call: &ToolCall, ctx: &Context) -> Result<Intent, String> {
        required(call, "search_files")?;
        let raw = call.str_arg("path").unwrap_or_else(|| ".".to_owned());
        Ok(Intent::reading(vec![ctx.resolve(&raw)?]))
    }

    async fn run(&self, call: &ToolCall, ctx: &Context) -> ToolResult {
        let scope = match self.intent(call, ctx) {
            Ok(intent) => intent.paths[0].clone(),
            Err(problem) => return failure(call, problem),
        };
        let Some(pattern) = call.str_arg("pattern") else {
            return failure(call, "search_files needs a \"pattern\"".to_owned());
        };
        let Some(glob) = compile_glob(&pattern) else {
            return failure(
                call,
                format!("{pattern} is not a usable file pattern, so nothing was searched"),
            );
        };

        let mut matches: Vec<String> = Vec::new();
        let walk = walker(&scope);
        for entry in walk.build().flatten() {
            if !entry.file_type().is_some_and(|kind| kind.is_file()) {
                continue;
            }
            let relative = ctx.relative(entry.path());
            if glob.is_match(PathLike(&relative)) {
                matches.push(relative);
                if matches.len() >= MAX_PATHS {
                    break;
                }
            }
        }
        matches.sort();
        let mut text = matches.join("\n");
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&format!(
            "[{} file(s) match{}]",
            matches.len(),
            if matches.len() >= MAX_PATHS {
                ", and there are more; narrow the pattern"
            } else {
                ""
            }
        ));
        ToolResult::completed(call.clone(), Permission::ReadOnly, ToolOutput::new(text), 0)
    }
}

/// Where a name is defined, as opposed to every place it is mentioned.
pub struct SearchSymbols;

#[async_trait]
impl Tool for SearchSymbols {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "search_symbols",
            "Find where `symbol` is defined and return `file:line:` followed by the declaration \
             itself, at most 60 of them. It reads what each file declares — fn, struct, enum, \
             trait, class, def, a heading in Markdown, a table in TOML — so it answers \"where \
             does this live\" in one call instead of a search and ten reads. Only top-level \
             declarations are seen: a method inside an impl or a class is not, and \
             `search_text` is the tool for that. Skips what .gitignore skips.",
            schema::object(
                json!({
                    "symbol": schema::string("the name to find the definition of; matches part of a name, case-insensitive"),
                    "path": schema::path("file or directory to search; default the whole repository"),
                    "glob": schema::string("only look in files whose relative path matches this pattern, e.g. \"*.rs\""),
                }),
                &["symbol"],
            ),
            Permission::ReadOnly,
        )
        .with_timeout(Duration::from_secs(DEFAULT_TIMEOUT_SECONDS * 3))
    }

    fn intent(&self, call: &ToolCall, ctx: &Context) -> Result<Intent, String> {
        if call.str_arg("symbol").is_none() {
            return Err("search_symbols needs a \"symbol\"".to_owned());
        }
        let raw = call.str_arg("path").unwrap_or_else(|| ".".to_owned());
        Ok(Intent::reading(vec![ctx.resolve(&raw)?]))
    }

    async fn run(&self, call: &ToolCall, ctx: &Context) -> ToolResult {
        let scope = match self.intent(call, ctx) {
            Ok(intent) => intent.paths[0].clone(),
            Err(problem) => return failure(call, problem),
        };
        let Some(symbol) = call.str_arg("symbol") else {
            return failure(call, "search_symbols needs a \"symbol\"".to_owned());
        };
        let needle = symbol.to_lowercase();
        let filter = match call.str_arg("glob") {
            Some(glob) => match compile_glob(&glob) {
                Some(set) => Some(set),
                None => {
                    return failure(
                        call,
                        format!("{glob} is not a usable file pattern, so nothing was searched"),
                    )
                }
            },
            None => None,
        };

        let mut found: Vec<String> = Vec::new();
        let mut skipped = 0usize;
        let mut scanned = 0usize;
        for entry in walker(&scope).build().flatten() {
            if !entry.file_type().is_some_and(|kind| kind.is_file()) {
                continue;
            }
            let relative = ctx.relative(entry.path());
            if let Some(filter) = &filter {
                if !filter.is_match(PathLike(&relative)) {
                    continue;
                }
            }
            let oversized = entry
                .metadata()
                .map(|metadata| metadata.len() > MAX_FILE_BYTES)
                .unwrap_or(false);
            let text = if oversized {
                None
            } else {
                std::fs::read(entry.path())
                    .ok()
                    .and_then(|bytes| String::from_utf8(bytes).ok())
            };
            let Some(text) = text else {
                skipped += 1;
                continue;
            };
            scanned += 1;
            let language = agent_repo::Language::from_path(&relative);
            let declarations = agent_repo::declarations_at(&text, language, DECLARATIONS_PER_FILE);
            for declaration in declarations {
                if declaration.name.to_lowercase().contains(&needle) {
                    found.push(format!(
                        "{relative}:{}:{}",
                        declaration.line, declaration.text
                    ));
                    if found.len() >= MAX_MATCHES {
                        break;
                    }
                }
            }
            if found.len() >= MAX_MATCHES {
                break;
            }
        }

        let hits = found.len();
        let mut text = found.join("\n");
        if !text.is_empty() {
            text.push('\n');
        }
        let omission = if skipped > 0 {
            format!(", {skipped} skipped as binary or too large")
        } else {
            String::new()
        };
        if hits == 0 {
            // The honest answer includes why this can be empty: a name that is
            // used everywhere and declared at column zero nowhere looks like a
            // tool failure rather than a tool with a known edge.
            text.push_str(&format!(
                "[no definition of {symbol} in {scanned} file(s){omission}]\n[an indented \
                 method, a closure or a `use` rename is not a top-level declaration; \
                 search_text finds every mention of it]"
            ));
        } else {
            let next = if hits >= MAX_MATCHES {
                "; narrow the search"
            } else {
                "; read_file at one of these lines to use it"
            };
            text.push_str(&format!(
                "[{hits} definition(s) in {scanned} file(s){omission}{next}]"
            ));
        }
        ToolResult::completed(call.clone(), Permission::ReadOnly, ToolOutput::new(text), 0)
    }
}

/// Both search tools walk the tree the model named, so an absent pattern is the
/// only argument worth checking before the walk.
fn required(call: &ToolCall, tool: &str) -> Result<(), String> {
    if call.str_arg("pattern").is_some() {
        return Ok(());
    }
    Err(format!("{tool} needs a \"pattern\""))
}

/// A walk that honours .gitignore and still shows dotfiles, because `.cargo` and
/// `.github` are configuration a repository agent gets asked about.
///
/// `require_git(false)` is the difference between a tool that keeps its promise
/// and one that quietly starts reading `node_modules`: the `ignore` crate skips
/// .gitignore rules altogether outside a working tree, and a repository the
/// agent was pointed at may be an export, a submodule, or a directory that has
/// not been initialised yet.
fn walker(scope: &std::path::Path) -> ignore::WalkBuilder {
    let mut builder = ignore::WalkBuilder::new(scope);
    builder.hidden(false).require_git(false);
    builder
}

fn failure(call: &ToolCall, message: String) -> ToolResult {
    ToolResult::failed(
        call.clone(),
        Permission::ReadOnly,
        message,
        ToolOutput::empty(),
    )
}

/// A pattern that matches either as written or as a bare name anywhere below,
/// which is what a person means when they type `*.rs`.
fn compile_glob(pattern: &str) -> Option<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    let mut added = false;
    for candidate in glob_candidates(pattern) {
        if let Ok(glob) = Glob::new(&candidate) {
            builder.add(glob);
            added = true;
        }
    }
    if !added {
        return None;
    }
    builder.build().ok()
}

fn glob_candidates(pattern: &str) -> Vec<String> {
    if pattern.contains('/') {
        return vec![pattern.to_owned()];
    }
    vec![pattern.to_owned(), format!("**/{pattern}")]
}

/// `GlobSet` matches against anything that borrows as a `Path`; the relative
/// path a tool printed is the string an allowlist author wrote, so that is what
/// gets matched rather than a second conversion.
struct PathLike<'a>(&'a str);

impl AsRef<std::path::Path> for PathLike<'_> {
    fn as_ref(&self) -> &std::path::Path {
        std::path::Path::new(self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("a temp repo");
        let write = |name: &str, contents: &str| {
            let path = dir.path().join(name);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("writable");
            }
            std::fs::write(path, contents).expect("writable");
        };
        write(
            "src/auth.rs",
            "pub fn sign_token(secret: &str) -> String {\n    format!(\"{secret}\")\n}\n",
        );
        write(
            "src/lib.rs",
            "pub mod auth;\n// token handling lives in auth\n",
        );
        write(
            "docs/notes.md",
            "# Tokens\n\nNothing about token signing here.\n",
        );
        write("target/out.rs", "token token token\n");
        std::fs::write(dir.path().join(".gitignore"), "target/\n").expect("writable");
        dir
    }

    fn context(dir: &tempfile::TempDir) -> Context {
        Context::new(dir.path().to_path_buf())
    }

    #[tokio::test]
    async fn a_hit_names_the_file_the_line_and_the_text() {
        let dir = repo();
        let ctx = context(&dir);
        let call = ToolCall::new("search_text", json!({"pattern": "sign_token"}));
        let result = SearchText.run(&call, &ctx).await;
        assert!(result.is_success(), "{}", result.summary());
        assert!(
            result
                .output
                .text
                .contains("src/auth.rs:1:pub fn sign_token(secret: &str) -> String {"),
            "{}",
            result.output.text
        );
        assert!(
            result.output.text.contains("1 match"),
            "{}",
            result.output.text
        );
    }

    #[tokio::test]
    async fn a_search_ignores_case_until_told_not_to() {
        let dir = repo();
        let ctx = context(&dir);
        let loose = ToolCall::new("search_text", json!({"pattern": "TOKEN HANDLING"}));
        let result = SearchText.run(&loose, &ctx).await;
        assert!(
            result.output.text.contains("src/lib.rs:2"),
            "{}",
            result.output.text
        );

        let exact = ToolCall::new(
            "search_text",
            json!({"pattern": "TOKEN HANDLING", "case_sensitive": true}),
        );
        let result = SearchText.run(&exact, &ctx).await;
        assert!(
            result.output.text.contains("0 match"),
            "a case-sensitive search found something it should not have: {}",
            result.output.text
        );
    }

    #[tokio::test]
    async fn gitignored_files_are_never_opened() {
        let dir = repo();
        let ctx = context(&dir);
        let call = ToolCall::new("search_text", json!({"pattern": "token token token"}));
        let result = SearchText.run(&call, &ctx).await;
        assert!(
            result.output.text.contains("0 match"),
            "only target/ holds that string: {}",
            result.output.text
        );
    }

    #[tokio::test]
    async fn a_glob_narrows_which_files_are_opened() {
        let dir = repo();
        let ctx = context(&dir);
        let call = ToolCall::new("search_text", json!({"pattern": "token", "glob": "*.md"}));
        let result = SearchText.run(&call, &ctx).await;
        assert!(
            result.output.text.contains("docs/notes.md:3"),
            "{}",
            result.output.text
        );
        assert!(
            !result.output.text.contains("src/"),
            "{}",
            result.output.text
        );
    }

    #[tokio::test]
    async fn a_binary_file_is_skipped_and_says_so() {
        let dir = repo();
        std::fs::write(dir.path().join("blob.bin"), [0u8, 1, 2, 3, 255, 128]).expect("writable");
        let ctx = context(&dir);
        let call = ToolCall::new("search_text", json!({"pattern": "nothing like this"}));
        let result = SearchText.run(&call, &ctx).await;
        assert!(
            result.output.text.contains("1 skipped"),
            "{}",
            result.output.text
        );
    }

    #[tokio::test]
    async fn a_missing_pattern_is_a_message_not_a_panic() {
        let dir = repo();
        let ctx = context(&dir);
        let call = ToolCall::new("search_text", json!({"path": "src"}));
        let problem = SearchText.intent(&call, &ctx).expect_err("no pattern");
        assert!(problem.contains("pattern"), "{problem}");
    }

    #[tokio::test]
    async fn an_unusable_glob_is_refused_before_the_walk() {
        let dir = repo();
        let ctx = context(&dir);
        let call = ToolCall::new(
            "search_text",
            json!({"pattern": "token", "glob": "src/[unclosed"}),
        );
        let result = SearchText.run(&call, &ctx).await;
        assert!(!result.is_success(), "{}", result.summary());
        assert!(
            result.summary().contains("usable file pattern"),
            "{}",
            result.summary()
        );
    }

    #[tokio::test]
    async fn files_are_found_by_glob_and_by_bare_name() {
        let dir = repo();
        let ctx = context(&dir);
        let call = ToolCall::new("search_files", json!({"pattern": "**/*.rs"}));
        let result = SearchFiles.run(&call, &ctx).await;
        let text = result.output.text;
        assert!(text.contains("src/auth.rs"), "{text}");
        assert!(text.contains("src/lib.rs"), "{text}");
        assert!(!text.contains("target/"), "{text}");
        assert!(text.contains("2 file(s) match"), "{text}");

        let call = ToolCall::new("search_files", json!({"pattern": "notes.md"}));
        let result = SearchFiles.run(&call, &ctx).await;
        assert!(
            result.output.text.contains("docs/notes.md"),
            "{}",
            result.output.text
        );
    }

    #[tokio::test]
    async fn a_search_scope_can_be_narrowed_to_one_directory() {
        let dir = repo();
        let ctx = context(&dir);
        let call = ToolCall::new("search_files", json!({"pattern": "*.rs", "path": "src"}));
        let result = SearchFiles.run(&call, &ctx).await;
        let text = result.output.text;
        assert!(text.contains("src/auth.rs"), "{text}");
        assert!(!text.contains("docs/"), "{text}");
    }

    #[test]
    fn a_search_never_claims_more_than_read_only() {
        for tool in [
            Box::new(SearchText) as Box<dyn Tool>,
            Box::new(SearchFiles),
            Box::new(SearchSymbols),
        ] {
            let definition = tool.definition();
            assert_eq!(definition.permission, Permission::ReadOnly);
        }
        for tool in [Box::new(SearchText) as Box<dyn Tool>, Box::new(SearchFiles)] {
            assert!(
                tool.definition().description.contains("Not a regular")
                    || tool.definition().description.contains("glob"),
                "the description tells the model what kind of pattern this is"
            );
        }
    }

    /// The one thing a model cannot work out from an empty answer: this sees
    /// declarations at column zero and nothing else, so the description has to
    /// say that and name the tool that does see the rest.
    #[test]
    fn a_definition_search_says_what_it_cannot_see() {
        let description = SearchSymbols.definition().description;
        assert!(description.contains("top-level"), "{description}");
        assert!(description.contains("search_text"), "{description}");
    }

    #[tokio::test]
    async fn a_definition_is_found_with_the_line_it_is_on() {
        let dir = repo();
        let ctx = context(&dir);
        let call = ToolCall::new("search_symbols", json!({"symbol": "sign_token"}));
        let result = SearchSymbols.run(&call, &ctx).await;
        assert!(result.is_success(), "{}", result.summary());
        let text = result.output.text;
        assert!(
            text.contains("src/auth.rs:1:pub fn sign_token(secret: &str) -> String {"),
            "{text}"
        );
        assert!(text.contains("1 definition(s)"), "{text}");
    }

    /// The known edge, said out loud rather than left as an empty answer: this
    /// reads declarations at column zero, so a method inside an `impl` is
    /// invisible and a model that got nothing back has to be told which tool to
    /// ask instead.
    #[tokio::test]
    async fn an_indented_method_is_missed_and_the_answer_points_at_search_text() {
        let dir = repo();
        std::fs::write(
            dir.path().join("src/session.rs"),
            "pub struct Session;\n\nimpl Session {\n    fn reopen(&self) {}\n}\n",
        )
        .expect("writable");
        let ctx = context(&dir);
        let result = SearchSymbols
            .run(
                &ToolCall::new("search_symbols", json!({"symbol": "reopen"})),
                &ctx,
            )
            .await;
        let text = result.output.text;
        assert!(text.contains("no definition of reopen"), "{text}");
        assert!(text.contains("search_text"), "{text}");

        // The struct above it is top-level and is found.
        let result = SearchSymbols
            .run(
                &ToolCall::new("search_symbols", json!({"symbol": "Session"})),
                &ctx,
            )
            .await;
        assert!(
            result.output.text.contains("src/session.rs:1"),
            "{}",
            result.output.text
        );
    }

    #[tokio::test]
    async fn a_part_of_a_name_matches_across_languages_and_ignore_rules() {
        let dir = repo();
        let ctx = context(&dir);
        // Case-insensitive substring, so "token" finds the Rust function and the
        // Markdown heading, and never the ignored `target/` copy of both.
        let result = SearchSymbols
            .run(
                &ToolCall::new("search_symbols", json!({"symbol": "TOKEN"})),
                &ctx,
            )
            .await;
        let text = result.output.text;
        assert!(text.contains("src/auth.rs:1"), "{text}");
        assert!(text.contains("docs/notes.md:1:# Tokens"), "{text}");
        assert!(!text.contains("target/out.rs"), "{text}");
    }

    #[tokio::test]
    async fn a_glob_narrows_which_files_are_read_for_definitions() {
        let dir = repo();
        let ctx = context(&dir);
        let result = SearchSymbols
            .run(
                &ToolCall::new("search_symbols", json!({"symbol": "token", "glob": "*.md"})),
                &ctx,
            )
            .await;
        let text = result.output.text;
        assert!(text.contains("docs/notes.md"), "{text}");
        assert!(!text.contains("src/auth.rs"), "{text}");
    }

    #[tokio::test]
    async fn a_bad_glob_is_refused_before_the_walk() {
        let dir = repo();
        let ctx = context(&dir);
        let result = SearchSymbols
            .run(
                &ToolCall::new("search_symbols", json!({"symbol": "x", "glob": "[a-"})),
                &ctx,
            )
            .await;
        assert!(!result.is_success(), "{}", result.summary());
        assert!(
            result.summary().contains("usable file pattern"),
            "{}",
            result.summary()
        );
    }

    #[test]
    fn a_definition_search_needs_a_name_and_stays_inside_the_repository() {
        let ctx = Context::new(PathBuf::from("/repo"));
        let problem = SearchSymbols
            .intent(&ToolCall::new("search_symbols", json!({})), &ctx)
            .expect_err("no symbol");
        assert!(problem.contains("\"symbol\""), "{problem}");
        let problem = SearchSymbols
            .intent(
                &ToolCall::new("search_symbols", json!({"symbol": "x", "path": "../out"})),
                &ctx,
            )
            .expect_err("climbs out");
        assert!(problem.contains("climbs out"), "{problem}");
    }

    #[test]
    fn a_path_outside_the_repository_is_refused_by_the_intent() {
        let ctx = Context::new(PathBuf::from("/repo"));
        let call = ToolCall::new("search_text", json!({"pattern": "x", "path": "../out"}));
        let problem = SearchText.intent(&call, &ctx).expect_err("climbs out");
        assert!(problem.contains("climbs out"), "{problem}");
    }

    #[test]
    fn a_bare_name_pattern_matches_at_any_depth() {
        assert_eq!(glob_candidates("*.rs"), vec!["*.rs", "**/*.rs"]);
        assert_eq!(
            glob_candidates("src/**/test*"),
            vec!["src/**/test*".to_owned()]
        );
    }
}
