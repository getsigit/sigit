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
//! The seam is consumed by both surfaces: the interactive client (`#[cfg(unix)]`,
//! see `run_interactive` in `main.rs` and `mod tui` in `chat.rs`) and the ACP
//! server's prompt loop. Some items are still reached only through the
//! Unix-only interactive paths, so the dead-code lint stays suppressed on
//! non-Unix targets only — Unix builds keep full coverage.
#![cfg_attr(not(unix), allow(dead_code))]

use std::sync::Arc;

use async_trait::async_trait;
use onde::inference::{ChatEngine, ChatMessage, ChatRole, ToolDefinition};
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

/// Rough context budget for a conversation, in estimated tokens (see
/// [`estimate_tokens`]). When a snapshot exceeds this, the agent loops compact
/// history before the next tool round.
pub const DEFAULT_CONTEXT_TOKEN_BUDGET: usize = 24_000;

/// How many trailing messages survive a compaction verbatim (the rest are
/// folded into the summary).
pub const COMPACT_KEEP_LAST: usize = 6;

/// The summarization request sent to the model when compacting history.
const SUMMARIZE_PROMPT: &str = "Summarize this coding session so far: decisions made, \
    files touched, current state, open items. Be concise and factual.";

/// Crude token estimate for a history snapshot: serialized characters / 4.
/// Deliberately model-agnostic — it only needs to be in the right ballpark to
/// decide when compaction is worth an extra inference round.
pub fn estimate_tokens(history: &[serde_json::Value]) -> usize {
    let chars: usize = history
        .iter()
        .map(|message| message.to_string().chars().count())
        .sum();
    chars / 4
}

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

    /// Record tool results in the conversation history *without* asking the
    /// model to continue the turn. Used when a turn is abandoned mid-round
    /// (the user cancelled at the permission gate): by then the assistant
    /// message carrying the tool calls is already in history, and leaving them
    /// unanswered makes strict OpenAI-compatible endpoints reject every later
    /// request in the session.
    async fn record_cancelled_tool_results(&self, results: Vec<ToolResult>);

    /// Whether inference runs over the network (a configured provider) rather
    /// than on-device. Drives UI labelling so the displayed model can't claim a
    /// local model while requests actually go to the cloud.
    fn is_remote(&self) -> bool;

    /// A serializable snapshot of the conversation history, one JSON object per
    /// message (`{"role": ..., "content": ...}` at minimum). The snapshot is
    /// what the session store persists; it includes any seeded system message
    /// so [`InferenceBackend::restore_history`] can replace state wholesale.
    async fn history_snapshot(&self) -> Vec<serde_json::Value>;

    /// Replace the conversation history with a previously saved snapshot.
    /// Backends that cannot represent every entry (e.g. on-device history has
    /// no tool-call structure) flatten what they can and drop the rest.
    async fn restore_history(&self, history: Vec<serde_json::Value>);

    /// Shrink the conversation history: summarize everything so far with one
    /// extra (non-streaming) inference round, then rebuild history as
    /// `[system message, summary, last keep_last non-system messages]`. On
    /// error the original history is left in place.
    async fn compact_history(&self, keep_last: usize) -> Result<(), BackendError>;
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

    async fn record_cancelled_tool_results(&self, _results: Vec<ToolResult>) {
        // onde's public API cannot append tool-result history entries without
        // running another inference round, so the dangling tool call stays in
        // its history. The chat template replays it as-is, which local models
        // tolerate — worst case the model re-issues the call next turn.
    }

    fn is_remote(&self) -> bool {
        false
    }

    async fn history_snapshot(&self) -> Vec<serde_json::Value> {
        // onde's `history()` already flattens tool entries: assistant tool
        // calls become plain assistant text and tool results are omitted, so
        // the snapshot is lossy for tool-heavy turns (acceptable in this MVP).
        self.engine
            .history()
            .await
            .iter()
            .map(|message| {
                serde_json::json!({
                    "role": message.role.to_string(),
                    "content": message.content,
                })
            })
            .collect()
    }

    async fn restore_history(&self, history: Vec<serde_json::Value>) {
        self.engine.clear_history().await;
        for entry in history {
            let role = entry["role"].as_str().unwrap_or("");
            let content = entry["content"].as_str().unwrap_or("").to_string();
            // Tool-call-only assistant entries and empty tool results carry no
            // text a plain chat history can replay; drop them.
            if content.is_empty() && role != "user" && role != "system" {
                continue;
            }
            let message = match role {
                "system" => ChatMessage::system(content),
                "user" => ChatMessage::user(content),
                "assistant" => ChatMessage::assistant(content),
                // Tool results flatten to plain text (MVP; acceptable loss).
                "tool" => ChatMessage::user(format!("[tool result]\n{content}")),
                _ => continue,
            };
            self.engine.push_history(message).await;
        }
    }

    async fn compact_history(&self, keep_last: usize) -> Result<(), BackendError> {
        let snapshot = self.engine.history().await;
        // One plain (tool-free) inference round produces the summary. On error
        // history is untouched — send_message only mutates it on success, and
        // whatever it appended is wiped by the clear below anyway.
        let result = self
            .engine
            .send_message(SUMMARIZE_PROMPT)
            .await
            .map_err(|error| error.to_string())?;
        // Local models may reason in <think> blocks; keep only the visible part.
        let (_think, summary) = crate::chat::strip_think_blocks(&result.text);

        self.engine.clear_history().await;
        // Leading system messages carry the session context; keep them all.
        for message in snapshot
            .iter()
            .take_while(|message| message.role == ChatRole::System)
        {
            self.engine.push_history(message.clone()).await;
        }
        self.engine
            .push_history(ChatMessage::user(format!(
                "[Conversation summary]\n{summary}"
            )))
            .await;
        let non_system: Vec<&ChatMessage> = snapshot
            .iter()
            .filter(|message| message.role != ChatRole::System)
            .collect();
        let tail_start = non_system.len().saturating_sub(keep_last);
        for message in &non_system[tail_start..] {
            self.engine.push_history((*message).clone()).await;
        }
        Ok(())
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
            // OpenAI specifies `auto` as the default when tools are present,
            // but not every OpenAI-compatible gateway implements that default.
            // Sending it explicitly is important for the cloud path: otherwise
            // a model can describe the action it intends to take as text and
            // end the turn without returning a tool call at all.
            body["tool_choice"] = serde_json::Value::String("auto".to_string());
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
            let body = response.text().await.unwrap_or_default();
            return Err(describe_api_error(status, &body));
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

                // An endpoint that fails after the status line is already sent
                // reports it in the stream instead, as an ordinary `data:`
                // frame holding an error envelope. siGit Code Cloud does this
                // when the upstream fails mid-turn. It has no `choices`, so
                // without this it parses as an empty chunk and is skipped, and
                // the turn ends looking like the model simply said nothing.
                if let Some(message) = api_error_message(data) {
                    return Err(message);
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

    async fn record_cancelled_tool_results(&self, results: Vec<ToolResult>) {
        let mut history = self.history.lock().await;
        for result in results {
            history.push(serde_json::json!({
                "role": "tool",
                "tool_call_id": result.tool_call_id,
                "content": result.content,
            }));
        }
    }

    fn is_remote(&self) -> bool {
        true
    }

    async fn history_snapshot(&self) -> Vec<serde_json::Value> {
        self.history.lock().await.clone()
    }

    async fn restore_history(&self, history: Vec<serde_json::Value>) {
        // The snapshot includes the seeded system message, so a wholesale
        // replacement restores exactly what was saved.
        *self.history.lock().await = history;
    }

    async fn compact_history(&self, keep_last: usize) -> Result<(), BackendError> {
        let snapshot: Vec<serde_json::Value> = self.history.lock().await.clone();

        // Ask the endpoint for a summary of the conversation so far, through
        // the ordinary completion machinery (non-streaming).
        self.history
            .lock()
            .await
            .push(serde_json::json!({ "role": "user", "content": SUMMARIZE_PROMPT }));
        let summary = match self.complete(None, None).await {
            Ok(result) => result.text,
            Err(error) => {
                // Roll back the summarization request; the turn never happened.
                *self.history.lock().await = snapshot;
                return Err(error);
            }
        };

        let system = snapshot
            .first()
            .filter(|message| message["role"] == "system")
            .cloned();
        let non_system: Vec<serde_json::Value> = snapshot
            .iter()
            .filter(|message| message["role"] != "system")
            .cloned()
            .collect();
        let tail_start = non_system.len().saturating_sub(keep_last);
        let mut tail = non_system[tail_start..].to_vec();
        // Drop leading tool results whose assistant tool-call message was
        // summarized away — strict endpoints reject orphaned `role: "tool"`
        // entries on the very next request.
        while tail
            .first()
            .is_some_and(|message| message["role"] == "tool")
        {
            tail.remove(0);
        }

        let mut rebuilt = Vec::new();
        if let Some(system) = system {
            rebuilt.push(system);
        }
        rebuilt.push(serde_json::json!({
            "role": "user",
            "content": format!("[Conversation summary]\n{summary}"),
        }));
        rebuilt.extend(tail);
        *self.history.lock().await = rebuilt;
        Ok(())
    }
}

// ── OpenAI error shape ────────────────────────────────────────────────────────

/// The OpenAI error envelope: `{"error": {"message": ..., "type": ...}}`.
///
/// Every endpoint siGit talks to speaks it, including siGit Code Cloud, whose
/// messages are written for the person reading them ("Monthly siGit Code Cloud
/// allowance reached..."). Worth unwrapping rather than pasting the raw body
/// into the editor's error banner.
#[derive(Debug, Deserialize)]
struct ApiErrorEnvelope {
    error: ApiErrorBody,
}

#[derive(Debug, Deserialize)]
struct ApiErrorBody {
    #[serde(default)]
    message: Option<String>,
}

/// How much of an unparseable error body to keep. An endpoint behind a proxy
/// can answer with a full HTML page, and the whole thing ends up in the
/// editor's error banner.
const ERROR_BODY_LIMIT: usize = 500;

/// Turn an error response into something worth showing a person.
///
/// The endpoint's own message wins when there is one: it is written for the
/// user, and the status code repeats what it already says. Anything else falls
/// back to the status plus whatever the body held, which is all there is to go
/// on.
fn describe_api_error(status: reqwest::StatusCode, body: &str) -> String {
    if let Some(message) = api_error_message(body) {
        return message;
    }

    let body = body.trim();
    if body.is_empty() {
        return format!("endpoint returned {status}");
    }

    let mut detail = body;
    if detail.len() > ERROR_BODY_LIMIT {
        let mut cut = ERROR_BODY_LIMIT;
        while !detail.is_char_boundary(cut) {
            cut -= 1;
        }
        detail = &detail[..cut];
        return format!("endpoint returned {status}: {detail}…");
    }
    format!("endpoint returned {status}: {detail}")
}

/// The human-readable message out of an OpenAI error envelope, if the body is
/// one and carries a non-empty message.
fn api_error_message(body: &str) -> Option<String> {
    let envelope: ApiErrorEnvelope = serde_json::from_str(body).ok()?;
    let message = envelope.error.message?;
    let message = message.trim();
    (!message.is_empty()).then(|| message.to_string())
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

// ── Carrying a conversation across a model switch ────────────────────────────

/// The part of a history snapshot that survives a model switch: every
/// non-system message, with half-finished tool plumbing repaired.
///
/// System messages are dropped because the backend being switched *to* seeds
/// its own — a different model's prompt, or a freshly pushed session-context
/// message. A switch can land mid-turn, after the assistant asked for a tool
/// but before the results came back; a tool call with no matching result (or a
/// result with no matching call) makes strict OpenAI-compatible endpoints
/// reject every later request in the session, so those halves are stripped
/// instead of carried.
pub fn carryover_history(snapshot: Vec<serde_json::Value>) -> Vec<serde_json::Value> {
    let answered: std::collections::HashSet<String> = snapshot
        .iter()
        .filter(|message| message["role"] == "tool")
        .filter_map(|message| message["tool_call_id"].as_str().map(str::to_string))
        .collect();

    let mut carried: Vec<serde_json::Value> = Vec::with_capacity(snapshot.len());
    let mut called: std::collections::HashSet<String> = std::collections::HashSet::new();

    for mut message in snapshot {
        match message["role"].as_str().unwrap_or_default() {
            "system" => continue,
            "assistant" => {
                if let Some(calls) = message["tool_calls"].as_array() {
                    let kept: Vec<serde_json::Value> = calls
                        .iter()
                        .filter(|call| call["id"].as_str().is_some_and(|id| answered.contains(id)))
                        .cloned()
                        .collect();
                    if kept.is_empty() {
                        // Only unanswered requests: keep whatever text came
                        // with them, drop the message if there was none.
                        if let Some(object) = message.as_object_mut() {
                            object.remove("tool_calls");
                        }
                        if message["content"].as_str().unwrap_or_default().is_empty() {
                            continue;
                        }
                    } else {
                        for call in &kept {
                            called.insert(call["id"].as_str().unwrap_or_default().to_string());
                        }
                        message["tool_calls"] = serde_json::Value::Array(kept);
                    }
                }
                carried.push(message);
            }
            "tool" => {
                let answers_a_kept_call = message["tool_call_id"]
                    .as_str()
                    .is_some_and(|id| called.contains(id));
                if answers_a_kept_call {
                    carried.push(message);
                }
            }
            _ => carried.push(message),
        }
    }

    carried
}

/// Replay `carried` (from [`carryover_history`]) into `backend`, on top of the
/// system messages `backend` seeded for itself. Used when a model switch
/// installs a new backend — or reloads the on-device engine, which wipes its
/// history — so the thread continues under the new model instead of restarting.
pub async fn adopt_carryover(backend: &dyn InferenceBackend, carried: Vec<serde_json::Value>) {
    if carried.is_empty() {
        return;
    }
    let mut rebuilt: Vec<serde_json::Value> = backend
        .history_snapshot()
        .await
        .into_iter()
        .take_while(|message| message["role"] == "system")
        .collect();
    rebuilt.extend(carried);
    backend.restore_history(rebuilt).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The case from the issue: the endpoint says something a person can act
    /// on, and the status code and JSON around it are noise.
    #[test]
    fn an_endpoints_own_message_is_what_the_user_sees() {
        let body = r#"{"error":{"message":"Monthly siGit Code Cloud allowance reached. It resets at the start of your next billing period.","type":"server_error"}}"#;

        let described = describe_api_error(reqwest::StatusCode::TOO_MANY_REQUESTS, body);

        assert_eq!(
            described,
            "Monthly siGit Code Cloud allowance reached. \
             It resets at the start of your next billing period."
        );
    }

    #[test]
    fn a_body_that_is_not_an_error_envelope_keeps_the_status() {
        let described =
            describe_api_error(reqwest::StatusCode::BAD_GATEWAY, "<html>bad gateway</html>");

        assert!(described.contains("502"), "{described}");
        assert!(described.contains("bad gateway"), "{described}");
    }

    #[test]
    fn an_empty_body_still_says_something() {
        let described = describe_api_error(reqwest::StatusCode::INTERNAL_SERVER_ERROR, "   ");

        assert_eq!(described, "endpoint returned 500 Internal Server Error");
    }

    #[test]
    fn an_oversized_body_is_cut_down() {
        let body = "x".repeat(ERROR_BODY_LIMIT * 3);

        let described = describe_api_error(reqwest::StatusCode::BAD_GATEWAY, &body);

        assert!(described.len() < ERROR_BODY_LIMIT + 100, "{described}");
        assert!(described.ends_with('…'), "{described}");
    }

    /// An envelope with nothing useful in it must not shadow the status, which
    /// would leave the user with an empty error.
    #[test]
    fn an_envelope_with_a_blank_message_falls_back() {
        let described = describe_api_error(
            reqwest::StatusCode::FORBIDDEN,
            r#"{"error":{"message":"  "}}"#,
        );

        assert!(described.contains("403"), "{described}");
    }

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

    #[tokio::test]
    async fn cancelled_tool_results_close_out_history() {
        let backend = OpenAiBackend::new("http://localhost", "", "test-model", None);
        backend
            .history
            .lock()
            .await
            .push(streamed_assistant_history(
                "",
                &[ToolCall {
                    id: "call_9".to_string(),
                    name: "run_command".to_string(),
                    arguments: r#"{"command":"ls"}"#.to_string(),
                }],
            ));

        backend
            .record_cancelled_tool_results(vec![ToolResult {
                tool_call_id: "call_9".to_string(),
                content: "cancelled by the user".to_string(),
            }])
            .await;

        let history = backend.history.lock().await;
        let last = history.last().unwrap();
        assert_eq!(last["role"], "tool");
        assert_eq!(last["tool_call_id"], "call_9");
        assert_eq!(last["content"], "cancelled by the user");
    }

    #[test]
    fn carryover_drops_system_messages_and_keeps_the_turns() {
        let snapshot = vec![
            serde_json::json!({ "role": "system", "content": "old prompt" }),
            serde_json::json!({ "role": "user", "content": "hello" }),
            serde_json::json!({ "role": "assistant", "content": "hi" }),
        ];

        let carried = carryover_history(snapshot);

        assert_eq!(carried.len(), 2);
        assert_eq!(carried[0]["role"], "user");
        assert_eq!(carried[1]["content"], "hi");
    }

    #[test]
    fn carryover_strips_a_tool_call_that_never_got_a_result() {
        // Switching mid-turn: the assistant asked for a tool, nothing answered.
        let snapshot = vec![
            serde_json::json!({ "role": "user", "content": "read a.rs" }),
            streamed_assistant_history(
                "on it",
                &[ToolCall {
                    id: "call_1".to_string(),
                    name: "read_file".to_string(),
                    arguments: "{}".to_string(),
                }],
            ),
        ];

        let carried = carryover_history(snapshot);

        assert_eq!(carried.len(), 2);
        assert_eq!(carried[1]["content"], "on it");
        assert!(
            carried[1].get("tool_calls").is_none(),
            "an unanswered tool call must not survive the switch"
        );
    }

    #[test]
    fn carryover_drops_a_textless_unanswered_tool_call_and_its_late_result() {
        let snapshot = vec![
            streamed_assistant_history(
                "",
                &[ToolCall {
                    id: "call_1".to_string(),
                    name: "read_file".to_string(),
                    arguments: "{}".to_string(),
                }],
            ),
            // An orphan: its assistant message is gone with the line above.
            serde_json::json!({
                "role": "tool",
                "tool_call_id": "call_other",
                "content": "file contents",
            }),
        ];

        assert!(carryover_history(snapshot).is_empty());
    }

    #[test]
    fn carryover_keeps_an_answered_tool_call_paired_with_its_result() {
        let snapshot = vec![
            streamed_assistant_history(
                "",
                &[ToolCall {
                    id: "call_1".to_string(),
                    name: "read_file".to_string(),
                    arguments: "{}".to_string(),
                }],
            ),
            serde_json::json!({
                "role": "tool",
                "tool_call_id": "call_1",
                "content": "file contents",
            }),
        ];

        let carried = carryover_history(snapshot);

        assert_eq!(carried.len(), 2);
        assert_eq!(carried[0]["tool_calls"][0]["id"], "call_1");
        assert_eq!(carried[1]["tool_call_id"], "call_1");
    }

    #[tokio::test]
    async fn adopt_carryover_replays_the_thread_under_the_new_system_prompt() {
        let new_backend =
            OpenAiBackend::new("http://localhost", "", "m", Some("new prompt".into()));

        let carried = carryover_history(vec![
            serde_json::json!({ "role": "system", "content": "old prompt" }),
            serde_json::json!({ "role": "user", "content": "hello" }),
            serde_json::json!({ "role": "assistant", "content": "hi" }),
        ]);
        adopt_carryover(&new_backend, carried).await;

        let history = new_backend.history_snapshot().await;
        assert_eq!(history.len(), 3);
        assert_eq!(history[0]["role"], "system");
        assert_eq!(history[0]["content"], "new prompt");
        assert_eq!(history[1]["content"], "hello");
        assert_eq!(history[2]["content"], "hi");
    }

    #[tokio::test]
    async fn adopt_carryover_leaves_a_fresh_backend_alone() {
        let new_backend =
            OpenAiBackend::new("http://localhost", "", "m", Some("new prompt".into()));

        adopt_carryover(&new_backend, Vec::new()).await;

        let history = new_backend.history_snapshot().await;
        assert_eq!(history.len(), 1);
        assert_eq!(history[0]["content"], "new prompt");
    }

    #[test]
    fn estimate_tokens_scales_with_serialized_size() {
        assert_eq!(estimate_tokens(&[]), 0);

        let short = vec![serde_json::json!({ "role": "user", "content": "hi" })];
        let long = vec![serde_json::json!({ "role": "user", "content": "x".repeat(4_000) })];
        let short_estimate = estimate_tokens(&short);
        let long_estimate = estimate_tokens(&long);

        assert!(short_estimate > 0, "non-empty history estimates > 0 tokens");
        assert!(long_estimate > short_estimate, "longer history costs more");
        // 4,000 content chars / 4 ≈ 1,000 tokens, plus a little JSON framing.
        assert!((1_000..1_100).contains(&long_estimate), "{long_estimate}");
    }

    #[tokio::test]
    async fn openai_snapshot_restore_round_trips_exactly() {
        let backend = OpenAiBackend::new("http://localhost", "", "m", Some("be helpful".into()));
        {
            let mut history = backend.history.lock().await;
            history.push(serde_json::json!({ "role": "user", "content": "hello" }));
            history.push(streamed_assistant_history(
                "",
                &[ToolCall {
                    id: "call_1".to_string(),
                    name: "read_file".to_string(),
                    arguments: r#"{"path":"a.rs"}"#.to_string(),
                }],
            ));
            history.push(serde_json::json!({
                "role": "tool", "tool_call_id": "call_1", "content": "fn main() {}",
            }));
            history.push(serde_json::json!({ "role": "assistant", "content": "done" }));
        }
        let snapshot = backend.history_snapshot().await;
        assert_eq!(
            snapshot[0]["role"], "system",
            "snapshot keeps the system message"
        );

        // Restoring into a backend seeded with a *different* system prompt must
        // replace everything, including that seed.
        let restored = OpenAiBackend::new("http://localhost", "", "m", Some("other seed".into()));
        restored.restore_history(snapshot.clone()).await;
        assert_eq!(restored.history_snapshot().await, snapshot);
    }

    /// Minimal scripted OpenAI-compatible endpoint: accepts one HTTP request on
    /// a std listener and answers with a fixed non-streaming completion.
    fn spawn_completion_stub(summary: &str) -> std::net::SocketAddr {
        use std::io::{Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let body = serde_json::json!({
            "choices": [{ "message": { "role": "assistant", "content": summary } }]
        })
        .to_string();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            // Read until the full request (headers + content-length body) is in.
            let mut request = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                let n = stream.read(&mut chunk).unwrap_or(0);
                if n == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..n]);
                if let Some(headers_end) =
                    request.windows(4).position(|window| window == b"\r\n\r\n")
                {
                    let headers = String::from_utf8_lossy(&request[..headers_end]);
                    let content_length = headers
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|value| value.trim().parse::<usize>().unwrap_or(0))
                        })
                        .unwrap_or(0);
                    if request.len() >= headers_end + 4 + content_length {
                        break;
                    }
                }
            }
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                 content-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes());
        });
        addr
    }

    #[tokio::test]
    async fn compact_history_rebuilds_system_summary_and_tail() {
        let addr = spawn_completion_stub("We refactored backend.rs; tests pass.");
        let backend = OpenAiBackend::new(
            format!("http://{addr}/v1"),
            "test-key",
            "test-model",
            Some("be helpful".into()),
        );
        {
            let mut history = backend.history.lock().await;
            for i in 0..5 {
                let role = if i % 2 == 0 { "user" } else { "assistant" };
                history.push(serde_json::json!({
                    "role": role, "content": format!("message {i}"),
                }));
            }
        }

        backend.compact_history(2).await.unwrap();

        let history = backend.history_snapshot().await;
        assert_eq!(history.len(), 4, "system + summary + last 2: {history:?}");
        assert_eq!(history[0]["role"], "system");
        assert_eq!(history[0]["content"], "be helpful");
        assert_eq!(history[1]["role"], "user");
        let summary_text = history[1]["content"].as_str().unwrap();
        assert!(summary_text.starts_with("[Conversation summary]\n"));
        assert!(summary_text.contains("We refactored backend.rs; tests pass."));
        assert_eq!(
            history[2],
            serde_json::json!({ "role": "assistant", "content": "message 3" })
        );
        assert_eq!(
            history[3],
            serde_json::json!({ "role": "user", "content": "message 4" })
        );
    }

    #[tokio::test]
    async fn compact_history_failure_leaves_history_intact() {
        // No listener at this address: the summarization request fails, and
        // history must roll back to exactly what it was.
        let backend =
            OpenAiBackend::new("http://127.0.0.1:9", "", "test-model", Some("sys".into()));
        backend
            .history
            .lock()
            .await
            .push(serde_json::json!({ "role": "user", "content": "hello" }));
        let before = backend.history_snapshot().await;

        assert!(backend.compact_history(2).await.is_err());
        assert_eq!(backend.history_snapshot().await, before);
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
