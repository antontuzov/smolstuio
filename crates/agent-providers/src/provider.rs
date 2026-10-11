//! The one interface between the agent loop and anything that produces text.
//!
//! A provider is asked for a completion and either answers, or explains why it
//! cannot in a way the loop can act on ([`crate::ProviderError::retryable`]).
//! It is not a chat client, not a tokenizer and not a model manager: those
//! belong to the loop and to this repo's model layer. This exists so the same
//! run can be driven by a scripted mock in a test, by `ollama` on a laptop, or
//! by a GGUF file through the engine layer, without the loop knowing.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use smollm_core::chat::{approx_token_count, ChatMessage, TokenUsage};

use agent_tools::ToolCall;

use crate::error::ProviderResult;

/// What a provider can do, asked rather than assumed.
///
/// The loop reads this before it decides how to prompt. A model without
/// `tool_calling` is not an error: it is a run that gets told to answer in
/// prose with a fenced JSON block, and whose answer then has to be parsed
/// back. Discovering that at runtime is what keeps a 1.5B model usable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    pub streaming: bool,
    pub tool_calling: bool,
    /// The window the model was configured with, not the one it can be stretched
    /// to. Overstating it is how a run dies at step nine.
    pub context_tokens: usize,
}

impl Capabilities {
    /// The floor every provider meets: no streaming, no tool calls, a small
    /// window. A provider that can do more says so.
    pub fn basic() -> Self {
        Self {
            streaming: false,
            tool_calling: false,
            context_tokens: 4096,
        }
    }

    pub fn with_tool_calling(mut self) -> Self {
        self.tool_calling = true;
        self
    }

    pub fn with_streaming(mut self) -> Self {
        self.streaming = true;
        self
    }

    pub fn window(mut self, tokens: usize) -> Self {
        self.context_tokens = tokens;
        self
    }
}

/// A tool the provider may call, as the model sees it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    /// A JSON Schema object for the arguments.
    pub parameters: serde_json::Value,
}

impl ToolSchema {
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: serde_json::Value,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            parameters,
        }
    }

    /// The token cost of naming this tool, which is paid on every step of the
    /// run. It is why an agent for small models exposes fewer tools than it
    /// could.
    pub fn token_cost(&self) -> usize {
        approx_token_count(&format!(
            "{} {} {}",
            self.name,
            self.description,
            serde_json::to_string(&self.parameters).unwrap_or_default()
        )) as usize
    }
}

/// One request for one completion.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompletionRequest {
    pub messages: Vec<ChatMessage>,
    /// Omitted when the model cannot be trusted with a tool list.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolSchema>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stop: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<i64>,
}

impl CompletionRequest {
    pub fn new(messages: Vec<ChatMessage>) -> Self {
        Self {
            messages,
            tools: Vec::new(),
            max_tokens: None,
            temperature: None,
            top_p: None,
            stop: Vec::new(),
            seed: None,
        }
    }

    /// Rough cost of the prompt, using the same heuristic everywhere in this
    /// repository so the number in the UI and the number in the loop agree.
    pub fn prompt_tokens(&self) -> u32 {
        self.messages
            .iter()
            .map(|message| approx_token_count(&message.content))
            .sum()
    }

    /// Whether the prompt plus the reply budget fits the window, checked before
    /// a request is made rather than after the engine has said no.
    pub fn fits(&self, capabilities: &Capabilities, reply_budget: usize) -> bool {
        // A token count is small enough to hold in either width.
        self.prompt_tokens() as usize + reply_budget <= capabilities.context_tokens
    }
}

/// Why a generation stopped where it did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// The model finished its turn.
    EndOfTurn,
    /// It asked for a tool instead of answering.
    ToolCalls,
    /// It ran out of the reply budget mid-sentence.
    Length,
    /// A stop sequence was hit.
    StopSequence,
}

impl StopReason {
    pub fn is_truncated(self) -> bool {
        self == Self::Length
    }
}

/// A finished answer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Completion {
    /// The text the model produced, including any JSON block it meant as a call.
    pub text: String,
    /// Calls the provider recognised natively. Empty for most small models.
    #[serde(default)]
    pub calls: Vec<ToolCall>,
    pub stop: StopReason,
    pub usage: TokenUsage,
}

impl Completion {
    pub fn text(text: impl Into<String>) -> Self {
        let text = text.into();
        let completion_tokens = approx_token_count(&text);
        Self {
            calls: Vec::new(),
            stop: StopReason::EndOfTurn,
            usage: TokenUsage::new(0, completion_tokens),
            text,
        }
    }

    pub fn with_calls(mut self, calls: Vec<ToolCall>) -> Self {
        self.stop = if calls.is_empty() {
            self.stop
        } else {
            StopReason::ToolCalls
        };
        self.calls = calls;
        self
    }

    pub fn asks_for_tools(&self) -> bool {
        !self.calls.is_empty() || self.stop == StopReason::ToolCalls
    }
}

/// A turn with nothing in it carries no information, and the engines refuse an
/// empty assistant message rather than prompting with it — so a model that once
/// answered with an empty string does not cost the rest of the run. Every real
/// provider filters through here, because the transcript the loop builds is the
/// same whichever side of the engine or the wire it ends up on.
///
/// The `cfg` is that same fact in build terms: mock and echo render or replay a
/// prompt rather than serialise one, so only the two adapters need it.
#[cfg(any(feature = "gguf", feature = "openai-compatible"))]
pub(crate) fn usable_messages(messages: &[ChatMessage]) -> Vec<ChatMessage> {
    messages
        .iter()
        .filter(|message| !is_blank_turn(message))
        .cloned()
        .collect()
}

#[cfg(any(feature = "gguf", feature = "openai-compatible"))]
fn is_blank_turn(message: &ChatMessage) -> bool {
    message.role == smollm_core::chat::Role::Assistant && message.content.is_empty()
}

#[async_trait]
pub trait Provider: Send + Sync {
    /// The name the loop prints and the config uses to find this provider.
    fn name(&self) -> &str;

    fn capabilities(&self) -> Capabilities;

    /// The model's own identifier, for a status line and for the transcript.
    fn model(&self) -> Option<&str> {
        None
    }

    async fn complete(&self, request: &CompletionRequest) -> ProviderResult<Completion>;

    /// Ask for the answer one more time, because the first was unusable. The
    /// default re-asks the same question, which is enough for a flaky server;
    /// a provider that can change the prompt overrides it.
    async fn complete_again(&self, request: &CompletionRequest) -> ProviderResult<Completion> {
        self.complete(request).await
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::{MockProvider, Reply};
    use serde_json::json;
    use smollm_core::chat::{chat_message, Role};

    fn request(text: &str) -> CompletionRequest {
        CompletionRequest::new(vec![chat_message(Role::User, text)])
    }

    #[test]
    fn the_basic_capability_set_is_the_honest_floor() {
        let capabilities = Capabilities::basic();
        assert!(!capabilities.streaming);
        assert!(!capabilities.tool_calling);
        assert_eq!(capabilities.context_tokens, 4096);
        // A provider adds what it has rather than claiming what it does not.
        assert_eq!(
            Capabilities::basic().with_tool_calling().window(8192),
            Capabilities {
                streaming: false,
                tool_calling: true,
                context_tokens: 8192,
            }
        );
    }

    #[test]
    fn a_tool_costs_the_tokens_it_would_really_take() {
        let small = ToolSchema::new(
            "pwd",
            "Print the working directory.",
            json!({"type": "object"}),
        );
        let large = ToolSchema::new(
            "search_repo_content",
            "Search every tracked file for a pattern, with context lines and a result cap.",
            json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "description": "The text to look for"},
                    "max_results": {"type": "integer"}
                },
                "required": ["pattern"]
            }),
        );
        assert!(small.token_cost() < large.token_cost());
        assert!(small.token_cost() > 0);
    }

    #[test]
    fn the_prompt_is_counted_over_every_message_in_it() {
        let both = CompletionRequest::new(vec![
            chat_message(Role::System, "You edit code and never push it."),
            chat_message(Role::User, "Find where the config is read."),
        ]);
        let alone = both.prompt_tokens();
        assert!(alone > 0);
        let mut longer = both;
        longer
            .messages
            .push(chat_message(Role::User, "and then summarise it"));
        assert!(longer.prompt_tokens() > alone);
    }

    #[test]
    fn a_prompt_that_leaves_no_room_for_an_answer_does_not_fit() {
        let capabilities = Capabilities::basic().window(100);
        let request = request("a short prompt");
        assert!(request.fits(&capabilities, 10));
        // The reply budget is part of the window, so the same prompt with a
        // 100-token answer has nowhere to go.
        assert!(!request.fits(&capabilities, 100));
    }

    #[test]
    fn a_long_prompt_is_refused_before_the_engine_hears_about_it() {
        let capabilities = Capabilities::basic().window(512);
        let request = CompletionRequest::new(vec![chat_message(Role::User, "word ".repeat(2000))]);
        assert!(
            !request.fits(&capabilities, 64),
            "10 KB of prose is not a 512-token prompt"
        );
    }

    #[test]
    fn an_empty_request_costs_nothing_and_fits_anywhere() {
        let request = CompletionRequest::new(Vec::new());
        assert_eq!(request.prompt_tokens(), 0);
        assert!(request.fits(&Capabilities::basic(), 1));
    }

    #[test]
    fn a_text_answer_does_not_ask_for_tools() {
        let answer = Completion::text("The bug is in the parser.");
        assert_eq!(answer.stop, StopReason::EndOfTurn);
        assert!(!answer.asks_for_tools());
        assert_eq!(
            answer.usage.total_tokens, answer.usage.completion_tokens,
            "an answer with no prompt still counts what it cost"
        );
    }

    #[test]
    fn an_answer_with_calls_reports_that_it_asked_for_them() {
        let answer = Completion::text("Reading now.").with_calls(vec![ToolCall::new(
            "read_file",
            json!({"path": "src/main.rs"}),
        )]);
        assert_eq!(answer.stop, StopReason::ToolCalls);
        assert!(answer.asks_for_tools());
    }

    #[test]
    fn an_empty_call_list_leaves_the_stop_reason_alone() {
        let answer = Completion::text("done").with_calls(Vec::new());
        assert_eq!(answer.stop, StopReason::EndOfTurn);
        assert!(!answer.asks_for_tools());
    }

    #[test]
    fn only_running_out_of_budget_counts_as_truncated() {
        assert!(StopReason::Length.is_truncated());
        assert!(!StopReason::EndOfTurn.is_truncated());
        assert!(!StopReason::ToolCalls.is_truncated());
        assert!(!StopReason::StopSequence.is_truncated());
    }

    #[tokio::test]
    async fn asking_again_defaults_to_asking_the_same_question() {
        let mock = MockProvider::scripted([Reply::text("the first answer"), Reply::text("second")]);
        // `complete_again` is not overridden here, so it takes the next scripted
        // reply through the same door.
        assert_eq!(
            mock.complete_again(&request("answer once"))
                .await
                .unwrap()
                .text,
            "the first answer"
        );
        assert_eq!(mock.requests().len(), 1, "a repair is one request, not two");
    }

    #[test]
    #[cfg(any(feature = "gguf", feature = "openai-compatible"))]
    fn blank_turns_go_and_the_rest_keep_their_order() {
        let messages = vec![
            chat_message(Role::System, "rules"),
            chat_message(Role::Assistant, ""),
            chat_message(Role::User, "do the thing"),
            chat_message(Role::Assistant, "listed src"),
        ];
        let kept = usable_messages(&messages);
        assert_eq!(kept.len(), 3, "only the blank turn leaves");
        assert_eq!(kept[0].role, Role::System);
        assert_eq!(kept[1].content, "do the thing", "order is not reordered");
        assert_eq!(kept[2].content, "listed src");
        // A blank user or system turn stays: the model has to be told the rules
        // even if this build's loop never sends an empty one.
        let mut with_blank_user = messages.clone();
        with_blank_user.push(chat_message(Role::User, ""));
        assert_eq!(usable_messages(&with_blank_user).len(), 4);
    }

    #[test]
    fn a_request_survives_being_written_and_read_back() {
        let mut request = CompletionRequest::new(vec![chat_message(Role::System, "rules")]);
        request.tools = vec![ToolSchema::new(
            "read_file",
            "Read a file",
            json!({"type": "object"}),
        )];
        request.max_tokens = Some(128);
        request.temperature = Some(0.2);
        request.seed = Some(7);
        let text = serde_json::to_string(&request).expect("a request is serialisable");
        let back: CompletionRequest = serde_json::from_str(&text).expect("and readable");
        assert_eq!(back, request);
        assert!(
            !text.contains("top_p"),
            "an unset option is not written: {text}"
        );
    }

    #[test]
    fn stop_reasons_are_written_the_way_a_config_file_spells_them() {
        assert_eq!(
            serde_json::to_string(&StopReason::EndOfTurn).expect("a word"),
            "\"end_of_turn\""
        );
        assert_eq!(
            serde_json::from_str::<StopReason>("\"tool_calls\"").expect("a word"),
            StopReason::ToolCalls
        );
    }

    #[test]
    fn an_answer_keeps_its_text_alongside_its_calls() {
        let answer = Completion::text("listing src")
            .with_calls(vec![ToolCall::new("list_files", json!({"dir": "src"}))]);
        let text = serde_json::to_string(&answer).expect("an answer is serialisable");
        let back: Completion = serde_json::from_str(&text).expect("and readable");
        assert_eq!(back, answer);
        assert_eq!(back.calls[0].str_arg("dir"), Some("src".to_owned()));
    }
}
