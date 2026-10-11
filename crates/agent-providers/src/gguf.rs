//! A local `.gguf` file, run through this repository's own engine layer.
//!
//! Nothing about inference is re-implemented here. [`smollm_engine::Engine`]
//! already loads weights, applies the model's own chat template, decodes on a
//! dedicated worker thread and honours a cancel token, so this file is only the
//! translation between what the agent loop asks for and what that engine takes —
//! plus the one question the engine cannot answer for itself, which is whether
//! the prompt still fits once the transcript has grown.
//!
//! Two rules hold it together:
//!
//! - **Never a stand-in.** Without a linked engine [`GgufProvider::new`] reports
//!   [`ProviderError::NotCompiled`] naming the cargo feature. Handing back
//!   MockEngine, or the metadata reader that shares the crate, would be worse
//!   than failing: the run would read like your model and say nothing about your
//!   repository.
//! - **Numbers the model actually paid.** A finished generation carries llama.cpp's
//!   own token counts, from its own tokeniser, rather than the character heuristic
//!   every estimate elsewhere in this repo falls back on.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use async_trait::async_trait;
use futures::StreamExt;
use smollm_core::chat::{
    approx_token_count, normalize_stop_sequences, CancelToken, ChatMessage, ChatRequest,
    LoadModelOptions, LoadModelRequest, SamplingParams, TokenUsage,
};
use smollm_core::gguf::GgufHeader;
use smollm_core::model::ModelMetadata;
use smollm_core::system::Backend;
use smollm_core::AppError;
use smollm_engine::{Engine, EngineKind, EngineManager, TokenStream};

use agent_config::ProviderConfig;

use crate::error::{ProviderError, ProviderResult};
use crate::provider::{Capabilities, Completion, CompletionRequest, Provider, StopReason};

/// `SamplingParams::validate`'s own ceiling, applied to a reply budget rather
/// than refused: a budget that is too big is a rounding mistake, not a bug.
const MAX_REPLY_TOKENS: u32 = 32_768;

/// A local `.gguf` file, asked for one finished answer at a time.
pub struct GgufProvider {
    name: String,
    model_id: String,
    path: PathBuf,
    /// Read out of the file's own header at construction, which is what lets the
    /// engine detect the architecture — and therefore pick a chat template —
    /// before any weights are in memory.
    metadata: Option<ModelMetadata>,
    options: LoadModelOptions,
    params: SamplingParams,
    stop: Vec<String>,
    /// The window as last known: the config and the file header before loading,
    /// the handle's real context length after.
    window: AtomicU32,
    engine: Arc<Mutex<EngineManager>>,
    cancel: CancelToken,
}

impl GgufProvider {
    /// The provider a `[providers.<name>] type = "gguf"` section names.
    pub fn new(name: &str, config: &ProviderConfig) -> ProviderResult<Self> {
        let kind = EngineKind::LlamaCpp;
        if !kind.is_available() {
            // `is_available` is this repo's own "can serve a request" question,
            // answered by the library rather than guessed from the config.
            return Err(ProviderError::not_compiled("gguf", "llama-cpp"));
        }
        Self::with_engine(name, config, kind.build())
    }

    /// Build the adapter around an engine the caller has already chosen.
    ///
    /// This is the seam tests use: MockEngine keeps the same streaming,
    /// cancellation and template-fallback contract as llama.cpp, so the
    /// translation can be exercised on a machine with no native toolchain.
    /// [`Self::new`] is the door a configuration goes through, and it will not
    /// hand back an engine that cannot run weights.
    pub fn with_engine(
        name: &str,
        config: &ProviderConfig,
        engine: Box<dyn Engine>,
    ) -> ProviderResult<Self> {
        let path = model_path(config, name)?;
        let metadata = GgufHeader::read(&path).ok().map(|header| header.summary());
        let (backend, gpu_layers) = device_choice(config.device.as_deref(), &path);
        let window = config
            .context_length
            .or_else(|| metadata.as_ref().and_then(|entry| entry.context_length))
            .unwrap_or_else(|| LoadModelOptions::default().context_length);
        let stop = stop_sequences(config, name)?;
        let mut params = SamplingParams::preset("coding");
        params.temperature = config.temperature.unwrap_or(params.temperature);
        params.top_p = config.top_p.unwrap_or(params.top_p);
        // No seed unless a request asks for one: a fixed seed makes the repair
        // round ask the same question and get the same truncated answer back.
        params.seed = None;
        params.max_tokens = config.max_tokens.unwrap_or(params.max_tokens);

        Ok(Self {
            name: name.to_string(),
            model_id: model_id(config, &path),
            path,
            metadata,
            options: LoadModelOptions {
                context_length: window,
                gpu_layers,
                backend,
                threads: None,
            },
            params,
            stop,
            window: AtomicU32::new(window),
            engine: Arc::new(Mutex::new(EngineManager::with_engine(engine))),
            cancel: CancelToken::new(),
        })
    }

    /// Share a cancellation handle with whoever owns the run.
    ///
    /// The loop holds its own token and checks it between steps; this is the same
    /// wire pulled from the other end, so a stop pressed mid-generation reaches
    /// the decode thread instead of waiting for an answer to finish. The
    /// [`Provider`] trait has no way to receive it, so nothing in `smoll task`
    /// calls this yet — a run that times out still stops llama.cpp, because the
    /// dropped stream is the other half of that contract, and the engine's decode
    /// worker ends when its consumer goes. This is the seam the TUI's Esc key uses.
    pub fn with_cancel(mut self, cancel: CancelToken) -> Self {
        self.cancel = cancel;
        self
    }

    fn window(&self) -> usize {
        usize::try_from(self.window.load(Ordering::Relaxed)).unwrap_or(usize::MAX)
    }

    /// Run `with` against the manager, holding the lock only for the sync part.
    ///
    /// Generations are handed out of the lock and consumed outside it, so a long
    /// decode never keeps a second caller waiting on a mutex.
    fn with_manager<T>(&self, with: impl FnOnce(&mut EngineManager) -> T) -> T {
        with(&mut lock(&self.engine))
    }

    fn load_request(&self) -> LoadModelRequest {
        LoadModelRequest {
            model_id: self.model_id.clone(),
            // Left blank so the engine prefers the name inside the file.
            display_name: String::new(),
            path: self.path.clone(),
            options: self.options.clone(),
            metadata: self.metadata.clone(),
        }
    }

    /// The weights, in memory, before the first token of the run.
    ///
    /// The `allow` is a judgement about one await, not a blanket one: the blocking
    /// closure carries a *clone* of the provider's `Arc`, and this is a method on
    /// `&self`, so the provider always outlives the call. A load cancelled mid-way
    /// therefore cannot be the last handle to the engine, and dropping a non-final
    /// clone runs no llama.cpp teardown at all.
    #[allow(clippy::futures_drop_with_ffi)]
    async fn ensure_loaded(&self) -> ProviderResult<()> {
        if self
            .with_manager(|manager| manager.loaded_handle())
            .is_some()
        {
            return Ok(());
        }

        let engine = Arc::clone(&self.engine);
        let request = self.load_request();
        // Reading a multi-gigabyte file and mapping its tensors takes seconds, and
        // the loop runs on a tokio worker: on the executor thread, every other
        // task — including the TUI's input pump — waits for the weights.
        let loaded = tokio::task::spawn_blocking(move || lock(&engine).load(request))
            .await
            .map_err(|join| ProviderError::Other {
                message: format!("the model loader stopped: {join}"),
            })?
            .map_err(|error| self.engine_error(error))?;

        for warning in &loaded.warnings {
            tracing::warn!(provider = %self.name, warning = %warning, "loading the model needed a change");
        }
        // The file's own number is the one to trim a prompt against from now on:
        // llama.cpp caps a context at what the weights were trained with, which a
        // config written for a different model can overstate.
        self.window
            .store(loaded.handle.context_length, Ordering::Relaxed);
        tracing::info!(
            provider = %self.name,
            model = %loaded.handle.model_id,
            engine = %loaded.engine,
            context = loaded.handle.context_length,
            device = %loaded.handle.metadata.architecture.as_deref().unwrap_or("unknown"),
            "a gguf provider loaded its model"
        );
        Ok(())
    }

    /// The engine's request for one step of the loop.
    fn chat_request(&self, request: &CompletionRequest) -> ChatRequest {
        ChatRequest {
            request_id: uuid::Uuid::new_v4().simple().to_string(),
            model_id: self.model_id.clone(),
            // The loop puts its instructions in a system message, and both the
            // model's template and the architecture table render one from the
            // list. Filling `system_prompt` as well would say them twice.
            messages: usable(&request.messages),
            system_prompt: None,
            params: self.params_for(request),
            stop: self.stop.clone(),
        }
    }

    fn params_for(&self, request: &CompletionRequest) -> SamplingParams {
        let mut params = self.params.clone();
        if let Some(temperature) = request.temperature {
            params.temperature = temperature;
        }
        if let Some(top_p) = request.top_p {
            params.top_p = top_p;
        }
        if request.seed.is_some() {
            params.seed = request.seed;
        }
        params.max_tokens = request
            .max_tokens
            .unwrap_or(params.max_tokens)
            .clamp(1, MAX_REPLY_TOKENS);
        params
    }

    /// Render the prompt, check it against the window and start decoding.
    fn start(&self, chat: &ChatRequest) -> ProviderResult<(TokenStream, u32)> {
        let (prompt, needed, window) = self.with_manager(|manager| {
            let prompt = manager.render_prompt(chat);
            // The model's own tokeniser is the only one allowed to say whether
            // this prompt fits, and it has to be asked before llama.cpp starts
            // prefilling. A prompt that overflow costs one error either way; the
            // numbers are what differ.
            let needed = manager
                .tokenize(&prompt)
                .map(|tokens| tokens.len())
                .unwrap_or_else(|_| approx_token_count(&prompt) as usize);
            let window = manager.loaded_handle().map_or_else(
                || self.window(),
                |handle| usize::try_from(handle.context_length).unwrap_or(usize::MAX),
            );
            (prompt, needed, window)
        });
        if needed + 1 > window {
            // `+ 1` is the same arithmetic the engine applies to its own context:
            // one position has to stay free for the first sampled token.
            return Err(ProviderError::ContextTooLong {
                wanted: needed,
                available: window,
            });
        }

        let params = chat.params.clone();
        let stop = chat.stop.clone();
        let request_id = chat.request_id.clone();
        self.with_manager(|manager| {
            manager
                .start_generation(request_id, prompt, params, stop)
                .map(|stream| (stream, needed as u32))
                .map_err(|error| self.engine_error(error))
        })
    }

    async fn collect(
        &self,
        mut stream: TokenStream,
        request_id: &str,
        prompt_tokens: u32,
    ) -> ProviderResult<Completion> {
        let mut text = String::new();
        let mut usage = None;
        let mut reason = None;
        while let Some(item) = stream.next().await {
            if self.cancel.is_cancelled() {
                self.stop_generation(request_id);
                return Err(ProviderError::Cancelled);
            }
            match item {
                Ok(token) => {
                    text.push_str(&token.text);
                    usage = token.usage.or(usage);
                    reason = token.finish_reason.or(reason);
                }
                Err(error) => {
                    self.stop_generation(request_id);
                    return Err(self.engine_error(error));
                }
            }
        }
        if reason.is_none() {
            if self.cancel.is_cancelled() {
                // The stream ended without saying why, and the human is the reason.
                // A finished answer is still returned after a stop, because the
                // stop was answered before it took effect and the text is real.
                return Err(ProviderError::Cancelled);
            }
            return Err(ProviderError::Malformed {
                message: format!(
                    "{} stopped mid-answer without saying why",
                    self.with_manager(|manager| manager.engine_name())
                ),
            });
        }
        // The engine's own numbers when it got them, and the same estimate every
        // other layer falls back on when a stream was cut short before its
        // terminal token.
        let usage =
            usage.unwrap_or_else(|| TokenUsage::new(prompt_tokens, approx_token_count(&text)));
        Ok(Completion {
            text,
            // Nothing here reads calls out of a template, so the loop is not told
            // the model can hold a tool list: it asks for prose and parses that.
            calls: Vec::new(),
            stop: stop_reason(reason.as_deref()),
            usage,
        })
    }

    fn stop_generation(&self, request_id: &str) {
        self.with_manager(|manager| manager.stop(request_id));
    }

    /// Turn an engine failure into the kind of answer the loop can act on.
    ///
    /// `retryable` is the point of the split: out of memory and a busy decoder are
    /// worth another try, while a file that is not there never is — and the loop
    /// would spend its retries learning the same thing.
    fn engine_error(&self, error: AppError) -> ProviderError {
        let message = error.to_string();
        match error {
            // The engine never got going, which the loop reads as a provider that
            // could not be started — and whose message says which file it tried.
            AppError::ModelNotFound(_)
            | AppError::ModelNotDownloaded(_)
            | AppError::EngineLoadFailed(_)
            | AppError::UnsupportedBackend(_) => ProviderError::Unreachable {
                target: self.path.display().to_string(),
                message,
            },
            // A decoder that failed mid-answer, a machine that ran out of room, an
            // I/O error under the read: all worth another try.
            _ => ProviderError::Other { message },
        }
    }
}

#[async_trait]
impl Provider for GgufProvider {
    fn name(&self) -> &str {
        &self.name
    }

    fn capabilities(&self) -> Capabilities {
        // `streaming` is false because the trait has no way to show a partial
        // answer: the engine streams to this provider, which hands the loop one
        // finished completion. `tool_calling` is false because no GGUF file has
        // said it can hold a tool list — its template says what it says, and a
        // model that cannot be trusted with one is what the prose path is for.
        Capabilities::basic().window(self.window())
    }

    fn model(&self) -> Option<&str> {
        Some(&self.model_id)
    }

    async fn complete(&self, request: &CompletionRequest) -> ProviderResult<Completion> {
        if self.cancel.is_cancelled() {
            return Err(ProviderError::Cancelled);
        }
        if request.messages.is_empty() {
            return Err(ProviderError::Malformed {
                message: "a completion was asked for with no messages in it".to_owned(),
            });
        }
        let chat = self.chat_request(request);
        // One load for the whole run: the weights stay resident between steps,
        // which is the only reason a second step costs a generation and not a
        // file read.
        self.ensure_loaded().await?;
        let (stream, prompt_tokens) = self.start(&chat)?;
        self.collect(stream, &chat.request_id, prompt_tokens).await
    }
}

/// The file a gguf section points at, or the reason it cannot be used.
fn model_path(config: &ProviderConfig, name: &str) -> ProviderResult<PathBuf> {
    let raw = config.path.as_deref().unwrap_or_default().trim();
    if raw.is_empty() {
        return Err(ProviderError::Unreachable {
            target: name.to_owned(),
            message: format!("a gguf provider needs providers.{name}.path to name a .gguf file"),
        });
    }
    let path = expand_home(raw);
    if !path.is_file() {
        return Err(ProviderError::Unreachable {
            target: raw.to_owned(),
            message: "there is no model file here to load".to_owned(),
        });
    }
    Ok(path)
}

/// `~/Models/small.gguf` is how a person writes a path in a config file, and the
/// starter `smoll.toml` offers exactly that shape — so it has to mean the home
/// directory here rather than a folder literally called `~`. With no home in the
/// environment the words are left as written, and the refusal names them.
fn expand_home(raw: &str) -> PathBuf {
    let home = || {
        std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from)
    };
    if raw == "~" {
        return home().unwrap_or_else(|| PathBuf::from(raw));
    }
    match raw.strip_prefix("~/").or_else(|| raw.strip_prefix("~\\")) {
        Some(rest) => home()
            .map(|home| home.join(rest))
            .unwrap_or_else(|| PathBuf::from(raw)),
        None => PathBuf::from(raw),
    }
}

/// Every stop marker the run should honour, in the shape the engines accept.
fn stop_sequences(config: &ProviderConfig, name: &str) -> ProviderResult<Vec<String>> {
    normalize_stop_sequences(&config.stop).map_err(|error| ProviderError::Unreachable {
        target: name.to_owned(),
        message: error.to_string(),
    })
}

/// The compute device the config names, and how many layers to ask for.
///
/// A device is a request, not a promise: llama.cpp is asked at load time whether
/// it can offload at all, and only an explicit `cpu` turns the question off.
/// An unrecognised name is reported and ignored rather than guessed at, because
/// the honest default is the one the engine measures.
fn device_choice(requested: Option<&str>, path: &Path) -> (Backend, i32) {
    let Some(raw) = requested.map(str::trim).filter(|value| !value.is_empty()) else {
        return (Backend::default(), -1);
    };
    match raw.to_ascii_lowercase().replace('-', "").as_str() {
        "cpu" => (Backend::Cpu, 0),
        "metal" | "mtl" => (Backend::Metal, -1),
        "cuda" => (Backend::Cuda, -1),
        "vulkan" => (Backend::Vulkan, -1),
        other => {
            tracing::warn!(
                device = other,
                file = %path.display(),
                "this device name means nothing to the engine, so offloading is left to llama.cpp to decide"
            );
            (Backend::default(), -1)
        }
    }
}

/// What the model is called in a transcript: the config's word, or the file's.
fn model_id(config: &ProviderConfig, path: &Path) -> String {
    config
        .model
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| {
            path.file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
                .unwrap_or_else(|| "gguf".to_owned())
        })
}

/// A turn with nothing in it carries no information, and the engines refuse an
/// empty assistant message rather than prompting with it — so a model that once
/// answered with an empty string does not cost the rest of the run.
fn usable(messages: &[ChatMessage]) -> Vec<ChatMessage> {
    messages
        .iter()
        .filter(|message| !is_blank_turn(message))
        .cloned()
        .collect()
}

fn is_blank_turn(message: &ChatMessage) -> bool {
    matches!(message.role, smollm_core::chat::Role::Assistant) && message.content.is_empty()
}

/// llama.cpp's `finish_reason` in this crate's vocabulary.
///
/// A matched stop sequence is reported as `stop`, the same word for the model
/// finishing its turn, so `StopSequence` is not something an engine can tell us;
/// the loop only asks whether the answer was cut off, and `length` answers that.
fn stop_reason(reason: Option<&str>) -> StopReason {
    match reason {
        Some("length") => StopReason::Length,
        _ => StopReason::EndOfTurn,
    }
}

fn lock<T>(mutex: &Arc<Mutex<T>>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use smollm_core::chat::{chat_message, Role};
    use smollm_engine::mock::{MockConfig, MockEngine};

    fn config(path: &Path) -> ProviderConfig {
        ProviderConfig {
            path: Some(path.to_string_lossy().into_owned()),
            ..ProviderConfig::new(agent_config::ProviderKind::Gguf)
        }
    }

    /// A file to point at: the injected engine reads nothing from it, and the
    /// real one is not in this build.
    fn model_file(dir: &Path) -> PathBuf {
        let path = dir.join("qwen2.5-1.5b-instruct-q4_k_m.gguf");
        std::fs::write(&path, b"not really weights").expect("write a stand-in file");
        path
    }

    fn mock_engine() -> Box<dyn Engine> {
        Box::new(MockEngine::with_config(MockConfig::instant()))
    }

    fn provider(dir: &Path) -> GgufProvider {
        GgufProvider::with_engine("local", &config(&model_file(dir)), mock_engine())
            .expect("a provider around the mock engine")
    }

    fn request(text: &str) -> CompletionRequest {
        CompletionRequest::new(vec![
            chat_message(Role::System, "You edit code and never push it."),
            chat_message(Role::User, text),
        ])
    }

    #[test]
    fn a_gguf_provider_this_build_cannot_run_names_the_feature_instead_of_answering() {
        // Under `--features gguf` alone the engine crate holds MockEngine and the
        // metadata reader, neither of which runs weights, so `build` must refuse.
        let dir = tempfile::tempdir().expect("temp dir");
        let outcome = GgufProvider::new("local", &config(&model_file(dir.path())));
        if EngineKind::LlamaCpp.is_available() {
            assert!(
                outcome.is_ok(),
                "a linked engine is the answer a configuration should get"
            );
        } else {
            let error = outcome.err().expect("no linked engine in this build");
            let message = error.to_string();
            assert!(message.contains("gguf"), "{message}");
            assert!(message.contains("llama-cpp"), "{message}");
            assert!(!error.retryable(), "{message}");
        }
    }

    #[test]
    fn a_path_that_is_not_a_model_file_is_refused_before_anything_loads() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut config = config(&dir.path().join("nowhere.gguf"));
        let error = GgufProvider::with_engine("local", &config, mock_engine())
            .err()
            .expect("the file is not there");
        assert!(error.to_string().contains("nowhere.gguf"), "{error}");

        config.path = None;
        let error = GgufProvider::with_engine("small", &config, mock_engine())
            .err()
            .expect("no path at all");
        assert!(error.to_string().contains("small"), "{error}");
        assert!(error.to_string().contains("path"), "{error}");
    }

    /// The starter `smoll.toml` shows a path as `~/Models/…`, so that shape has to
    /// mean the home folder rather than a directory literally called `~`.
    #[test]
    fn a_home_shortcut_in_a_config_path_is_the_home_folder() {
        let expanded = expand_home("~/Models/small.gguf");
        assert!(
            expanded.is_absolute(),
            "{} is not where a home folder is",
            expanded.display()
        );
        assert_eq!(
            expanded.file_name().and_then(|name| name.to_str()),
            Some("small.gguf")
        );
        assert!(
            !expanded.to_string_lossy().contains('~'),
            "the tilde went away: {}",
            expanded.display()
        );
        if let Some(home) = std::env::var_os("HOME") {
            assert_eq!(expanded, PathBuf::from(home).join("Models/small.gguf"));
        }
        // Anything else is the caller's own words: a relative path stays relative
        // and an absolute one stays itself.
        assert_eq!(
            expand_home("Models/small.gguf"),
            PathBuf::from("Models/small.gguf")
        );
        assert_eq!(
            expand_home("~other/small.gguf"),
            PathBuf::from("~other/small.gguf"),
            "a named user is not this one"
        );
    }

    #[tokio::test]
    async fn the_answer_comes_from_the_engine_and_carries_its_token_counts() {
        let dir = tempfile::tempdir().expect("temp dir");
        let provider = provider(dir.path());
        let answer = provider
            .complete(&request("what does this repo do?"))
            .await
            .expect("the mock engine answers");

        assert!(
            answer.text.contains("what does this repo do?"),
            "the prompt reached the engine: {}",
            answer.text
        );
        assert!(
            answer.text.contains(MockEngine::DISCLAIMER),
            "a simulated engine says so in its own words"
        );
        assert_eq!(answer.stop, StopReason::EndOfTurn);
        assert!(answer.usage.prompt_tokens > 0, "{:?}", answer.usage);
        assert!(answer.usage.completion_tokens > 0, "{:?}", answer.usage);
        assert_eq!(
            answer.usage.total_tokens,
            answer.usage.prompt_tokens + answer.usage.completion_tokens
        );
    }

    #[tokio::test]
    async fn the_reply_budget_the_loop_asks_for_is_the_budget_the_engine_gets() {
        let dir = tempfile::tempdir().expect("temp dir");
        let provider = provider(dir.path());
        let mut request = request("write at length please");
        request.max_tokens = Some(1);
        let answer = provider
            .complete(&request)
            .await
            .expect("one token is still an answer");
        assert_eq!(answer.stop, StopReason::Length, "it ran out of budget");
        assert_eq!(answer.usage.completion_tokens, 1, "{:?}", answer.usage);
    }

    #[tokio::test]
    async fn the_model_is_named_by_the_file_and_the_config_in_that_order() {
        let dir = tempfile::tempdir().expect("temp dir");
        let provider = provider(dir.path());
        assert_eq!(
            provider.model(),
            Some("qwen2.5-1.5b-instruct-q4_k_m"),
            "a config with no model name still says which file is answering"
        );

        let mut config = config(&model_file(dir.path()));
        config.model = Some("qwen2.5:1.5b".to_owned());
        let provider = GgufProvider::with_engine("local", &config, mock_engine()).expect("built");
        assert_eq!(provider.model(), Some("qwen2.5:1.5b"));
    }

    #[tokio::test]
    async fn the_capability_floor_is_a_window_and_two_honest_nos() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut config = config(&model_file(dir.path()));
        // Small enough that the engine's own number wins, and large enough to
        // pass agent-config's floor.
        config.context_length = Some(4096);
        let provider = GgufProvider::with_engine("local", &config, mock_engine()).expect("built");
        assert_eq!(provider.capabilities().context_tokens, 4096);
        // Nothing is claimed about streaming or tool calls that the loop cannot use.
        assert!(!provider.capabilities().streaming);
        assert!(!provider.capabilities().tool_calling);
    }

    #[tokio::test]
    async fn a_prompt_wider_than_the_window_is_refused_with_both_numbers() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut config = config(&model_file(dir.path()));
        config.context_length = Some(64);
        let provider = GgufProvider::with_engine("local", &config, mock_engine()).expect("built");

        let long = "measure twice cut once ".repeat(60);
        let error = provider
            .complete(&request(&long))
            .await
            .expect_err("no room for an answer");
        match error {
            ProviderError::ContextTooLong { wanted, available } => {
                assert_eq!(available, 64, "{available}");
                assert!(wanted > 64, "{wanted} tokens fit in 64?");
            }
            other => panic!("expected a window refusal, got {other:?}"),
        }
        assert!(!error.retryable(), "asking again needs a shorter prompt");
    }

    #[tokio::test]
    async fn an_empty_turn_does_not_cost_the_rest_of_the_run() {
        let dir = tempfile::tempdir().expect("temp dir");
        let provider = provider(dir.path());
        // The loop records every answer, and a model that answered with nothing
        // would otherwise leave a turn the engines refuse to prompt with.
        let mut request = request("and again");
        request
            .messages
            .push(chat_message(Role::Assistant, String::new()));
        request
            .messages
            .push(chat_message(Role::User, "keep going"));
        let answer = provider
            .complete(&request)
            .await
            .expect("the blank turn is dropped, not sent");
        assert!(answer.text.contains("keep going"), "{}", answer.text);
    }

    #[tokio::test]
    async fn nothing_at_all_in_the_prompt_is_a_refusal_rather_than_a_hang() {
        let dir = tempfile::tempdir().expect("temp dir");
        let provider = provider(dir.path());
        let error = provider
            .complete(&CompletionRequest::new(Vec::new()))
            .await
            .expect_err("there is no transcript to render");
        assert!(matches!(error, ProviderError::Malformed { .. }), "{error}");
    }

    #[tokio::test]
    async fn a_cancelled_run_stops_instead_of_waiting_for_the_answer() {
        let dir = tempfile::tempdir().expect("temp dir");
        let cancel = CancelToken::new();
        cancel.cancel();
        let provider = provider(dir.path()).with_cancel(cancel);
        let error = provider
            .complete(&request("stop before you start"))
            .await
            .expect_err("the human stopped it");
        assert!(matches!(error, ProviderError::Cancelled), "{error}");
        assert!(!error.retryable(), "a stopped run is not a failed one");
    }

    #[tokio::test]
    async fn the_same_model_is_loaded_once_across_a_run() {
        let dir = tempfile::tempdir().expect("temp dir");
        let provider = provider(dir.path());
        provider
            .complete(&request("first step"))
            .await
            .expect("answers");
        // A fresh load mints a new handle id, so the same id on the second step
        // means that step read from weights already in memory.
        let first = provider
            .with_manager(|manager| manager.loaded_handle())
            .expect("loaded by the first step");
        provider
            .complete(&request("second step"))
            .await
            .expect("answers again");
        let second = provider
            .with_manager(|manager| manager.loaded_handle())
            .expect("still loaded");
        assert_eq!(first.id, second.id, "the handle did not change");
        let metrics = provider.with_manager(|manager| manager.metrics());
        assert_eq!(metrics.requests, 2, "and each step cost one generation");
    }

    #[test]
    fn a_device_is_a_request_and_cpu_means_no_offload_at_all() {
        assert_eq!(device_choice(None, Path::new("m.gguf")).1, -1);
        assert_eq!(device_choice(Some("cpu"), Path::new("m.gguf")).1, 0);
        assert_eq!(
            device_choice(Some("  METAL "), Path::new("m.gguf")).0,
            Backend::Metal
        );
        // A name the engine does not know is left alone rather than guessed at.
        assert_eq!(
            device_choice(Some("neuron"), Path::new("m.gguf")).1,
            -1,
            "an unknown device still asks llama.cpp"
        );
    }

    #[test]
    fn blank_turns_go_and_the_rest_keep_their_order() {
        let messages = vec![
            chat_message(Role::System, "rules"),
            chat_message(Role::Assistant, ""),
            chat_message(Role::User, "do the thing"),
            chat_message(Role::Assistant, "listed src"),
        ];
        let kept = usable(&messages);
        assert_eq!(kept.len(), 3, "only the blank turn leaves");
        assert_eq!(kept[0].role, Role::System);
        assert_eq!(kept[1].content, "do the thing", "order is not reordered");
        assert_eq!(kept[2].content, "listed src");
    }

    #[test]
    fn a_stop_marker_that_is_too_long_is_the_config_s_problem_not_the_runs() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut config = config(&model_file(dir.path()));
        config.stop = vec!["x".repeat(200)];
        let error = GgufProvider::with_engine("local", &config, mock_engine())
            .err()
            .expect("the engine would ignore it anyway");
        assert!(error.to_string().contains("stop sequence"), "{error}");
    }

    #[test]
    fn only_a_real_end_of_budget_reads_as_truncated() {
        assert_eq!(stop_reason(Some("length")), StopReason::Length);
        assert_eq!(stop_reason(Some("stop")), StopReason::EndOfTurn);
        // The mock's own word for finishing, and anything a server invents.
        assert_eq!(stop_reason(Some("eos")), StopReason::EndOfTurn);
        assert_eq!(stop_reason(None), StopReason::EndOfTurn);
    }

    /// A file that is simply not there is refused at construction; this is the
    /// engine's own version of bad news, which has to keep the file's name in it.
    #[test]
    fn an_engine_failure_keeps_the_file_it_tried_and_stays_worth_another_try() {
        let dir = tempfile::tempdir().expect("temp dir");
        let provider = provider(dir.path());
        let error = provider.engine_error(AppError::EngineLoadFailed(
            "the tensor data is truncated".to_owned(),
        ));
        let message = error.to_string();
        assert!(
            message.contains("qwen2.5-1.5b-instruct-q4_k_m.gguf"),
            "{message}"
        );
        assert!(error.retryable(), "a second attempt may find the room");
        assert!(provider
            .engine_error(AppError::InsufficientMemory {
                required_gb: 12.0,
                available_gb: 4.0,
            })
            .retryable());
    }

    #[test]
    fn the_tool_list_is_not_sent_because_the_provider_cannot_read_the_answer() {
        // A guard on the honest floor rather than a feature: the loop only offers
        // schemas when `tool_calling` says so, and this provider never says so.
        let dir = tempfile::tempdir().expect("temp dir");
        let provider = provider(dir.path());
        let mut request = request("list the files");
        request.tools = vec![crate::provider::ToolSchema::new(
            "list_files",
            "List a directory",
            json!({"type": "object"}),
        )];
        assert!(!provider.capabilities().tool_calling);
    }
}
