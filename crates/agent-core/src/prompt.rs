//! The prompt, written for a model that has about a thousand tokens of patience.
//!
//! Everything the loop knows has to reach the model through these strings, so
//! their order is the design: what the model must not forget is at the top and is
//! short, the repository's table of contents comes before any file body, and the
//! tool list is only spelled out in prose when the provider cannot carry it
//! natively. A tool call is asked for as one JSON object, because that is the
//! shape this repository can read back out of a messy answer.
//!
//! The rendering functions here are pure and take the types from the caller, so
//! what the model saw on step four can be asserted in a test and printed by
//! `smoll chat --trace` without running anything.

use smollm_core::chat::{chat_message, Role};

use agent_providers::{CompletionRequest, ToolSchema};
use agent_tools::{ToolResult, ToolStatus};

use crate::types::{ContextBundle, Plan};

/// The rules of the run, in the order the model is least likely to lose them.
const RULES: &str = "You are a coding agent working inside one local repository, with the tools \
listed below. Rules:\n\
- Work one step at a time. Ask for a tool, wait for its answer, then decide again.\n\
- Read before you write. Never change a file you have not read in this run.\n\
- Use paths relative to the repository root. A path that leaves it is refused.\n\
- Prefer patch_file for a small change to a file that already exists; write_file replaces a \
whole file and refuses to empty one.\n\
- Never run a command that pushes, merges, deletes outside the repository, or rewrites history.\n\
- When the work is finished, answer in plain prose with no tool call. Say what you changed and \
what you did not manage to do.";

/// How a call is requested when the model has no native function calling.
const TEXT_CALL_FORMAT: &str = "To ask for a tool, reply with exactly one JSON object in a fenced \
block and nothing else after it:\n```json\n{\"tool\": \"read_file\", \"arguments\": {\"path\": \
\"src/lib.rs\"}}\n```\nThe `tool` value must be one of the names above. `arguments` may be \
omitted when the tool needs none.";

/// The tools as a list the model can choose from.
///
/// `described` is whether the model gets the prose at all. A provider that
/// carries a tool array sends each description to the model itself, so spelling
/// it out here too buys the same words twice against a window that cannot afford
/// it: every step of the run pays, and the repository is what goes unpaid. A
/// provider that cannot take the array gets the full sentences, because this is
/// the only place it learns what a tool does.
fn tool_list(tools: &[ToolSchema], described: bool) -> String {
    if tools.is_empty() {
        return "You have no tools on this run. Answer from the context below and say what you \
                would need in order to do more."
            .to_owned();
    }
    let mut lines = vec!["Tools:".to_owned()];
    for tool in tools {
        let required = tool.parameters.get("required");
        let needs = match required.and_then(|value| value.as_array()) {
            Some(items) if !items.is_empty() => items
                .iter()
                .filter_map(|item| item.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            _ => "no arguments".to_owned(),
        };
        if described {
            lines.push(format!(
                "- {}: {} Required: {}.",
                tool.name, tool.description, needs
            ));
        } else {
            // The names still belong here: an array the chat template drops
            // silently leaves a model asking for a tool it was never told.
            lines.push(format!("- {name} (Required: {needs})", name = tool.name));
        }
    }
    lines.join("\n")
}

/// The system message: rules, what the repository is, and how to ask for a tool.
///
/// `about` is `Project::describe` and `Index::describe` joined — the ecosystem,
/// the crates and the file count — which is what lets a small model pick
/// `cargo test -p something` out of the air rather than inventing a command.
pub fn system(tools: &[ToolSchema], tool_calling: bool, about: &str) -> String {
    let mut prompt = String::from(RULES);
    if !tool_calling {
        prompt.push_str("\n\n");
        prompt.push_str(TEXT_CALL_FORMAT);
    }
    if !about.trim().is_empty() {
        prompt.push_str("\n\nThe repository: ");
        prompt.push_str(about.trim());
    }
    prompt.push_str("\n\n");
    prompt.push_str(&tool_list(tools, !tool_calling));
    prompt
}

/// The first user turn: the task, the repository map, the chosen files, the plan.
///
/// The map precedes the bodies on purpose. A model that has seen the table of
/// contents asks for the right file; one that has only seen three files assumes
/// the repository is three files.
pub fn open(bundle: &ContextBundle, plan: &Plan) -> String {
    let mut turn = format!("Task: {}\n", bundle.task);
    if !bundle.repo_map.trim().is_empty() {
        turn.push_str("\nRepository map:\n");
        turn.push_str(bundle.repo_map.trim());
        turn.push('\n');
    }
    if !bundle.files.is_empty() {
        turn.push_str("\nFiles most likely to matter:\n");
        for file in &bundle.files {
            turn.push_str(&format!(
                "\n### {} ({})\n```\n{}\n```\n",
                file.path,
                file.reason,
                file.content.trim_end()
            ));
        }
    }
    if !bundle.notes.is_empty() {
        turn.push_str("\nRemembered from earlier runs:\n");
        for note in &bundle.notes {
            turn.push_str(&format!("- {note}\n"));
        }
    }
    turn.push_str("\nPlan for this run:\n");
    turn.push_str(plan.render().trim());
    turn.push_str("\n\nWhat does your first step need?");
    turn
}

/// What a model that cannot hold a tool list wrote, turned back into an
/// observation it can read: one line per call, in the order it asked.
///
/// A failure is written as a failure. The temptation in a harness like this is to
/// soften it, and a small model that is told "read_file completed" when the file
/// was not there spends its remaining steps editing a file it never saw.
pub fn observation(results: &[ToolResult]) -> String {
    let mut blocks = Vec::new();
    for result in results {
        let heading = match &result.status {
            ToolStatus::Completed => format!("{} answered:", result.call.name),
            ToolStatus::Failed { message } => {
                format!("{} did not work ({message}). Answer:", result.call.name)
            }
            ToolStatus::Refused { reason } => {
                format!("{} was refused: {reason}. Answer:", result.call.name)
            }
            ToolStatus::AwaitingApproval => format!(
                "{} is waiting for a person to approve it. Answer:",
                result.call.name
            ),
        };
        let text = result.output.text.trim_end();
        blocks.push(if text.is_empty() {
            heading
        } else {
            format!("{heading}\n{text}")
        });
    }
    blocks.join("\n\n")
}

/// The same request, asked again in a way that can succeed.
///
/// The rejected text is quoted back rather than dropped: the model is the only
/// thing in the room that knows what it meant, and a repair prompt that says only
/// "that was invalid" gets a repeat of the same sentence. Nothing here adds a
/// tool call the model did not write — if the second answer is still unreadable,
/// the run says so instead of guessing.
pub fn repair(base: &CompletionRequest, raw: &str, problem: &str) -> CompletionRequest {
    let mut messages = base.messages.clone();
    messages.push(chat_message(Role::Assistant, raw.trim()));
    messages.push(chat_message(
        Role::User,
        format!(
            "That answer could not be used: {problem}\n\nReply with only the tool call, as one \
             JSON object in a fenced block. Keep it short: the tool name and its arguments, no \
             explanation, no second object."
        ),
    ));
    let mut request = CompletionRequest::new(messages);
    request.tools = base.tools.clone();
    request.max_tokens = base.max_tokens;
    request.temperature = Some(0.0);
    request.top_p = base.top_p;
    request.stop = base.stop.clone();
    request.seed = base.seed;
    request
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_sandbox::Permission;
    use agent_tools::{ToolCall, ToolOutput};
    use serde_json::json;

    fn tool(name: &str, required: &[&str]) -> ToolSchema {
        ToolSchema::new(
            name,
            format!("the {name} tool"),
            json!({"type": "object", "required": required}),
        )
    }

    fn bundle() -> ContextBundle {
        let mut bundle = ContextBundle::new("rename the session helper");
        bundle.repo_map = "src/session.rs\nsrc/lib.rs".to_owned();
        bundle.push_file(crate::types::ContextFile {
            path: "src/session.rs".to_owned(),
            content: "pub fn load() {}".to_owned(),
            reason: "the task names it".to_owned(),
        });
        bundle.notes = vec!["the tests need a fixture".to_owned()];
        bundle
    }

    /// A model with native calls already receives each description in the tool
    /// array, so the prose list stops at the names: what is left of the window
    /// belongs to the repository.
    #[test]
    fn a_tool_that_carries_its_own_description_is_not_described_twice() {
        let prompt = system(
            &[tool("read_file", &["path"]), tool("list_dir", &[])],
            true,
            "a Rust workspace of 40 files",
        );
        assert!(prompt.contains("- read_file (Required: path)"), "{prompt}");
        assert!(
            prompt.contains("Required: no arguments"),
            "a tool that needs nothing says so: {prompt}"
        );
        assert!(
            !prompt.contains("the read_file tool"),
            "the sentence the array already carries is not paid for again:\n{prompt}"
        );
        assert!(prompt.contains("a Rust workspace of 40 files"), "{prompt}");
        assert!(
            !prompt.contains("reply with exactly one JSON object"),
            "a model with native calls is not shown the prose format"
        );
    }

    #[test]
    fn a_model_without_function_calling_is_told_the_shape_and_the_names() {
        let prompt = system(&[tool("search_text", &["pattern"])], false, "");
        assert!(prompt.contains("```json"), "{prompt}");
        assert!(prompt.contains("search_text"), "{prompt}");
        assert!(
            prompt.contains("search_text: the search_text tool Required: pattern."),
            "the prose list is this model's only tool documentation: {prompt}"
        );
        assert!(
            !prompt.contains("The repository:"),
            "an empty description is not printed as an empty heading"
        );
    }

    #[test]
    fn no_tools_says_so_rather_than_offering_an_empty_list() {
        let prompt = system(&[], true, "");
        assert!(prompt.contains("no tools"), "{prompt}");
    }

    #[test]
    fn the_first_turn_puts_the_map_before_the_bodies_and_ends_with_a_question() {
        let plan = Plan::new(
            "rename the session helper",
            vec!["read src/session.rs".to_owned()],
        );
        let turn = open(&bundle(), &plan);
        let map = turn.find("Repository map:").expect("the map");
        let body = turn.find("### src/session.rs").expect("the file");
        assert!(map < body, "{turn}");
        assert!(turn.contains("the task names it"), "{turn}");
        assert!(turn.contains("the tests need a fixture"), "{turn}");
        assert!(turn.contains("[ ] 1. read src/session.rs"), "{turn}");
        assert!(
            turn.trim_end().ends_with("What does your first step need?"),
            "{turn}"
        );
    }

    #[test]
    fn an_observation_keeps_a_failure_a_failure() {
        let read = ToolResult::completed(
            ToolCall::new("read_file", json!({"path": "src/lib.rs"})),
            Permission::ReadOnly,
            ToolOutput::new("fn lib() {}"),
            3,
        );
        let missing = ToolResult::failed(
            ToolCall::new("read_file", json!({"path": "src/nope.rs"})),
            Permission::ReadOnly,
            "no such file",
            ToolOutput::empty(),
        );
        let refused = ToolResult::refused(
            ToolCall::new("run_command", json!({"argv": ["rm", "-rf", "/"]})),
            Permission::Destructive,
            "outside the workspace",
        );
        let text = observation(&[read, missing, refused]);
        assert!(text.contains("read_file answered:\nfn lib() {}"), "{text}");
        assert!(text.contains("did not work (no such file)"), "{text}");
        assert!(
            text.contains("was refused: outside the workspace"),
            "{text}"
        );
    }

    #[test]
    fn an_empty_answer_is_still_attributed_to_the_tool_that_gave_it() {
        let quiet = ToolResult::completed(
            ToolCall::new("git_status", json!({})),
            Permission::ReadOnly,
            ToolOutput::empty(),
            1,
        );
        assert_eq!(observation(&[quiet]), "git_status answered:");
    }

    #[test]
    fn a_repair_quotes_the_rejected_text_and_cools_the_sampling() {
        let mut base = CompletionRequest::new(vec![chat_message(Role::User, "do the thing")]);
        base.tools = vec![tool("read_file", &["path"])];
        base.max_tokens = Some(200);
        base.temperature = Some(0.7);
        let repaired = repair(
            &base,
            "```json\n{\"tool\": \"read_file\", \"path\": \"sr",
            "the fenced block does not close",
        );
        assert_eq!(repaired.messages.len(), 3);
        assert_eq!(repaired.messages[1].role, Role::Assistant);
        assert!(
            repaired.messages[1].content.contains("\"sr"),
            "the model sees its own words"
        );
        assert!(
            repaired.messages[2]
                .content
                .contains("the fenced block does not close"),
            "{}",
            repaired.messages[2].content
        );
        assert_eq!(repaired.max_tokens, Some(200));
        assert_eq!(
            repaired.temperature,
            Some(0.0),
            "a repair is not a second guess"
        );
        assert_eq!(repaired.tools, base.tools, "the same tools, asked again");
    }
}
