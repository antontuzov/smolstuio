//! One trait between the agent loop and whatever is answering it.
//!
//! The loop asks for a completion with tool schemas attached and gets back text
//! plus whatever calls it recognises — one answer per step, so a step can be
//! retried, timed out and logged as a unit.
//!
//! Behind the trait today sit the scripted mock, the echo provider, the GGUF
//! adapter onto this repository's engine layer and the OpenAI-compatible HTTP
//! client for Ollama, LM Studio, vLLM, TGI, `llama-server` and this repo's own
//! `smoll serve`. Every test in this workspace runs against the mock and the echo;
//! the two real adapters sit behind the `gguf` and `openai-compatible` features,
//! and `llama-cpp` on top of `gguf` to run weights rather than refuse. Asking for
//! a provider this build cannot make returns [`ProviderError::not_compiled`]
//! naming the flag, and a section that points at nothing returns an error naming
//! the file or the URL — never a silent fallback to the mock, because an agent
//! that quietly answers from a script in place of your model is worse than one
//! that stops.
//!
//! Small models are the design centre, not an afterthought: tool calls are
//! parsed from native function calls, fenced JSON and plain text in that order,
//! a malformed answer gets one repair attempt, and a provider that cannot do
//! tool calling is not an error — it degrades the run into suggestion mode.
//!
//! ```
//! use agent_providers::{tool_calls, CompletionRequest, MockProvider, Provider, Reply, ToolSchema};
//! use serde_json::json;
//! use smollm_core::chat::{chat_message, Role};
//!
//! let provider = MockProvider::scripted([Reply::call("list_files", json!({"dir": "src"}))]);
//! let mut request = CompletionRequest::new(vec![chat_message(Role::User, "what is in src?")]);
//! request.tools = vec![ToolSchema::new(
//!     "list_files",
//!     "List a directory",
//!     json!({"type": "object", "properties": {"dir": {"type": "string"}}}),
//! )];
//!
//! let runtime = tokio::runtime::Runtime::new().expect("a test runtime");
//! let answer = runtime.block_on(provider.complete(&request)).expect("a scripted answer");
//! assert_eq!(answer.calls.len(), 1);
//! assert_eq!(answer.calls[0].name, "list_files");
//!
//! // The same reading works on raw text, which is how a prose answer from a
//! // model without tool calling still becomes a call.
//! assert_eq!(tool_calls(r#"{ "tool": "list_files", "dir": "src" }"#, &|_| true).len(), 1);
//! ```

mod build;
mod echo;
mod error;
#[cfg(feature = "gguf")]
mod gguf;
mod mock;
#[cfg(feature = "openai-compatible")]
mod openai;
mod parse;
mod provider;

pub use build::{build, capabilities_of};
pub use echo::EchoProvider;
pub use error::{ProviderError, ProviderResult};
#[cfg(feature = "gguf")]
pub use gguf::GgufProvider;
pub use mock::{MockProvider, Reply, UNSCRIPTED};
#[cfg(feature = "openai-compatible")]
pub use openai::OpenAiProvider;
pub use parse::{repair, tool_calls, tool_calls_among};
pub use provider::{Capabilities, Completion, CompletionRequest, Provider, StopReason, ToolSchema};
