//! Inference backend abstraction.
//!
//! The agent loop only needs to send a turn (optionally with tools) and return
//! tool results. This module defines that seam as the `InferenceBackend` trait
//! plus a few neutral types, with two implementations:
//!
//! - `LocalBackend` runs on-device through the `onde` crate (`ChatEngine`).
//! - `OpenAiBackend` talks to any OpenAI-compatible HTTP endpoint, configured by
//!   `base_url`, `api_key`, and `model`.
//!
//! The trait exposes neither `onde` nor OpenAI types, so the loop does not depend
//! on a specific backend.
//!
//! The whole backend seam is wired up only through the interactive client, which
//! is `#[cfg(unix)]` (see `run_interactive` in `main.rs` and `mod tui` in
//! `chat.rs`). On non-Unix targets the binary runs ACP-only and drives `onde`
//! directly, so every item here is legitimately unused there. Suppress the
//! dead-code lint on those targets only — Unix builds still get full coverage.
#![cfg_attr(not(unix), allow(dead_code))]

use std::sync::Arc;

use async_trait::async_trait;
use onde::inference::{ChatEngine, ToolDefinition};
use serde::Deserialize;
use tokio::sync::Mutex;

// ── Neutral types ───────────────────────────────────────────────────────────────

/// A tool the model may call, in a provider-neutral form. `parameters_schema` is
/// a JSON Schema encoded as a string (matching how siGit already declares tools).
#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters_schema: String,
}

/// A tool call requested by the model.
#[derive(Debug, Clone)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// Arguments as a JSON-encoded string.
    pub arguments: String,
}

/// The output of executing one tool call, fed back to the model.
#[derive(Debug, Clone)]
pub struct ToolResult {
    pub tool_call_id: String,
    pub content: String,
}

/// The result of one assistant turn: free text and/or tool calls.
#[derive(Debug, Clone, Default)]
pub struct TurnResult {
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
}

/// Backend errors are plain strings. Callers map them to ACP errors.
pub type BackendError = String;

/// A sink for streaming assistant text deltas to the UI as they are produced.
///
/// When a caller passes `Some(sink)`, a streaming-capable backend forwards each
/// text fragment through it as the model emits it; the returned [`TurnResult`]
/// still carries the fully assembled text (and any tool calls). When the sink is
/// `None`, the backend runs in non-streaming mode. Unbounded so the inference
/// task never blocks on a slow consumer.
pub type TokenSink = tokio::sync::mpsc::UnboundedSender<String>;

// ── The trait ───────────────────────────────────────────────────────────────────

/// A swappable inference backend driving siGit Code's agent loop.
#[async_trait]
pub trait InferenceBackend: Send + Sync {
    /// Start an assistant turn from a new user message, offering `tools`.
    ///
    /// If `sink` is `Some`, text is streamed through it as it is generated. A
    /// backend may decline to stream a given round (for example, on-device
    /// inference cannot stream while it is still deciding whether to call a
    /// tool); in that case the text is delivered only via the returned result.
    async fn send_message_with_tools(
        &self,
        text: &str,
        tools: &[ToolSpec],
        sink: Option<&TokenSink>,
    ) -> Result<TurnResult, BackendError>;

    /// Continue the turn by returning tool results. `tools` may be `None` on the
    /// final round to force a text answer. `sink` streams that text when set.
    async fn send_tool_results(
        &self,
        results: Vec<ToolResult>,
        tools: Option<&[ToolSpec]>,
        sink: Option<&TokenSink>,
    ) -> Result<TurnResult, BackendError>;

    /// Whether inference runs over the network (a configured provider) rather
    /// than on-device. Drives UI labelling so the displayed model can't claim a
    /// local model while requests actually go to the cloud.
    fn is_remote(&self) -> bool;
}

// ── Local backend (onde ChatEngine) ──────────────────────────────────────────────

/// On-device inference. A thin adapter over `onde::ChatEngine`.
pub struct LocalBackend {
    engine: Arc<ChatEngine>,
}

impl LocalBackend {
    pub fn new(engine: Arc<ChatEngine>) -> Self {
        Self { engine }
    }
}

fn to_onde_tools(tools: &[ToolSpec]) -> Vec<ToolDefinition> {
    tools
        .iter()
        .map(|tool| ToolDefinition {
            name: tool.name.clone(),
            description: tool.description.clone(),
            parameters_schema: tool.parameters_schema.clone(),
        })
        .collect()
}

#[async_trait]
impl InferenceBackend for LocalBackend {
    async fn send_message_with_tools(
        &self,
        text: &str,
        tools: &[ToolSpec],
        sink: Option<&TokenSink>,
    ) -> Result<TurnResult, BackendError> {
        // onde's tool-aware path is non-streaming: it has to buffer the whole
        // reply to detect tool calls. We can only stream when no tools are on
        // offer (a plain answer), which is exactly the tools-disabled case.
        if let Some(sink) = sink
            && tools.is_empty()
        {
            let rx = self
                .engine
                .stream_message(text)
                .await
                .map_err(|error| error.to_string())?;
            return drain_onde_stream(rx, sink).await;
        }

        let onde_tools = to_onde_tools(tools);
        let result = self
            .engine
            .send_message_with_tools(text, &onde_tools)
            .await
            .map_err(|error| error.to_string())?;
        Ok(onde_result_to_turn(result))
    }

    async fn send_tool_results(
        &self,
        results: Vec<ToolResult>,
        tools: Option<&[ToolSpec]>,
        sink: Option<&TokenSink>,
    ) -> Result<TurnResult, BackendError> {
        let onde_results: Vec<onde::inference::ToolResult> = results
            .into_iter()
            .map(|result| onde::inference::ToolResult {
                tool_call_id: result.tool_call_id,
                content: result.content,
            })
            .collect();

        // The final round passes `tools = None` to force a text answer; that's
        // the only round onde can stream, since no further tool calls are parsed.
        if let Some(sink) = sink
            && tools.is_none()
        {
            let rx = self
                .engine
                .stream_tool_results(onde_results, None)
                .await
                .map_err(|error| error.to_string())?;
            return drain_onde_stream(rx, sink).await;
        }

        let onde_tools = tools.map(to_onde_tools);
        let result = self
            .engine
            .send_tool_results(onde_results, onde_tools.as_deref())
            .await
            .map_err(|error| error.to_string())?;
        Ok(onde_result_to_turn(result))
    }

    fn is_remote(&self) -> bool {
        false
    }
}

/// Drain an onde streaming receiver, forwarding each token to `sink` and
/// assembling the full text. onde reports stream failures as a final chunk whose
/// `finish_reason` is `"error: …"`; surface those as a backend error.
async fn drain_onde_stream(
    mut rx: tokio::sync::mpsc::Receiver<onde::inference::StreamChunk>,
    sink: &TokenSink,
) -> Result<TurnResult, BackendError> {
    let mut text = String::new();
    while let Some(chunk) = rx.recv().await {
        if !chunk.delta.is_empty() {
            text.push_str(&chunk.delta);
            // The receiver is the UI; if it's gone the turn is being cancelled,
            // so stop assembling rather than spinning the model to completion.
            if sink.send(chunk.delta).is_err() {
                break;
            }
        }
        if chunk.done {
            if let Some(reason) = chunk.finish_reason
                && let Some(message) = reason.strip_prefix("error: ")
            {
                return Err(message.to_string());
            }
            break;
        }
    }
    Ok(TurnResult {
        text,
        tool_calls: Vec::new(),
    })
}

/// Convert an `onde` tool-aware result into the neutral [`TurnResult`].
fn onde_result_to_turn(result: onde::inference::ToolAwareResult) -> TurnResult {
    TurnResult {
        text: result.text,
        tool_calls: result
            .tool_calls
            .into_iter()
            .map(|call| ToolCall {
                id: call.id,
                name: call.function_name,
                arguments: call.arguments,
            })
            .collect(),
    }
}

// ── OpenAI-compatible backend ─────────────────────────────────────────────────────

/// Inference against any OpenAI-compatible Chat Completions endpoint.
///
/// Conversation state is held client-side and replayed on every request, so the
/// endpoint can be stateless. Standard OpenAI function-calling is used end to
/// end (`tools`, `choices[].message.tool_calls`, `role: "tool"` follow-ups).
pub struct OpenAiBackend {
    base_url: String,
    api_key: String,
    model: String,
    http: reqwest::Client,
    /// The full message list sent on each request (system + turns + tool results).
    history: Mutex<Vec<serde_json::Value>>,
}

impl OpenAiBackend {
    /// Build a backend for `{base_url, api_key, model}`, seeding the optional
    /// system prompt. `base_url` should include the API root (e.g. ending in
    /// `/v1`); the chat path is appended.
    pub fn new(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
        system_prompt: Option<String>,
    ) -> Self {
        let mut history = Vec::new();
        if let Some(prompt) = system_prompt {
            history.push(serde_json::json!({ "role": "system", "content": prompt }));
        }
        Self {
            base_url: base_url.into(),
            api_key: api_key.into(),
            model: model.into(),
            http: reqwest::Client::new(),
            history: Mutex::new(history),
        }
    }

    fn tools_json(tools: &[ToolSpec]) -> Vec<serde_json::Value> {
        tools
            .iter()
            .map(|tool| {
                // parameters_schema is a JSON string; parse it, defaulting to an
                // empty object schema if malformed.
                let parameters: serde_json::Value = serde_json::from_str(&tool.parameters_schema)
                    .unwrap_or_else(|_| serde_json::json!({ "type": "object", "properties": {} }));
                serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": tool.name,
                        "description": tool.description,
                        "parameters": parameters,
                    }
                })
            })
            .collect()
    }

    /// POST the current history (plus `tools`) and apply the assistant reply to
    /// history, returning the neutral turn result. Streams via SSE when `sink`
    /// is set; otherwise reads a single JSON response.
    async fn complete(
        &self,
        tools: Option<&[ToolSpec]>,
        sink: Option<&TokenSink>,
    ) -> Result<TurnResult, BackendError> {
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let streaming = sink.is_some();

        let mut body = serde_json::json!({
            "model": self.model,
            "messages": *self.history.lock().await,
            "stream": streaming,
        });
        if let Some(tools) = tools
            && !tools.is_empty()
        {
            body["tools"] = serde_json::Value::Array(Self::tools_json(tools));
        }

        let response = self
            .http
            .post(&url)
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|error| format!("request to {url} failed: {error}"))?;

        if !response.status().is_success() {
            let status = response.status();
            let detail = response.text().await.unwrap_or_default();
            return Err(format!("endpoint returned {status}: {detail}"));
        }

        if let Some(sink) = sink {
            self.consume_stream(response, sink).await
        } else {
            self.consume_json(response).await
        }
    }

    /// Parse a single non-streaming chat-completion response.
    async fn consume_json(&self, response: reqwest::Response) -> Result<TurnResult, BackendError> {
        let parsed: ChatCompletion = response
            .json()
            .await
            .map_err(|error| format!("response parse error: {error}"))?;

        let message = parsed
            .choices
            .into_iter()
            .next()
            .map(|choice| choice.message)
            .ok_or_else(|| "endpoint returned no choices".to_string())?;

        let text = message.content.clone().unwrap_or_default();
        let tool_calls: Vec<ToolCall> = message
            .tool_calls
            .iter()
            .flatten()
            .map(|call| ToolCall {
                id: call.id.clone(),
                name: call.function.name.clone(),
                arguments: call.function.arguments.clone(),
            })
            .collect();

        // Record the assistant turn so later tool results have context.
        self.history.lock().await.push(message.into_history_value());

        Ok(TurnResult { text, tool_calls })
    }

    /// Consume an OpenAI Server-Sent Events stream, forwarding content deltas to
    /// `sink` and reassembling any tool calls (which arrive fragmented across
    /// chunks, keyed by `index`).
    async fn consume_stream(
        &self,
        response: reqwest::Response,
        sink: &TokenSink,
    ) -> Result<TurnResult, BackendError> {
        use futures::StreamExt;

        let mut stream = response.bytes_stream();
        // Newlines are ASCII, so splitting raw bytes on `\n` never bisects a
        // multibyte UTF-8 sequence; we only lossily decode whole lines.
        let mut buffer: Vec<u8> = Vec::new();
        let mut text = String::new();
        let mut tool_accum: Vec<StreamingToolCall> = Vec::new();
        let mut done = false;

        while let Some(item) = stream.next().await {
            let bytes = item.map_err(|error| format!("stream read error: {error}"))?;
            buffer.extend_from_slice(&bytes);

            while let Some(pos) = buffer.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = buffer.drain(..=pos).collect();
                let line = String::from_utf8_lossy(&line);
                let line = line.trim();

                let Some(data) = line.strip_prefix("data:") else {
                    continue;
                };
                let data = data.trim();
                if data == "[DONE]" {
                    done = true;
                    break;
                }
                if data.is_empty() {
                    continue;
                }

                let chunk: StreamCompletion = match serde_json::from_str(data) {
                    Ok(chunk) => chunk,
                    // Skip keep-alive comments and anything we can't parse rather
                    // than aborting a turn over one malformed frame.
                    Err(_) => continue,
                };

                let Some(choice) = chunk.choices.into_iter().next() else {
                    continue;
                };
                if let Some(content) = choice.delta.content
                    && !content.is_empty()
                {
                    text.push_str(&content);
                    if sink.send(content).is_err() {
                        // Consumer dropped (turn cancelled) — stop reading.
                        done = true;
                        break;
                    }
                }
                for delta in choice.delta.tool_calls.into_iter().flatten() {
                    let index = delta.index.unwrap_or(0) as usize;
                    if tool_accum.len() <= index {
                        tool_accum.resize_with(index + 1, StreamingToolCall::default);
                    }
                    let slot = &mut tool_accum[index];
                    if let Some(id) = delta.id {
                        slot.id = id;
                    }
                    if let Some(function) = delta.function {
                        if let Some(name) = function.name {
                            slot.name = name;
                        }
                        if let Some(arguments) = function.arguments {
                            slot.arguments.push_str(&arguments);
                        }
                    }
                }
            }

            if done {
                break;
            }
        }

        let tool_calls: Vec<ToolCall> = tool_accum
            .iter()
            .filter(|call| !call.name.is_empty())
            .enumerate()
            .map(|(index, call)| ToolCall {
                id: if call.id.is_empty() {
                    format!("call_{index}")
                } else {
                    call.id.clone()
                },
                name: call.name.clone(),
                arguments: call.arguments.clone(),
            })
            .collect();

        // Record the assistant turn so later tool results have context.
        self.history
            .lock()
            .await
            .push(streamed_assistant_history(&text, &tool_calls));

        Ok(TurnResult { text, tool_calls })
    }
}

/// One tool call being reassembled from streamed deltas.
#[derive(Default)]
struct StreamingToolCall {
    id: String,
    name: String,
    arguments: String,
}

/// Rebuild the assistant message for replay in history after a streamed turn,
/// preserving any tool calls so the follow-up request is well-formed. Mirrors
/// [`ResponseMessage::into_history_value`] for the non-streaming path.
fn streamed_assistant_history(text: &str, tool_calls: &[ToolCall]) -> serde_json::Value {
    let mut message = serde_json::json!({ "role": "assistant" });
    message["content"] = if text.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::Value::String(text.to_string())
    };
    if !tool_calls.is_empty() {
        message["tool_calls"] = serde_json::json!(
            tool_calls
                .iter()
                .map(|call| serde_json::json!({
                    "id": call.id,
                    "type": "function",
                    "function": {
                        "name": call.name,
                        "arguments": call.arguments,
                    }
                }))
                .collect::<Vec<_>>()
        );
    }
    message
}

#[async_trait]
impl InferenceBackend for OpenAiBackend {
    async fn send_message_with_tools(
        &self,
        text: &str,
        tools: &[ToolSpec],
        sink: Option<&TokenSink>,
    ) -> Result<TurnResult, BackendError> {
        self.history
            .lock()
            .await
            .push(serde_json::json!({ "role": "user", "content": text }));
        self.complete(Some(tools), sink).await
    }

    async fn send_tool_results(
        &self,
        results: Vec<ToolResult>,
        tools: Option<&[ToolSpec]>,
        sink: Option<&TokenSink>,
    ) -> Result<TurnResult, BackendError> {
        {
            let mut history = self.history.lock().await;
            for result in results {
                history.push(serde_json::json!({
                    "role": "tool",
                    "tool_call_id": result.tool_call_id,
                    "content": result.content,
                }));
            }
        }
        self.complete(tools, sink).await
    }

    fn is_remote(&self) -> bool {
        true
    }
}

// ── OpenAI response shapes ────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct ChatCompletion {
    #[serde(default)]
    choices: Vec<CompletionChoice>,
}

#[derive(Debug, Deserialize)]
struct CompletionChoice {
    message: ResponseMessage,
}

#[derive(Debug, Deserialize)]
struct ResponseMessage {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<ResponseToolCall>>,
}

impl ResponseMessage {
    /// Reconstruct the assistant message for replay in history, preserving any
    /// tool calls so the follow-up request is well-formed.
    fn into_history_value(self) -> serde_json::Value {
        let mut message = serde_json::json!({ "role": "assistant" });
        message["content"] = match self.content {
            Some(text) => serde_json::Value::String(text),
            None => serde_json::Value::Null,
        };
        if let Some(tool_calls) = self.tool_calls {
            message["tool_calls"] = serde_json::json!(
                tool_calls
                    .into_iter()
                    .map(|call| serde_json::json!({
                        "id": call.id,
                        "type": "function",
                        "function": {
                            "name": call.function.name,
                            "arguments": call.function.arguments,
                        }
                    }))
                    .collect::<Vec<_>>()
            );
        }
        message
    }
}

#[derive(Debug, Deserialize)]
struct ResponseToolCall {
    id: String,
    function: ResponseFunction,
}

#[derive(Debug, Deserialize)]
struct ResponseFunction {
    name: String,
    #[serde(default)]
    arguments: String,
}

// ── OpenAI streaming (SSE) chunk shapes ─────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct StreamCompletion {
    #[serde(default)]
    choices: Vec<StreamChoice>,
}

#[derive(Debug, Deserialize)]
struct StreamChoice {
    #[serde(default)]
    delta: StreamDelta,
}

#[derive(Debug, Default, Deserialize)]
struct StreamDelta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<StreamToolCallDelta>>,
}

#[derive(Debug, Deserialize)]
struct StreamToolCallDelta {
    #[serde(default)]
    index: Option<u32>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<StreamFunctionDelta>,
}

#[derive(Debug, Deserialize)]
struct StreamFunctionDelta {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tools_json_wraps_function_schema() {
        let tools = vec![ToolSpec {
            name: "read_file".to_string(),
            description: "Read a file".to_string(),
            parameters_schema: r#"{"type":"object","properties":{"path":{"type":"string"}}}"#
                .to_string(),
        }];
        let json = OpenAiBackend::tools_json(&tools);
        assert_eq!(json[0]["type"], "function");
        assert_eq!(json[0]["function"]["name"], "read_file");
        assert_eq!(
            json[0]["function"]["parameters"]["properties"]["path"]["type"],
            "string"
        );
    }

    #[test]
    fn malformed_schema_falls_back_to_empty_object() {
        let tools = vec![ToolSpec {
            name: "x".to_string(),
            description: String::new(),
            parameters_schema: "not json".to_string(),
        }];
        let json = OpenAiBackend::tools_json(&tools);
        assert_eq!(json[0]["function"]["parameters"]["type"], "object");
    }

    #[test]
    fn streamed_assistant_history_omits_empty_tool_calls() {
        let value = streamed_assistant_history("hello", &[]);
        assert_eq!(value["role"], "assistant");
        assert_eq!(value["content"], "hello");
        assert!(value.get("tool_calls").is_none());
    }

    #[test]
    fn streamed_assistant_history_preserves_tool_calls() {
        let calls = vec![ToolCall {
            id: "call_0".to_string(),
            name: "read_file".to_string(),
            arguments: r#"{"path":"a.rs"}"#.to_string(),
        }];
        let value = streamed_assistant_history("", &calls);
        assert!(value["content"].is_null());
        assert_eq!(value["tool_calls"][0]["id"], "call_0");
        assert_eq!(value["tool_calls"][0]["type"], "function");
        assert_eq!(value["tool_calls"][0]["function"]["name"], "read_file");
        assert_eq!(
            value["tool_calls"][0]["function"]["arguments"],
            r#"{"path":"a.rs"}"#
        );
    }

    #[test]
    fn assistant_message_with_tool_calls_round_trips() {
        let message = ResponseMessage {
            content: None,
            tool_calls: Some(vec![ResponseToolCall {
                id: "call_1".to_string(),
                function: ResponseFunction {
                    name: "read_file".to_string(),
                    arguments: r#"{"path":"a.rs"}"#.to_string(),
                },
            }]),
        };
        let value = message.into_history_value();
        assert_eq!(value["role"], "assistant");
        assert!(value["content"].is_null());
        assert_eq!(value["tool_calls"][0]["id"], "call_1");
        assert_eq!(value["tool_calls"][0]["type"], "function");
        assert_eq!(value["tool_calls"][0]["function"]["name"], "read_file");
    }
}
