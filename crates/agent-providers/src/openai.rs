//! Any server that speaks the OpenAI chat wire format: Ollama, LM Studio,
//! vLLM, TGI, llama-server, or `smoll serve` from this repository.
//!
//! This is the only provider here that talks to something outside this process,
//! so it carries two rules the others do not need:
//!
//! - **A key goes to one host, and to that host only.** A configuration names an
//!   environment *variable*, never a key, and the bearer is attached to a request
//!   for the URL that section gave. Redirects are not followed at all, because
//!   following one would hand the key to whichever host the server chose to point
//!   at.
//! - **The server's own numbers are used when it gives them.** A reply with a
//!   `usage` block reports the tokens the model actually paid; only a server that
//!   sends none falls back on the character heuristic the rest of this repository
//!   estimates with.
//!
//! Everything about generation still belongs elsewhere. This file builds one
//! JSON request, reads one JSON reply, and turns a status code into the kind of
//! error that says whether asking again makes sense.

use async_trait::async_trait;
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, RETRY_AFTER};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use smollm_core::chat::{approx_token_count, normalize_stop_sequences, TokenUsage};
use url::Url;

use agent_config::ProviderConfig;
use agent_tools::ToolCall;

use crate::build::capabilities_of;
use crate::error::{ProviderError, ProviderResult};
use crate::provider::{
    usable_messages, Capabilities, Completion, CompletionRequest, Provider, StopReason, ToolSchema,
};

/// The one path this provider posts to, whichever way `base_url` was written.
const CHAT_PATH: &str = "chat/completions";

/// How much of a server's error body is quoted back: enough to act on, short
/// enough that a stack trace does not become the run's error message.
const MAX_ERROR_QUOTE: usize = 300;

/// A server that answers `POST /v1/chat/completions`.
pub struct OpenAiProvider {
    name: String,
    model: Option<String>,
    endpoint: Url,
    /// The bearer, read once and already attached. Private to the struct so that
    /// describing a provider cannot print it.
    headers: HeaderMap,
    capabilities: Capabilities,
    temperature: Option<f32>,
    top_p: Option<f32>,
    max_tokens: Option<u32>,
    stop: Vec<String>,
    http: reqwest::Client,
}

/// A provider that points at a server is worth describing — as long as the
/// description cannot carry a key out of the process. A run's log, an error's
/// `Debug` and a panic hook all reach this impl.
impl std::fmt::Debug for OpenAiProvider {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OpenAiProvider")
            .field("name", &self.name)
            .field("model", &self.model)
            .field("endpoint", &self.endpoint.as_str())
            .field("capabilities", &self.capabilities)
            .field("key", &"[from the environment, not printed]")
            .finish_non_exhaustive()
    }
}

impl OpenAiProvider {
    /// Build the provider a `[providers.<name>]` section with
    /// `type = "openai_compatible"` describes.
    ///
    /// The key is read here, before the run, so that a configuration pointing at
    /// a variable that is not set costs one line of output rather than the first
    /// step of a hundred-step run.
    pub fn new(name: &str, config: &ProviderConfig) -> ProviderResult<Self> {
        Ok(Self {
            name: name.to_owned(),
            model: trimmed(config.model.as_deref()),
            endpoint: chat_endpoint(config.base_url.as_deref(), name)?,
            headers: bearer_headers(name, config)?,
            capabilities: capabilities_of(config),
            temperature: config.temperature,
            top_p: config.top_p,
            max_tokens: config.max_tokens,
            stop: normalized_stop(config, name)?,
            // No client-side timeout: the loop owns the run's budget and reports a
            // step that ran out of it. A second deadline here would only disagree.
            http: reqwest::Client::builder()
                // Following a redirect would resend the bearer to a host the
                // configuration never named. `smoll serve`, Ollama, LM Studio and
                // vLLM all answer on the URL they were started with.
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .map_err(|error| ProviderError::Other {
                    message: format!("an HTTP client could not be made for {name}: {error}"),
                })?,
        })
    }

    /// The URL this provider posts to, for `doctor` and for an error message.
    pub fn endpoint(&self) -> &str {
        self.endpoint.as_str()
    }

    /// Whether a key was read from the environment — said without saying which
    /// variable, what it holds, or where it goes.
    pub fn sends_a_key(&self) -> bool {
        self.headers.contains_key(AUTHORIZATION)
    }

    /// The request body. An option the config does not mention is left out
    /// entirely, so the server's own defaults survive.
    fn body(&self, request: &CompletionRequest) -> Value {
        // `Role` serialises lowercase and a message is `{role, content}`, so the
        // list this repo carries is already the list the wire wants.
        let mut body = json!({ "messages": usable_messages(&request.messages) });
        if let Some(model) = &self.model {
            body["model"] = json!(model);
        }
        // A setting for this step from the loop wins over the one in the config,
        // which wins over saying nothing and letting the server decide.
        put(
            &mut body,
            "temperature",
            request.temperature.or(self.temperature),
        );
        put(&mut body, "top_p", request.top_p.or(self.top_p));
        put(
            &mut body,
            "max_tokens",
            request.max_tokens.or(self.max_tokens),
        );
        put(&mut body, "seed", request.seed);

        let mut stop = self.stop.clone();
        stop.extend(request.stop.iter().cloned());
        if !stop.is_empty() {
            body["stop"] = json!(stop);
        }
        // Only a server this configuration trusts with a tool list is given one:
        // `capabilities_of` decides, and the loop leaves `request.tools` empty
        // when the answer is no.
        if self.capabilities.tool_calling && !request.tools.is_empty() {
            body["tools"] = json!(request.tools.iter().map(function_tool).collect::<Vec<_>>());
        }
        body
    }

    /// The reply, in this crate's vocabulary.
    fn completion(
        &self,
        reply: ChatReply,
        request: &CompletionRequest,
    ) -> ProviderResult<Completion> {
        let choice = reply
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| ProviderError::Malformed {
                message: format!("{} answered with no choices at all", self.endpoint),
            })?;
        let message = choice.message.unwrap_or_default();
        let text = message.text();
        // A call for a tool nobody offered is not a request but a hallucination,
        // and acting on it would be this client's fault rather than the model's.
        let calls = offered(message.tool_calls, &request.tools);
        let usage = usage_for(reply.usage, request, &text);
        Ok(Completion {
            text,
            usage,
            stop: stop_reason(choice.finish_reason.as_deref(), !calls.is_empty()),
            calls,
        })
    }

    /// A server that did not answer at all. The URL is quoted back because "is
    /// Ollama running, and on which port" is the question the answer decides.
    fn unreachable(&self, error: &reqwest::Error) -> ProviderError {
        let message = if error.is_timeout() {
            "the request timed out waiting for a reply".to_owned()
        } else if error.is_connect() {
            "nothing is listening there".to_owned()
        } else {
            format!("{error}")
        };
        ProviderError::Unreachable {
            target: self.endpoint.to_string(),
            message,
        }
    }

    /// A status that is not a completion, sorted by whether asking again helps.
    async fn refused(
        &self,
        status: reqwest::StatusCode,
        response: reqwest::Response,
        request: &CompletionRequest,
    ) -> ProviderError {
        let code = status.as_u16();
        let retry_after = response
            .headers()
            .get(RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.trim().parse::<u64>().ok());
        let location = redirect_target(response.headers());
        // The body is read for its explanation, not for its size.
        let body = response
            .text()
            .await
            .unwrap_or_default()
            .chars()
            .take(MAX_ERROR_QUOTE)
            .collect::<String>();
        let quote = if body.is_empty() {
            String::new()
        } else {
            format!(": {}", one_line(&body))
        };
        let target = self.endpoint.to_string();
        match code {
            401 | 403 => ProviderError::Unauthorized {
                status: format!("{code}{quote}"),
            },
            // Not followed, so a bearer can never leave for a host the
            // configuration did not name.
            300..=399 => ProviderError::Unauthorized {
                status: format!(
                    "{code}{}; this client does not follow a redirect, because a key must not be sent to a host your config did not name",
                    match location {
                        Some(location) => format!(" moved to {location}"),
                        None => String::new(),
                    }
                ),
            },
            404 | 405 => ProviderError::Unreachable {
                target,
                message: format!("{code}{quote} — is that base_url the server's own, up to /v1?"),
            },
            429 => ProviderError::RateLimited {
                message: format!("the server is busy{quote}"),
                retry_after_seconds: retry_after,
            },
            500..=599 => ProviderError::Unreachable {
                target,
                message: format!("the server failed with {code}{quote}"),
            },
            // The one 400 the loop can do something about: it has to cut the
            // prompt, not ask for the same prompt again.
            400 if is_context_too_long(&body) => ProviderError::ContextTooLong {
                wanted: prompt_and_budget(request, self.max_tokens),
                available: self.capabilities.context_tokens,
            },
            _ => ProviderError::Malformed {
                message: format!("{code}{quote}"),
            },
        }
    }
}

/// Add a field only when there is something to say.
fn put(body: &mut Value, key: &str, value: Option<impl Serialize>) {
    if let Some(value) = value {
        body[key] = json!(value);
    }
}

/// This repo's flat tool, in the nested shape the OpenAI wire asks for.
fn function_tool(tool: &ToolSchema) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": tool.name,
            "description": tool.description,
            "parameters": tool.parameters,
        }
    })
}

/// Keep only calls for a tool the model was offered, with arguments the loop can
/// actually hand to it.
fn offered(calls: Vec<WireCall>, tools: &[ToolSchema]) -> Vec<ToolCall> {
    calls
        .into_iter()
        .filter(|call| tools.iter().any(|tool| tool.name == call.name()))
        .filter_map(WireCall::into_tool_call)
        .collect()
}

/// The token counts for one answer.
///
/// A server's `usage` block is what the model actually paid, so it wins. An empty
/// one, or none at all, falls back on the same character heuristic every estimate
/// in this repository uses — which is the number the loop's own budget check works
/// from, not a claim about the model.
fn usage_for(usage: Option<WireUsage>, request: &CompletionRequest, text: &str) -> TokenUsage {
    match usage.filter(WireUsage::is_real) {
        Some(usage) => usage.into_usage(),
        None => TokenUsage::new(request.prompt_tokens(), approx_token_count(text)),
    }
}

/// The prompt plus the reply budget this run asked for, in tokens.
fn prompt_and_budget(request: &CompletionRequest, configured: Option<u32>) -> usize {
    request.prompt_tokens() as usize + configured.or(request.max_tokens).unwrap_or(0) as usize
}

#[async_trait]
impl Provider for OpenAiProvider {
    fn name(&self) -> &str {
        &self.name
    }

    fn capabilities(&self) -> Capabilities {
        self.capabilities.clone()
    }

    fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    async fn complete(&self, request: &CompletionRequest) -> ProviderResult<Completion> {
        let response = self
            .http
            .post(self.endpoint.clone())
            .headers(self.headers.clone())
            .json(&self.body(request))
            .send()
            .await
            .map_err(|error| self.unreachable(&error))?;
        let status = response.status();
        if !status.is_success() {
            return Err(self.refused(status, response, request).await);
        }
        let reply: ChatReply = response
            .json()
            .await
            .map_err(|error| ProviderError::Malformed {
                message: format!(
                    "{} answered with something unreadable: {error}",
                    self.endpoint
                ),
            })?;
        self.completion(reply, request)
    }
}

/// A refusal is quoted on one line, so it fits inside the error it explains.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Where a redirect wanted to go, if it said. Read for the error message only:
/// nothing here follows it.
fn redirect_target(headers: &reqwest::header::HeaderMap) -> Option<String> {
    headers
        .get(reqwest::header::LOCATION)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

/// Whether a 400 is the server saying it cannot hold this prompt, which the loop
/// can answer by cutting context rather than by giving up.
fn is_context_too_long(body: &str) -> bool {
    let lower = body.to_lowercase();
    lower.contains("context") && (lower.contains("too long") || lower.contains("maximum"))
}

/// The configuration's stop list, in the form every engine and server in this
/// repo accepts.
fn normalized_stop(config: &ProviderConfig, name: &str) -> ProviderResult<Vec<String>> {
    normalize_stop_sequences(&config.stop)
        .map_err(|error| ProviderError::unconfigured(format!("providers.{name}.stop: {error}")))
}

fn trimmed(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

/// Where to post, finished off so it names the chat endpoint.
///
/// `base_url` is written three ways in the wild — `http://localhost:11434`,
/// `…/v1`, and the full endpoint — and all three have to work, because each one
/// appears in a server's own README.
///
/// A section that cannot be read as a URL is `Unconfigured`, not `Unreachable`:
/// the difference matters to the loop, because editing `base_url` is the fix and
/// repeating the request is not.
fn chat_endpoint(base_url: Option<&str>, name: &str) -> ProviderResult<Url> {
    let key = format!("providers.{name}.base_url");
    let raw = base_url.map(str::trim).unwrap_or_default();
    if raw.is_empty() {
        return Err(ProviderError::unconfigured(format!(
            "{key} is missing, and an openai_compatible provider needs a URL to ask"
        )));
    }
    let url = Url::parse(raw).map_err(|error| {
        ProviderError::unconfigured(format!(
            "{key} is not a URL a request could be made to: {error}"
        ))
    })?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(ProviderError::unconfigured(format!(
            "{key} should start with http:// or https://"
        )));
    }
    let path = url.path().trim_end_matches('/');
    // Already the endpoint: the config has done this function's job for it.
    if path.ends_with(CHAT_PATH) {
        return Ok(url);
    }
    let versioned = if path.is_empty() || path == "/" {
        "/v1".to_owned()
    } else if path.ends_with("/v1") {
        path.to_owned()
    } else {
        // A server mounted under a prefix, like `http://host:8000/gpt4all/v1`.
        format!("{path}/v1")
    };
    url.join(&format!("{versioned}/{CHAT_PATH}"))
        .map_err(|error| {
            ProviderError::unconfigured(format!(
                "{key} could not be made into a chat endpoint: {error}"
            ))
        })
}

/// The bearer header, from the variable the config names.
///
/// The value leaves this function only into the header map. A configuration file
/// is meant to be committed, and an environment variable is how the key stays out
/// of it — so an unset variable is a hard error naming the variable rather than
/// an anonymous 401 from the server.
fn bearer_headers(name: &str, config: &ProviderConfig) -> ProviderResult<HeaderMap> {
    let mut headers = HeaderMap::new();
    let Some(variable) =
        trimmed(config.api_key_env.as_deref()).or_else(|| trimmed(config.token_env.as_deref()))
    else {
        return Ok(headers);
    };
    let value = std::env::var(&variable).map_err(|_| {
        ProviderError::unconfigured(format!(
            "{variable} is not set in this environment, so there is no key to send to providers.{name}"
        ))
    })?;
    let value = value.trim();
    if value.is_empty() {
        return Err(ProviderError::unconfigured(format!(
            "{variable} is set but empty, so there is no key to send to providers.{name}"
        )));
    }
    // A key with a newline in it would let one value write a second header, and
    // no server wants a bearer that is not one token.
    let header = HeaderValue::from_str(&format!("Bearer {value}")).map_err(|_| {
        ProviderError::unconfigured(format!(
            "{variable} holds characters an HTTP header cannot carry"
        ))
    })?;
    headers.insert(AUTHORIZATION, header);
    Ok(headers)
}

/// `POST /v1/chat/completions` as the server writes it, keeping only what the
/// loop needs to read.
#[derive(Debug, Default, Deserialize)]
struct ChatReply {
    /// The first of these is the answer; `n` is never asked for.
    #[serde(default)]
    choices: Vec<Choice>,
    #[serde(default)]
    usage: Option<WireUsage>,
}

/// The wire's own spelling of a usage block.
///
/// [`TokenUsage`] is the shape this repository's config files use, which serde
/// renames to camelCase — a server writes `prompt_tokens`, so reading it straight
/// into that type would silently produce zeroes and a run that under-reports what
/// it cost.
#[derive(Debug, Default, Deserialize)]
struct WireUsage {
    #[serde(default)]
    prompt_tokens: u32,
    #[serde(default)]
    completion_tokens: u32,
}

impl WireUsage {
    /// Whether the server actually counted anything. A `{}` block is not a set of
    /// numbers to trust over the heuristic.
    fn is_real(&self) -> bool {
        self.prompt_tokens > 0 || self.completion_tokens > 0
    }

    fn into_usage(self) -> TokenUsage {
        TokenUsage::new(self.prompt_tokens, self.completion_tokens)
    }
}

#[derive(Debug, Default, Deserialize)]
struct Choice {
    #[serde(default)]
    message: Option<Message>,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct Message {
    /// A string, or `null` on a turn that is only a tool call.
    #[serde(default)]
    content: Option<Value>,
    #[serde(default)]
    tool_calls: Vec<WireCall>,
}

impl Message {
    fn text(&self) -> String {
        match &self.content {
            Some(Value::String(text)) => text.clone(),
            // `null` and a missing field both mean "this turn called instead".
            _ => String::new(),
        }
    }
}

/// One entry of `message.tool_calls`: OpenAI sends the arguments as a JSON
/// string inside a `function` object, and some servers send the object, flat.
#[derive(Debug, Deserialize)]
struct WireCall {
    #[serde(default)]
    function: Option<WireFunction>,
    /// TGI's older chat route puts the name at the top level.
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct WireFunction {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<Value>,
}

impl WireCall {
    fn name(&self) -> &str {
        self.function
            .as_ref()
            .and_then(|function| function.name.as_deref())
            .or(self.name.as_deref())
            .unwrap_or_default()
            .trim()
    }

    fn arguments(&self) -> Option<&Value> {
        self.function
            .as_ref()
            .and_then(|function| function.arguments.as_ref())
            .or(self.arguments.as_ref())
    }

    /// A call the loop can run, or nothing: an argument blob that is not a JSON
    /// object is worse to act on than a turn that asked for nothing.
    fn into_tool_call(self) -> Option<ToolCall> {
        let name = self.name().to_owned();
        let arguments = match self.arguments()? {
            Value::String(text) => serde_json::from_str::<Value>(text.trim()).ok()?,
            Value::Object(object) => Value::Object(object.clone()),
            _ => return None,
        };
        if !arguments.is_object() {
            return None;
        }
        Some(ToolCall::new(name, arguments))
    }
}

/// OpenAI's `finish_reason` in this crate's vocabulary.
///
/// A matched stop sequence and a finished turn are both `"stop"` on this wire, so
/// `StopSequence` is not something a server can tell us — the same thing the GGUF
/// adapter concludes from the engine. The loop only asks whether the answer was
/// cut off, and `"length"` answers that.
fn stop_reason(reason: Option<&str>, asked_for_tools: bool) -> StopReason {
    match reason.map(str::trim).filter(|reason| !reason.is_empty()) {
        Some("length") => StopReason::Length,
        Some("tool_calls") | Some("function_call") => {
            if asked_for_tools {
                StopReason::ToolCalls
            } else {
                StopReason::EndOfTurn
            }
        }
        _ => StopReason::EndOfTurn,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    use super::*;
    use agent_config::ProviderKind;
    use smollm_core::chat::{chat_message, Role};

    /// A server on a port of its own that answers every request with one fixed
    /// reply and keeps the raw text of each request it read.
    ///
    /// Hand-rolled rather than a transport fake on purpose: what this provider is
    /// judged on is the exact request a real Ollama or `smoll serve` would see,
    /// headers included, and that is the one thing an in-process mock cannot show.
    struct Stub {
        port: u16,
        requests: Arc<Mutex<Vec<String>>>,
    }

    impl Stub {
        async fn answering(response: impl Into<String>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0")
                .await
                .expect("a port to answer on");
            let port = listener
                .local_addr()
                .expect("the address of a bound socket")
                .port();
            let requests = Arc::new(Mutex::new(Vec::new()));
            tokio::spawn(serve(listener, Arc::clone(&requests), response.into()));
            Self { port, requests }
        }

        /// The `base_url` shape a config file would hold for this server.
        fn base_url(&self) -> String {
            format!("http://127.0.0.1:{}/v1", self.port)
        }

        fn raw(&self) -> Vec<String> {
            self.requests
                .lock()
                .expect("the stub is not shared across a panic")
                .clone()
        }

        /// The JSON bodies this client sent, oldest first.
        fn bodies(&self) -> Vec<Value> {
            self.raw()
                .iter()
                .filter_map(|text| text.split_once("\r\n\r\n"))
                .filter_map(|(_, body)| serde_json::from_str::<Value>(body).ok())
                .collect()
        }

        fn last_body(&self) -> Value {
            self.bodies().pop().expect("a request reached the stub")
        }
    }

    async fn serve(listener: TcpListener, requests: Arc<Mutex<Vec<String>>>, response: String) {
        // One connection per request is all this client needs, and the task goes
        // away with the runtime when the test ends.
        loop {
            let (mut socket, _) = match listener.accept().await {
                Ok(pair) => pair,
                Err(_) => return,
            };
            let text = read_request(&mut socket).await;
            requests
                .lock()
                .expect("the stub is not shared across a panic")
                .push(text);
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.shutdown().await;
        }
    }

    /// One HTTP/1.1 request, read through to the end of its body.
    async fn read_request(socket: &mut TcpStream) -> String {
        let mut raw: Vec<u8> = Vec::new();
        let mut buffer = [0_u8; 4096];
        let head_end = loop {
            let read = socket.read(&mut buffer).await.unwrap_or(0);
            if read == 0 {
                return String::from_utf8_lossy(&raw).into_owned();
            }
            raw.extend_from_slice(&buffer[..read]);
            if let Some(position) = raw.windows(4).position(|window| window == b"\r\n\r\n") {
                break position + 4;
            }
        };
        let head = String::from_utf8_lossy(&raw[..head_end]).to_lowercase();
        if head.contains("expect: 100-continue") {
            let _ = socket.write_all(b"HTTP/1.1 100 Continue\r\n\r\n").await;
        }
        let wanted = content_length(&head).unwrap_or(0);
        while raw.len() < head_end + wanted {
            match socket.read(&mut buffer).await {
                Ok(0) | Err(_) => break,
                Ok(read) => raw.extend_from_slice(&buffer[..read]),
            }
        }
        String::from_utf8_lossy(&raw).into_owned()
    }

    fn content_length(head: &str) -> Option<usize> {
        head.lines()
            .find_map(|line| line.strip_prefix("content-length:"))
            .and_then(|value| value.trim().parse::<usize>().ok())
    }

    /// A whole response, with the body's length stated the way a client needs it.
    fn http(status_line: &str, body: &str, extra: &[(&str, &str)]) -> String {
        let mut text = format!(
            "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
            body.len()
        );
        for (key, value) in extra {
            text.push_str(&format!("{key}: {value}\r\n"));
        }
        text.push_str("\r\n");
        text.push_str(body);
        text
    }

    fn ok(body: &str) -> String {
        http("200 OK", body, &[])
    }

    /// The smallest reply a real server sends: one choice, its text, why it ended.
    fn answered(text: &str, reason: &str) -> String {
        format!(
            r#"{{"choices":[{{"index":0,"message":{{"role":"assistant","content":{}}},"finish_reason":{}}}]}}"#,
            serde_json::to_value(text).expect("text is serialisable"),
            serde_json::to_value(reason).expect("a word is serialisable"),
        )
    }

    fn config(stub: &Stub) -> ProviderConfig {
        let mut config = ProviderConfig::new(ProviderKind::OpenaiCompatible);
        config.base_url = Some(stub.base_url());
        config.model = Some("qwen2.5:1.5b".to_owned());
        config
    }

    fn provider(stub: &Stub) -> OpenAiProvider {
        OpenAiProvider::new("served", &config(stub)).expect("a URL to ask")
    }

    fn request(text: &str) -> CompletionRequest {
        CompletionRequest::new(vec![
            chat_message(Role::System, "You read before you edit."),
            chat_message(Role::User, text),
        ])
    }

    #[tokio::test]
    async fn the_request_on_the_wire_is_the_shape_this_repo_s_server_accepts() {
        let stub = Stub::answering(ok(&answered("done", "stop"))).await;
        provider(&stub)
            .complete(&request("find the parser"))
            .await
            .expect("an answer");
        let body = stub.last_body();
        assert_eq!(body["model"], json!("qwen2.5:1.5b"), "named, not guessed");
        assert_eq!(body["messages"][0]["role"], json!("system"));
        assert_eq!(
            body["messages"][1]["content"],
            json!("find the parser"),
            "the transcript reaches the server in order"
        );
        // An option the configuration does not mention is not sent at all, so the
        // server's own default is what the run gets.
        assert!(body.get("temperature").is_none(), "{body}");
        assert!(body.get("tools").is_none(), "{body}");
    }

    #[tokio::test]
    async fn a_setting_for_this_step_outvotes_the_one_in_the_config_file() {
        let stub = Stub::answering(ok(&answered("done", "stop"))).await;
        let mut config = config(&stub);
        config.temperature = Some(0.2);
        config.max_tokens = Some(64);
        let provider = OpenAiProvider::new("served", &config).expect("a URL to ask");
        let mut asked = request("answer shortly");
        asked.max_tokens = Some(8);
        provider.complete(&asked).await.expect("an answer");
        let body = stub.last_body();
        assert_eq!(body["max_tokens"], json!(8), "the loop decides the budget");
        let temperature = body["temperature"].as_f64().expect("the config's number");
        // The config's number is an `f32`, so it arrives widened rather than
        // exact; the run cares about 0.2, not about its last places.
        assert!(
            (temperature - 0.2).abs() < 1e-6,
            "the config fills what the step left out: {body}"
        );
    }

    #[tokio::test]
    async fn a_blank_turn_in_the_transcript_does_not_reach_the_server() {
        let stub = Stub::answering(ok(&answered("done", "stop"))).await;
        let mut asked = request("keep going");
        asked.messages.push(chat_message(Role::Assistant, ""));
        asked.messages.push(chat_message(Role::User, "and then?"));
        provider(&stub).complete(&asked).await.expect("an answer");
        let body = stub.last_body();
        assert_eq!(body["messages"].as_array().expect("a list").len(), 3);
    }

    #[tokio::test]
    async fn a_reply_that_ran_out_of_budget_is_marked_as_cut_off() {
        let stub =
            Stub::answering(ok(&answered("it reads the config file and the", "length"))).await;
        let answer = provider(&stub)
            .complete(&request("explain the config"))
            .await
            .expect("an answer");
        assert!(answer.stop.is_truncated(), "{:?}", answer.stop);
    }

    #[tokio::test]
    async fn a_reply_with_no_choices_at_all_is_not_an_answer() {
        let stub = Stub::answering(ok(r#"{"choices":[]}"#)).await;
        let error = provider(&stub)
            .complete(&request("say something"))
            .await
            .expect_err("an empty list is not a completion");
        assert!(error.to_string().contains("no choices"), "{error}");
        assert!(!error.retryable(), "{error}");
    }

    #[tokio::test]
    async fn a_reply_that_is_not_json_says_so() {
        let stub = Stub::answering(ok("<html>proxy error</html>")).await;
        let error = provider(&stub)
            .complete(&request("say something"))
            .await
            .expect_err("that is not a completion");
        assert!(matches!(error, ProviderError::Malformed { .. }), "{error}");
    }

    #[test]
    fn a_base_url_written_three_ways_reaches_one_endpoint() {
        let name = "served";
        for written in [
            "http://localhost:11434",
            "http://localhost:11434/",
            "http://localhost:11434/v1",
            "http://localhost:11434/v1/",
            "http://localhost:11434/v1/chat/completions",
        ] {
            let mut config = ProviderConfig::new(ProviderKind::OpenaiCompatible);
            config.base_url = Some(written.to_owned());
            let provider = OpenAiProvider::new(name, &config).expect("a URL to ask");
            assert_eq!(
                provider.endpoint(),
                "http://localhost:11434/v1/chat/completions",
                "{written} should land on the chat endpoint"
            );
        }
        // A server mounted under a prefix keeps its prefix.
        let mut config = ProviderConfig::new(ProviderKind::OpenaiCompatible);
        config.base_url = Some("http://box:8000/gpt4all/v1".to_owned());
        assert_eq!(
            OpenAiProvider::new(name, &config)
                .expect("a URL to ask")
                .endpoint(),
            "http://box:8000/gpt4all/v1/chat/completions"
        );
    }

    #[test]
    fn a_base_url_that_cannot_be_asked_is_refused_before_anything_is_tried() {
        for written in ["", "localhost:11434", "ftp://box/model"] {
            let mut config = ProviderConfig::new(ProviderKind::OpenaiCompatible);
            config.base_url = Some(written.to_owned());
            let error = OpenAiProvider::new("served", &config)
                .expect_err("that is not a server to post to");
            let message = error.to_string();
            assert!(
                message.contains("base_url") || message.contains(written),
                "{message}"
            );
            assert!(
                matches!(error, ProviderError::Unconfigured { .. }),
                "a URL that cannot be asked is not a reply that could be re-read: {error}"
            );
            assert!(
                !error.retryable(),
                "the fix is an edit to the config, not another attempt: {error}"
            );
        }
    }

    /// A port that closed, which is what "is it even running" looks like to a
    /// client: the answer has to name the URL so the question can be checked.
    #[tokio::test]
    async fn a_server_that_is_not_there_names_the_url_it_tried() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a port to take and give back");
        let port = listener.local_addr().expect("an address").port();
        drop(listener);
        let mut config = ProviderConfig::new(ProviderKind::OpenaiCompatible);
        config.base_url = Some(format!("http://127.0.0.1:{port}/v1"));
        let provider = OpenAiProvider::new("absent", &config).expect("a URL to ask");
        let error = provider
            .complete(&request("is anyone home?"))
            .await
            .expect_err("nobody is");
        let message = error.to_string();
        assert!(message.contains(&format!("127.0.0.1:{port}")), "{message}");
        assert!(
            error.retryable(),
            "a server that starts five seconds later is worth asking again"
        );
    }

    #[tokio::test]
    async fn a_refused_key_is_not_asked_again() {
        let stub = Stub::answering(http("401 Unauthorized", r#"{"error":"bad key"}"#, &[])).await;
        let error = provider(&stub)
            .complete(&request("say something"))
            .await
            .expect_err("the server said no");
        assert!(
            matches!(error, ProviderError::Unauthorized { .. }),
            "{error}"
        );
        assert!(!error.retryable(), "{error}");
        assert!(error.to_string().contains("bad key"), "{error}");
    }

    #[tokio::test]
    async fn a_busy_server_is_asked_again_after_the_wait_it_named() {
        let stub = Stub::answering(http(
            "429 Too Many Requests",
            "slow down",
            &[("retry-after", "7")],
        ))
        .await;
        let error = provider(&stub)
            .complete(&request("say something"))
            .await
            .expect_err("the server is full");
        assert!(error.retryable(), "{error}");
        assert_eq!(error.backoff_hint(), Some(7), "{error}");
    }

    #[tokio::test]
    async fn a_server_that_crashed_is_worth_another_try() {
        let stub = Stub::answering(http("500 Internal Server Error", "kernel panic", &[])).await;
        let error = provider(&stub)
            .complete(&request("say something"))
            .await
            .expect_err("the server fell over");
        assert!(
            matches!(error, ProviderError::Unreachable { .. }),
            "{error}"
        );
        assert!(error.to_string().contains("500"), "{error}");
        assert!(error.retryable(), "{error}");
    }

    /// A tool list is only sent to a server the configuration vouched for, because
    /// `smoll serve` and TGI answer a request without ever reading one.
    #[tokio::test]
    async fn a_tool_list_is_sent_only_to_a_server_that_was_told_to_expect_one() {
        let stub = Stub::answering(ok(&answered("done", "stop"))).await;
        let mut asked = request("list the files");
        asked.tools = vec![ToolSchema::new(
            "list_files",
            "List a directory",
            json!({"type": "object", "properties": {"dir": {"type": "string"}}}),
        )];

        provider(&stub).complete(&asked).await.expect("an answer");
        assert!(
            stub.last_body().get("tools").is_none(),
            "a server nobody vouched for is not handed a list it would ignore"
        );

        let mut config = config(&stub);
        config.tool_calling = Some(true);
        let provider = OpenAiProvider::new("served", &config).expect("a URL to ask");
        assert!(provider.capabilities().tool_calling);
        provider.complete(&asked).await.expect("an answer");
        let sent = stub.last_body()["tools"].clone();
        assert_eq!(sent[0]["type"], json!("function"), "the nested shape");
        assert_eq!(sent[0]["function"]["name"], json!("list_files"));
        assert_eq!(sent[0]["function"]["parameters"]["type"], json!("object"));
    }

    /// A server that names a tool nobody offered has not asked for a thing: it has
    /// described one, and running it would be this client's mistake, not the
    /// model's.
    #[tokio::test]
    async fn a_call_for_a_tool_that_was_never_offered_is_not_run() {
        let stub = Stub::answering(ok(
            r#"{"choices":[{"message":{"role":"assistant","content":"one moment","tool_calls":[{"function":{"name":"delete_everything","arguments":"{}"}}]},"finish_reason":"tool_calls"}]}"#,
        ))
        .await;
        let answer = provider_offering_tools(&stub)
            .await
            .complete(&asked_for_list())
            .await
            .expect("an answer");
        assert!(answer.calls.is_empty(), "{:?}", answer.calls);
        assert_eq!(answer.stop, StopReason::EndOfTurn, "nothing was asked for");
        assert_eq!(answer.text, "one moment");
    }

    #[tokio::test]
    async fn a_call_for_a_tool_that_was_offered_becomes_a_call() {
        let stub = Stub::answering(ok(
            r#"{"choices":[{"message":{"role":"assistant","content":null,"tool_calls":[{"id":"one","type":"function","function":{"name":"list_files","arguments":"{\"dir\": \"src\"}"}}]},"finish_reason":"tool_calls"}]}"#,
        ))
        .await;
        let answer = provider_offering_tools(&stub)
            .await
            .complete(&asked_for_list())
            .await
            .expect("a call");
        assert_eq!(
            answer.text, "",
            "a turn that called instead of answering has no prose"
        );
        assert_eq!(answer.calls.len(), 1, "{:?}", answer.calls);
        assert_eq!(answer.calls[0].name, "list_files");
        assert_eq!(answer.calls[0].str_arg("dir"), Some("src".to_owned()));
        assert_eq!(answer.stop, StopReason::ToolCalls);
        assert!(answer.asks_for_tools());
    }

    /// Some servers send the call flat, with real JSON rather than a string of
    /// it. A run should not depend on which of the two a given build picked.
    #[tokio::test]
    async fn a_call_written_flat_instead_of_nested_is_still_read() {
        let stub = Stub::answering(ok(
            r#"{"choices":[{"message":{"role":"assistant","content":null,"tool_calls":[{"name":"list_files","arguments":{"dir":"src"}}]},"finish_reason":"tool_calls"}]}"#,
        ))
        .await;
        let answer = provider_offering_tools(&stub)
            .await
            .complete(&asked_for_list())
            .await
            .expect("an answer");
        assert_eq!(answer.calls.len(), 1, "the flat shape is a call too");
        assert_eq!(answer.calls[0].str_arg("dir"), Some("src".to_owned()));
    }

    /// Arguments that are not a JSON object are worse to act on than a turn that
    /// asked for nothing, so they go the same way an unoffered tool does.
    #[tokio::test]
    async fn a_call_whose_arguments_are_not_json_is_dropped() {
        let stub = Stub::answering(ok(
            r#"{"choices":[{"message":{"role":"assistant","content":"thinking","tool_calls":[{"function":{"name":"list_files","arguments":"not json at all"}}]},"finish_reason":"tool_calls"}]}"#,
        ))
        .await;
        let answer = provider_offering_tools(&stub)
            .await
            .complete(&asked_for_list())
            .await
            .expect("an answer");
        assert!(answer.calls.is_empty(), "{:?}", answer.calls);
        assert!(!answer.asks_for_tools());
    }

    /// The one provider in this crate that can carry a secret, and the reason a
    /// configuration file names a variable instead of a value.
    #[tokio::test]
    async fn a_key_read_from_the_environment_goes_in_a_header_and_not_into_a_prompt() {
        let variable = "SMOLL_OPENAI_TEST_SENT_KEY";
        std::env::set_var(variable, "held-by-the-environment");
        let stub = Stub::answering(ok(&answered("done", "stop"))).await;
        let mut config = config(&stub);
        config.api_key_env = Some(variable.to_owned());
        let provider = OpenAiProvider::new("served", &config).expect("a key to send");
        assert!(provider.sends_a_key());

        provider
            .complete(&request("say something"))
            .await
            .expect("an answer");
        let raw = stub.raw();
        assert_eq!(raw.len(), 1, "one request, and no follow-up");
        let (head, body) = raw[0]
            .split_once("\r\n\r\n")
            .expect("a request with a body");
        assert!(head.contains("held-by-the-environment"), "in a header");
        assert!(
            !body.contains("held-by-the-environment"),
            "never in what the model is shown: {body}"
        );
        // A run's log, an error's `Debug` and a panic hook all print the provider.
        assert!(
            !format!("{provider:?}").contains("held-by-the-environment"),
            "the provider does not print what it holds"
        );
        std::env::remove_var(variable);
    }

    #[tokio::test]
    async fn a_variable_that_is_not_set_costs_a_line_rather_than_a_run() {
        let variable = "SMOLL_OPENAI_TEST_ABSENT_KEY";
        std::env::remove_var(variable);
        let stub = Stub::answering(ok(&answered("done", "stop"))).await;
        let mut config = config(&stub);
        // The other spelling the same idea has ever had, accepted the same way.
        config.token_env = Some(variable.to_owned());
        let error = OpenAiProvider::new("served", &config).expect_err("there is no key to send");
        assert!(error.to_string().contains(variable), "{error}");
        assert!(
            stub.raw().is_empty(),
            "the server was never asked, so it has nothing to say about a key"
        );
    }

    #[tokio::test]
    async fn a_variable_that_is_set_but_empty_is_not_a_key() {
        let variable = "SMOLL_OPENAI_TEST_BLANK_KEY";
        std::env::set_var(variable, "   ");
        let stub = Stub::answering(ok(&answered("done", "stop"))).await;
        let mut config = config(&stub);
        config.api_key_env = Some(variable.to_owned());
        let error = OpenAiProvider::new("served", &config).expect_err("spaces are not a key");
        let message = error.to_string();
        assert!(message.contains(variable), "{message}");
        assert!(message.contains("empty"), "{message}");
        std::env::remove_var(variable);
    }

    /// Following a redirect would send this client's header to a host no
    /// configuration ever named, so a 3xx is an answer rather than a detour.
    #[tokio::test]
    async fn a_redirect_is_reported_and_the_key_stays_home() {
        let variable = "SMOLL_OPENAI_TEST_REDIRECT_KEY";
        std::env::set_var(variable, "not-for-another-host");
        let stub = Stub::answering(http(
            "302 Found",
            "",
            &[(
                "location",
                "http://elsewhere.example.com/v1/chat/completions",
            )],
        ))
        .await;
        let mut config = config(&stub);
        config.api_key_env = Some(variable.to_owned());
        let provider = OpenAiProvider::new("served", &config).expect("a URL to ask");
        let error = provider
            .complete(&request("say something"))
            .await
            .expect_err("the server moved");
        let message = error.to_string();
        assert!(message.contains("elsewhere.example.com"), "{message}");
        assert!(message.contains("redirect"), "{message}");
        assert!(!error.retryable(), "{message}");
        assert_eq!(stub.raw().len(), 1, "asked once, and did not follow");
        let sent = stub.raw()[0].clone();
        assert!(
            !sent.contains("elsewhere"),
            "the client never contacted the other host: {sent}"
        );
        std::env::remove_var(variable);
    }

    /// A server that counts every token as zero has not answered the question of
    /// what the step cost; the estimate is better than a run that looks free.
    #[tokio::test]
    async fn a_usage_block_of_zeroes_is_not_a_number_to_budget_a_run_by() {
        let stub = Stub::answering(ok(
            r#"{"choices":[{"message":{"role":"assistant","content":"a sentence of some length"},"finish_reason":"stop"}],"usage":{"prompt_tokens":0,"completion_tokens":0,"total_tokens":0}}"#,
        ))
        .await;
        let asked = request("say something");
        let answer = provider(&stub).complete(&asked).await.expect("an answer");
        assert_eq!(answer.usage.prompt_tokens, asked.prompt_tokens());
        assert!(answer.usage.completion_tokens > 0, "{:?}", answer.usage);
    }

    #[tokio::test]
    async fn the_stop_sequences_a_config_lists_are_sent_as_ones() {
        let stub = Stub::answering(ok(&answered("done", "stop"))).await;
        let mut config = config(&stub);
        config.stop = vec!["<|end|>".to_owned(), "  ".to_owned()];
        config.max_tokens = Some(256);
        OpenAiProvider::new("served", &config)
            .expect("a URL to ask")
            .complete(&request("say something"))
            .await
            .expect("an answer");
        let body = stub.last_body();
        assert_eq!(
            body["stop"],
            json!(["<|end|>"]),
            "a blank entry is dropped rather than sent"
        );
        assert_eq!(body["max_tokens"], json!(256));
    }

    async fn provider_offering_tools(stub: &Stub) -> OpenAiProvider {
        let mut config = config(stub);
        config.tool_calling = Some(true);
        OpenAiProvider::new("served", &config).expect("a URL to ask")
    }

    fn asked_for_list() -> CompletionRequest {
        let mut asked = request("list the files");
        asked.tools = vec![ToolSchema::new(
            "list_files",
            "List a directory",
            json!({"type": "object"}),
        )];
        asked
    }

    #[tokio::test]
    async fn a_wrong_path_says_to_check_the_base_url() {
        let stub = Stub::answering(http("404 Not Found", "no such endpoint", &[])).await;
        let error = provider(&stub)
            .complete(&request("say something"))
            .await
            .expect_err("that is not the endpoint");
        let message = error.to_string();
        assert!(message.contains("base_url"), "{message}");
    }

    #[tokio::test]
    async fn a_prompt_too_long_for_the_model_is_cut_rather_than_retried() {
        let stub = Stub::answering(http(
            "400 Bad Request",
            r#"{"error":"This model's maximum context length is 2048 tokens, but your prompt is longer"}"#,
            &[],
        ))
        .await;
        let mut config = config(&stub);
        config.context_length = Some(2048);
        config.max_tokens = Some(128);
        let provider = OpenAiProvider::new("served", &config).expect("a URL to ask");
        let error = provider
            .complete(&request("and again, the whole repository please"))
            .await
            .expect_err("it will not fit");
        match error {
            ProviderError::ContextTooLong { wanted, available } => {
                assert_eq!(available, 2048);
                assert!(wanted > 0, "{wanted}");
            }
            other => panic!("the loop needs to cut context, not retry: {other}"),
        }
    }
}
