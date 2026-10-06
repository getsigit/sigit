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
use onde::inference::{ChatEngine, ChatMessage, ChatRole, EngineStatus, ToolDefinition};
use serde::Deserialize;
use tokio::sync::Mutex;

// ── Neutral types ───────────────────────────────────────────────────────────────

/// A tool the model may call, in a provider-neutral form. `parameters_schema` is
/// a JSON Schema encoded as a string (matching how siGit Code already declares
/// tools).
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
    /// Why the model stopped. Only the last round of a turn decides how the
    /// turn is reported; a round that goes on to run tools is not the end.
    pub finish: FinishReason,
}

/// Why the model stopped generating, reduced to what the agent loop acts on.
/// Endpoints report it as `finish_reason`; without it a reply cut off at the
/// token limit is indistinguishable from one the model finished.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FinishReason {
    /// The model finished on its own: `stop`, `tool_calls`, or no reason given.
    #[default]
    Complete,
    /// The output ran into the token limit, so the reply is cut short.
    Length,
    /// The endpoint withheld the reply instead of answering.
    ContentFilter,
}

impl FinishReason {
    /// Map a wire `finish_reason`. OpenAI's spellings are `length` and
    /// `content_filter`; gateways in front of other providers sometimes pass
    /// the upstream's own word through, so those are accepted too.
    fn from_wire(reason: Option<&str>) -> Self {
        match reason {
            Some("length" | "max_tokens") => Self::Length,
            Some("content_filter" | "refusal") => Self::ContentFilter,
            _ => Self::Complete,
        }
    }
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

/// Largest transcript (in estimated tokens, see [`estimate_tokens`]) sent to
/// the model for summarization in one go.
///
/// Compaction runs because the conversation has outgrown the context budget,
/// so shipping the whole transcript verbatim asks the endpoint to process
/// exactly the payload that is already too big — on siGit Code Cloud that
/// request sits in inference until the gateway gives up (504), which is the
/// failure behind issue #125. Capping the transcript keeps the summarization
/// round inside the window the model actually has; the oldest and newest
/// exchanges carry the intent and the current state, while the truncated
/// middle is the part a summary can afford to lose.
const SUMMARY_TRANSCRIPT_TOKEN_CAP: usize = 12_000;

/// Trim `transcript` to about `cap` estimated tokens by cutting whole lines
/// from the middle, keeping the opening and closing exchanges.
///
/// A middle cut preserves the two ends that matter most to a summary: how the
/// session started (the original request) and where it stands now. Cutting by
/// whole lines keeps individual messages intact. A transcript already under
/// the cap is returned unchanged.
///
/// Line boundaries are best-effort. A head with no newline in its half is
/// dropped and the tail keeps its own half. A tail with no newline in its half
/// (a single huge message, or a giant final tool result) can't be cut on a
/// line, so the newest `cap` worth of text is kept instead and the head goes:
/// the newest exchanges are the ones the summary can least afford to lose.
fn truncate_transcript_middle(transcript: &str, cap: usize) -> String {
    let budget = cap * 4; // estimate_tokens is chars / 4; invert it.
    if transcript.len() <= budget {
        return transcript.to_string();
    }

    let total = transcript.len();
    // Each side gets half the budget, so the two together stay inside the cap.
    // Walk each half to a line boundary. A head with no boundary in reach
    // contributes nothing; a tail with none takes the hard cut below.
    let half = budget / 2;
    let head_end = {
        let mut cut = half.min(total);
        while cut < total && !transcript.is_char_boundary(cut) {
            cut += 1;
        }
        cut = cut.min(total);
        if transcript[..cut].contains('\n') {
            transcript[..cut].rfind('\n').unwrap() + 1
        } else {
            0
        }
    };
    let tail_start = {
        let mut cut = total.saturating_sub(half);
        while cut < total && !transcript.is_char_boundary(cut) {
            cut += 1;
        }
        if transcript[cut..].contains('\n') {
            transcript[cut..]
                .find('\n')
                .map(|offset| cut + offset + 1)
                .unwrap()
        } else {
            total
        }
    };

    if tail_start == total {
        // The closing exchange has no line boundary in reach (a transcript of
        // one huge message, or a giant newline-less tool result at the end):
        // keep the newest budget. A head-only cut would invert the contract
        // while the placeholder claims older messages were dropped.
        let mut start = total.saturating_sub(budget);
        while start > 0 && !transcript.is_char_boundary(start) {
            start -= 1;
        }
        return format!(
            "{}\n[…older messages omitted…]",
            transcript[start..].trim_start()
        );
    }

    if head_end >= tail_start {
        // No line boundary in reach anywhere: keep the newest budget.
        let mut start = total.saturating_sub(budget);
        while start > 0 && !transcript.is_char_boundary(start) {
            start -= 1;
        }
        return format!(
            "{}\n[…older messages omitted…]",
            transcript[start..].trim_start()
        );
    }

    let omitted_chars = transcript[head_end..tail_start].chars().count();
    format!(
        "{}\n[…{} characters of older messages omitted…]\n{}",
        transcript[..head_end].trim_end(),
        omitted_chars,
        transcript[tail_start..].trim_start(),
    )
}

/// Render a history snapshot as a plain-text transcript, with tool calls and
/// tool results spelled out as prose rather than left in their wire shapes.
///
/// Compaction summarizes a conversation that is, by definition, thick with
/// `tool_calls` and `role: "tool"` messages — but the summarization round asks
/// for a plain answer and so sends no `tools` array. Forwarding the raw shapes
/// in that request produces tool blocks with no schema to validate against,
/// which strict endpoints reject outright: Anthropic answers 400, so every
/// compaction of a session that had ever run a tool failed, permanently, no
/// matter how small the history was. Flattening to text keeps everything the
/// summary actually needs and drops the shapes that only make sense alongside
/// a tool schema. It also sidesteps orphaned `tool_call_id`s and role-
/// alternation rules, neither of which a transcript can violate.
fn transcript_for_summary(history: &[serde_json::Value]) -> String {
    let mut lines: Vec<String> = Vec::new();
    for message in history {
        let role = message["role"].as_str().unwrap_or("user");
        // The system prompt is carried over verbatim, so it needn't be summarized.
        if role == "system" {
            continue;
        }

        let mut parts: Vec<String> = Vec::new();
        let text = message_text(message);
        if !text.trim().is_empty() {
            parts.push(text);
        }
        // A summary cannot carry a picture, but it should say one was there.
        match message_image_count(message) {
            0 => {}
            1 => parts.push("[attached an image]".to_string()),
            count => parts.push(format!("[attached {count} images]")),
        }
        for call in message["tool_calls"].as_array().into_iter().flatten() {
            parts.push(format!(
                "called {}({})",
                call["function"]["name"].as_str().unwrap_or("tool"),
                call["function"]["arguments"].as_str().unwrap_or_default(),
            ));
        }
        if parts.is_empty() {
            continue;
        }

        let label = if role == "tool" { "tool result" } else { role };
        lines.push(format!("{label}: {}", parts.join("\n")));
    }
    lines.join("\n\n")
}

/// Crude token estimate for a history snapshot: serialized characters / 4.
/// Deliberately model-agnostic — it only needs to be in the right ballpark to
/// decide when compaction is worth an extra inference round.
pub fn estimate_tokens(history: &[serde_json::Value]) -> usize {
    let mut chars = 0;
    let mut images = 0;
    for message in history {
        let image_count = message_image_count(message);
        if image_count == 0 {
            chars += message.to_string().chars().count();
        } else {
            // An image travels as base64, megabytes of it, but a model bills
            // it by resolution. Counting the payload as text would put every
            // session with one screenshot permanently over budget.
            chars += message_text(message).chars().count();
            images += image_count;
        }
    }
    chars / 4 + images * IMAGE_TOKEN_ESTIMATE
}

/// What one attached image is assumed to cost. Providers land between a few
/// hundred and a couple of thousand tokens depending on resolution; like the
/// rest of [`estimate_tokens`] this only has to be in the right ballpark.
const IMAGE_TOKEN_ESTIMATE: usize = 1_000;

/// The text of a history message. `content` is a plain string for almost every
/// message; a user message that carries an image uses OpenAI's content-part
/// array instead, and its text parts are joined here.
pub fn message_text(message: &serde_json::Value) -> String {
    match &message["content"] {
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Array(parts) => parts
            .iter()
            .filter_map(|part| part["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// How many images a history message carries as `image_url` content parts.
pub fn message_image_count(message: &serde_json::Value) -> usize {
    message["content"]
        .as_array()
        .map(|parts| {
            parts
                .iter()
                .filter(|part| part["type"] == "image_url")
                .count()
        })
        .unwrap_or(0)
}

/// A sink for streaming assistant text deltas to the UI as they are produced.
///
/// When a caller passes `Some(sink)`, a streaming-capable backend forwards each
/// text fragment through it as the model emits it; the returned [`TurnResult`]
/// still carries the fully assembled text (and any tool calls). When the sink is
/// `None`, the backend runs in non-streaming mode. Unbounded so the inference
/// task never blocks on a slow consumer.
pub type TokenSink = tokio::sync::mpsc::UnboundedSender<String>;

/// An image attached to a user message: base64 data and its media type, as an
/// ACP client sends it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageInput {
    pub mime_type: String,
    pub data: String,
}

/// What a model that cannot read images is shown in place of one.
const IMAGE_OMITTED_NOTE: &str = "[image omitted: this model cannot read images]";

/// A user message in history form. Plain text keeps the string `content` every
/// other message uses; with images it becomes OpenAI's content-part array,
/// text first, each image as a base64 `data:` URL.
fn user_message(text: &str, images: &[ImageInput]) -> serde_json::Value {
    if images.is_empty() {
        return serde_json::json!({ "role": "user", "content": text });
    }
    let mut parts = Vec::with_capacity(images.len() + 1);
    if !text.is_empty() {
        parts.push(serde_json::json!({ "type": "text", "text": text }));
    }
    for image in images {
        parts.push(serde_json::json!({
            "type": "image_url",
            "image_url": {
                "url": format!("data:{};base64,{}", image.mime_type, image.data),
            },
        }));
    }
    serde_json::json!({ "role": "user", "content": parts })
}

/// `history` as a text-only model has to receive it: a message that carries
/// images is flattened back to a string, with a note where each image was.
///
/// History itself keeps the images. A thread can move to a model that reads
/// them (or back to one), so what was attached is not thrown away just because
/// the model active right now cannot use it.
fn without_images(history: &[serde_json::Value]) -> Vec<serde_json::Value> {
    history
        .iter()
        .map(|message| {
            let images = message_image_count(message);
            if images == 0 {
                return message.clone();
            }
            let mut text = message_text(message);
            for _ in 0..images {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(IMAGE_OMITTED_NOTE);
            }
            let mut flattened = message.clone();
            flattened["content"] = serde_json::Value::String(text);
            flattened
        })
        .collect()
}

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

    /// Continue the turn by returning tool results. `allow_tool_calls` controls
    /// whether `tools` are offered again; the complete catalog remains available
    /// so a disabled round can recognize and suppress tool-shaped model output.
    /// `sink` streams assistant text when set.
    async fn send_tool_results(
        &self,
        results: Vec<ToolResult>,
        tools: &[ToolSpec],
        allow_tool_calls: bool,
        sink: Option<&TokenSink>,
    ) -> Result<TurnResult, BackendError>;

    /// Record tool results in the conversation history *without* asking the
    /// model to continue the turn. Used when a turn is abandoned mid-round
    /// (the user cancelled at the permission gate): by then the assistant
    /// message carrying the tool calls is already in history, and leaving them
    /// unanswered makes strict OpenAI-compatible endpoints reject every later
    /// request in the session.
    async fn record_cancelled_tool_results(&self, results: Vec<ToolResult>);

    /// Start a turn from a user message that carries images.
    ///
    /// The default drops the images and sends the text, which is right for a
    /// backend that cannot read them. Callers check [`Self::accepts_images`]
    /// first so the user can be told, instead of the image vanishing.
    async fn send_message_with_images(
        &self,
        text: &str,
        images: &[ImageInput],
        tools: &[ToolSpec],
        sink: Option<&TokenSink>,
    ) -> Result<TurnResult, BackendError> {
        let _ = images;
        self.send_message_with_tools(text, tools, sink).await
    }

    /// Whether the model behind this backend reads images. On-device models do
    /// not; a remote one answers from its model id.
    fn accepts_images(&self) -> bool {
        false
    }

    /// Whether inference runs over the network (a configured provider) rather
    /// than on-device. Drives UI labelling so the displayed model can't claim a
    /// local model while requests actually go to the cloud.
    fn is_remote(&self) -> bool;

    /// A backend for the same endpoint and model with an empty conversation,
    /// or `None` when the conversation cannot be separated from the backend
    /// (on-device, the engine holds it). An editor keeps several threads open
    /// in one process; this is what lets each of them own its history instead
    /// of taking turns on a shared one.
    fn fresh(&self) -> Option<Arc<dyn InferenceBackend>> {
        None
    }

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
    /// `[system message, summary, last keep_last non-system messages]`. When
    /// the summarization round itself fails, a backend should fall back to a
    /// deterministic shrink (dropping the oldest messages) rather than leave
    /// an over-budget session stuck; an `Err` here means even that fallback
    /// could not fit the history in the budget.
    async fn compact_history(&self, keep_last: usize) -> Result<(), BackendError>;
}

// ── Local backend (onde ChatEngine) ──────────────────────────────────────────────

/// On-device inference. A thin adapter over `onde::ChatEngine`.
pub struct LocalBackend {
    engine: Arc<ChatEngine>,
    /// History restored before an on-device model is loaded. Onde ignores
    /// `push_history` while unloaded, so keep the snapshot here until the
    /// engine can accept it instead of silently dropping a loaded session.
    pending_history: Mutex<Option<Vec<serde_json::Value>>>,
}

impl LocalBackend {
    pub fn new(engine: Arc<ChatEngine>) -> Self {
        Self {
            engine,
            pending_history: Mutex::new(None),
        }
    }

    async fn apply_pending_history(&self) -> Result<(), BackendError> {
        if self.pending_history.lock().await.is_none() {
            return Ok(());
        }
        let status = self.engine.info().await.status;
        if status != EngineStatus::Ready {
            return Err(format!(
                "on-device model is not ready to restore session history (status: {status})"
            ));
        }
        let Some(history) = self.pending_history.lock().await.take() else {
            return Ok(());
        };

        self.engine.clear_history().await;
        for entry in history {
            let role = entry["role"].as_str().unwrap_or("");
            // The on-device engine keeps text only, so an image attached
            // while a cloud model was active stays behind here.
            let content = message_text(&entry);
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
        Ok(())
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
        self.apply_pending_history().await?;
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
        tools: &[ToolSpec],
        allow_tool_calls: bool,
        sink: Option<&TokenSink>,
    ) -> Result<TurnResult, BackendError> {
        self.apply_pending_history().await?;
        let onde_results: Vec<onde::inference::ToolResult> = results
            .into_iter()
            .map(|result| onde::inference::ToolResult {
                tool_call_id: result.tool_call_id,
                content: result.content,
            })
            .collect();

        // A forced-text round is the only round onde can stream, since no
        // further tool calls are parsed.
        if let Some(sink) = sink
            && !allow_tool_calls
        {
            let rx = self
                .engine
                .stream_tool_results(onde_results, None)
                .await
                .map_err(|error| error.to_string())?;
            return drain_onde_stream(rx, sink).await;
        }

        let onde_tools = allow_tool_calls.then(|| to_onde_tools(tools));
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
        if let Some(history) = self.pending_history.lock().await.as_ref() {
            return history.clone();
        }
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
        *self.pending_history.lock().await = Some(history);
        if self.engine.info().await.status == EngineStatus::Ready {
            // Ready was just observed, so failure here can only mean a status
            // transition; keep the pending copy for the next inference call.
            let _ = self.apply_pending_history().await;
        }
    }

    async fn compact_history(&self, keep_last: usize) -> Result<(), BackendError> {
        self.apply_pending_history().await?;
        let snapshot = self.engine.history().await;

        // Leading system messages carry session context; keep them all.
        let system: Vec<ChatMessage> = snapshot
            .iter()
            .take_while(|message| message.role == ChatRole::System)
            .cloned()
            .collect();
        let non_system: Vec<ChatMessage> = snapshot
            .into_iter()
            .filter(|message| message.role != ChatRole::System)
            .collect();
        let tail_start = non_system.len().saturating_sub(keep_last);
        let tail: Vec<ChatMessage> = non_system[tail_start..].to_vec();

        // One plain (tool-free) inference round produces the summary. Note the
        // cap only bounds this request's own payload: the engine still replays
        // its full (over-budget) history underneath it, so an already
        // over-budget local session usually fails here and lands in the
        // fallback below — the cap is about not making the doomed round worse.
        let transcript = truncate_transcript_middle(
            &chat_messages_transcript(&non_system),
            SUMMARY_TRANSCRIPT_TOKEN_CAP,
        );
        let result = self
            .engine
            .send_message(format!("{transcript}\n\n{SUMMARIZE_PROMPT}"))
            .await
            .map_err(|error| error.to_string());

        let rebuild: Vec<ChatMessage> = match result {
            Ok(result) => {
                // Local models may reason in <think> blocks; keep only the visible part.
                let (_think, summary) = crate::chat::strip_think_blocks(&result.text);
                let mut rebuilt = system;
                rebuilt.push(ChatMessage::user(format!(
                    "[Conversation summary]\n{summary}"
                )));
                rebuilt.extend(tail);
                rebuilt
            }
            Err(error) => {
                log::warn!(
                    "local summarization round failed ({error}); falling back to plain truncation"
                );
                // Same deterministic shrink as the remote path, on the plain
                // chat shapes the engine holds (its history has no tool-call
                // structure, so there are no orphaned tool results to guard).
                match truncate_chat_messages_to_budget(system, tail) {
                    Some(rebuilt) => rebuilt,
                    // The system messages alone overflow the window: unrecoverable.
                    None => return Err(error),
                }
            }
        };

        self.engine.clear_history().await;
        for message in rebuild {
            self.engine.push_history(message).await;
        }
        Ok(())
    }
}

/// The local counterpart of `truncate_history_to_budget`: system messages, the
/// truncation placeholder, and the newest of `tail` that fit the budget.
///
/// The tail may drain to empty. A newest message that overflows the window on
/// its own (a giant tool result, the very thing that triggers compaction) is
/// dropped rather than failing the session, so `None` only means the system
/// messages alone don't fit.
fn truncate_chat_messages_to_budget(
    system: Vec<ChatMessage>,
    mut tail: Vec<ChatMessage>,
) -> Option<Vec<ChatMessage>> {
    let budget = DEFAULT_CONTEXT_TOKEN_BUDGET * 4; // estimate_tokens is chars / 4.
    let fixed_chars: usize = system
        .iter()
        .map(|message| message.content.len())
        .sum::<usize>()
        + TRUNCATION_PLACEHOLDER.len();
    if fixed_chars > budget {
        return None;
    }

    let mut tail_chars: usize = tail.iter().map(|message| message.content.len()).sum();
    let mut drop = 0;
    while fixed_chars + tail_chars > budget {
        tail_chars -= tail[drop].content.len();
        drop += 1;
    }
    tail.drain(..drop);

    let mut rebuilt = system;
    rebuilt.push(ChatMessage::user(TRUNCATION_PLACEHOLDER.to_string()));
    rebuilt.extend(tail);
    Some(rebuilt)
}

/// Flatten `onde` chat messages into the transcript shape summarization
/// expects (the local engine's history has no tool-call structure, so this is
/// a plain role/content rendering).
fn chat_messages_transcript(history: &[ChatMessage]) -> String {
    history
        .iter()
        .filter(|message| message.role != ChatRole::System && !message.content.trim().is_empty())
        .map(|message| format!("{}: {}", message.role, message.content))
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Drain an onde streaming receiver, forwarding each token to `sink` and
/// assembling the full text. onde reports stream failures as a final chunk whose
/// `finish_reason` is `"error: …"`; surface those as a backend error.
async fn drain_onde_stream(
    mut rx: tokio::sync::mpsc::Receiver<onde::inference::StreamChunk>,
    sink: &TokenSink,
) -> Result<TurnResult, BackendError> {
    let mut text = String::new();
    let mut finish = FinishReason::default();
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
            if let Some(reason) = chunk.finish_reason.as_deref()
                && let Some(message) = reason.strip_prefix("error: ")
            {
                return Err(message.to_string());
            }
            finish = FinishReason::from_wire(chunk.finish_reason.as_deref());
            break;
        }
    }
    Ok(TurnResult {
        text,
        tool_calls: Vec::new(),
        finish,
    })
}

/// Convert an `onde` tool-aware result into the neutral [`TurnResult`].
fn onde_result_to_turn(result: onde::inference::ToolAwareResult) -> TurnResult {
    TurnResult {
        finish: FinishReason::from_wire(Some(&result.finish_reason)),
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
    /// Whether `model` reads images (see `provider::model_accepts_images`).
    /// When it does not, images already in `history` are left out of each
    /// request rather than sent to an endpoint that would refuse them.
    accepts_images: bool,
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
        let model = model.into();
        Self {
            base_url: base_url.into(),
            api_key: api_key.into(),
            accepts_images: crate::provider::model_accepts_images(&model),
            model,
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

    /// POST the current history and apply the assistant reply to history.
    /// `tools` is always the known catalog, while `allow_tool_calls` determines
    /// whether it is advertised to the model and whether returned calls may run.
    /// Streams via SSE when `sink` is set; otherwise reads a single JSON response.
    ///
    /// A reply whose only attempt at a tool call was a block that couldn't be
    /// parsed gets one retry: the model is told the call didn't run and asked
    /// to make it again through the structured interface. Without that the
    /// turn ends on whatever prose preceded the broken block, and the call is
    /// silently lost.
    async fn complete(
        &self,
        tools: &[ToolSpec],
        allow_tool_calls: bool,
        sink: Option<&TokenSink>,
    ) -> Result<TurnResult, BackendError> {
        let (result, malformed) = self.request(tools, allow_tool_calls, sink).await?;
        if malformed == 0 || !allow_tool_calls || !result.tool_calls.is_empty() {
            return Ok(result);
        }

        log::warn!(
            "dropped {malformed} unparseable inline tool call(s); asking the model to retry"
        );
        self.history.lock().await.push(serde_json::json!({
            "role": "user",
            "content": MALFORMED_TOOL_CALL_RETRY,
        }));
        if let Some(sink) = sink
            && !result.text.trim().is_empty()
        {
            // The retry's text follows what already streamed; keep it from
            // running on into the previous sentence.
            let _ = sink.send("\n\n".to_string());
        }
        let (retry, retry_malformed) = self.request(tools, allow_tool_calls, sink).await?;
        if retry_malformed > 0 && retry.tool_calls.is_empty() {
            log::warn!(
                "the retry also wrote {retry_malformed} unparseable inline tool call(s); \
                 ending the turn without them"
            );
        }
        Ok(TurnResult {
            text: join_reply_text(&result.text, &retry.text),
            tool_calls: retry.tool_calls,
            finish: retry.finish,
        })
    }

    /// One chat-completion round trip. Returns the turn plus how many inline
    /// tool-call blocks were dropped because they couldn't be parsed.
    async fn request(
        &self,
        tools: &[ToolSpec],
        allow_tool_calls: bool,
        sink: Option<&TokenSink>,
    ) -> Result<(TurnResult, usize), BackendError> {
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let streaming = sink.is_some();

        let messages = {
            let history = self.history.lock().await;
            if self.accepts_images {
                history.clone()
            } else {
                without_images(&history)
            }
        };
        let mut body = serde_json::json!({
            "model": self.model,
            "messages": messages,
            "stream": streaming,
        });
        if allow_tool_calls && !tools.is_empty() {
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
            self.consume_stream(response, sink, tools, allow_tool_calls)
                .await
        } else {
            self.consume_json(response, tools, allow_tool_calls).await
        }
    }

    /// One self-contained summarization round: summarize `transcript` without
    /// touching the live history.
    ///
    /// Unlike [`complete`], which replays and mutates `self.history`, this
    /// sends its own one-off message list. Compaction only rewrites history
    /// once it has a summary in hand, so a failed round leaves the session
    /// exactly as it was — and the caller can fall back to plain truncation
    /// instead of failing the session (issue #125).
    async fn summarize(
        &self,
        system: &[serde_json::Value],
        transcript: &str,
    ) -> Result<String, BackendError> {
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let mut messages = Vec::with_capacity(system.len() + 1);
        messages.extend(system.iter().cloned());
        messages.push(serde_json::json!({
            "role": "user",
            "content": format!("{transcript}\n\n{SUMMARIZE_PROMPT}"),
        }));
        let body = serde_json::json!({
            "model": self.model,
            "messages": messages,
            "stream": false,
        });

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

        let parsed: ChatCompletion = response
            .json()
            .await
            .map_err(|error| format!("response parse error: {error}"))?;
        parsed
            .choices
            .into_iter()
            .next()
            .and_then(|choice| choice.message.content)
            .filter(|text| !text.trim().is_empty())
            .ok_or_else(|| "endpoint returned no summary".to_string())
    }

    /// Parse a single non-streaming chat-completion response.
    async fn consume_json(
        &self,
        response: reqwest::Response,
        tools: &[ToolSpec],
        allow_tool_calls: bool,
    ) -> Result<(TurnResult, usize), BackendError> {
        let parsed: ChatCompletion = response
            .json()
            .await
            .map_err(|error| format!("response parse error: {error}"))?;

        let choice = parsed
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| "endpoint returned no choices".to_string())?;
        let finish = FinishReason::from_wire(choice.finish_reason.as_deref());
        let message = choice.message;

        let mut text = message.content.clone().unwrap_or_default();
        let mut tool_calls: Vec<ToolCall> = message
            .tool_calls
            .iter()
            .flatten()
            .map(|call| ToolCall {
                id: call.id.clone(),
                name: call.function.name.clone(),
                arguments: call.function.arguments.clone(),
            })
            .collect();

        let extracted = crate::inline_tool_calls::extract(&text, tools);
        let malformed = extracted.malformed;
        if malformed > 0 {
            log::warn!("dropped {malformed} unparseable inline tool call(s) from the reply");
        }

        if !allow_tool_calls {
            let recovered = extracted.calls;
            text = extracted.text;
            let suppressed = tool_calls.len() + recovered.len();
            if suppressed > 0 {
                let names = tool_calls
                    .iter()
                    .map(|call| call.name.as_str())
                    .chain(recovered.iter().map(|call| call.name.as_str()))
                    .collect::<Vec<_>>()
                    .join(", ");
                log::warn!(
                    "suppressed {suppressed} tool call(s) from a forced-text response: {names}"
                );
            }
            self.history
                .lock()
                .await
                .push(streamed_assistant_history(&text, &[]));
            return Ok((
                TurnResult {
                    text,
                    tool_calls: Vec::new(),
                    finish,
                },
                malformed,
            ));
        }

        // Some models write a tool call out as literal `<tool_call>` text
        // instead of using the structured field (see `inline_tool_calls`).
        // Recover it, or the turn ends with the tag rendered as prose and
        // whatever the model meant to do is dropped.
        if tool_calls.is_empty() {
            let cleaned = extracted.text;
            let recovered = extracted.calls;
            if !recovered.is_empty() {
                log::warn!(
                    "recovered {} tool call(s) the model emitted as text instead of a structured call",
                    recovered.len()
                );
                let tool_calls: Vec<ToolCall> = recovered
                    .into_iter()
                    .enumerate()
                    .map(|(index, call)| ToolCall {
                        id: format!("call_recovered_{index}"),
                        name: call.name,
                        arguments: call.arguments,
                    })
                    .collect();
                // Record the recovered shape, not the raw tag: the tool
                // results that follow have to answer an assistant message
                // that actually carries these calls, or the next request is
                // rejected for orphaned tool results.
                self.history
                    .lock()
                    .await
                    .push(streamed_assistant_history(&cleaned, &tool_calls));
                return Ok((
                    TurnResult {
                        text: cleaned,
                        tool_calls,
                        finish,
                    },
                    malformed,
                ));
            }
            if malformed > 0 {
                // Keep the broken block out of history as well as the reply;
                // see `inline_tool_calls` for why it poisons later turns.
                self.push_reply_without_calls(&cleaned).await;
                return Ok((
                    TurnResult {
                        text: cleaned,
                        tool_calls,
                        finish,
                    },
                    malformed,
                ));
            }
        } else if malformed > 0 || !extracted.calls.is_empty() {
            // Structured calls arrived with inline blocks beside them. As on
            // the streaming path, inline calls that parse run alongside the
            // structured ones, broken blocks are dropped, and neither kind of
            // block is left in the reply or the history.
            let offset = tool_calls.len();
            tool_calls.extend(
                extracted
                    .calls
                    .into_iter()
                    .enumerate()
                    .map(|(index, call)| ToolCall {
                        id: format!("call_recovered_{}", offset + index),
                        name: call.name,
                        arguments: call.arguments,
                    }),
            );
            self.history
                .lock()
                .await
                .push(streamed_assistant_history(&extracted.text, &tool_calls));
            return Ok((
                TurnResult {
                    text: extracted.text,
                    tool_calls,
                    finish,
                },
                malformed,
            ));
        }

        // Record the assistant turn so later tool results have context.
        self.history.lock().await.push(message.into_history_value());

        Ok((
            TurnResult {
                text,
                tool_calls,
                finish,
            },
            0,
        ))
    }

    /// Record a text-only assistant reply. An empty one is skipped: strict
    /// endpoints reject an assistant message with neither content nor tool
    /// calls, and there is nothing in it worth replaying.
    async fn push_reply_without_calls(&self, text: &str) {
        if !text.is_empty() {
            self.history
                .lock()
                .await
                .push(streamed_assistant_history(text, &[]));
        }
    }

    /// Consume an OpenAI Server-Sent Events stream, forwarding content deltas to
    /// `sink` and reassembling any tool calls (which arrive fragmented across
    /// chunks, keyed by `index`).
    async fn consume_stream(
        &self,
        response: reqwest::Response,
        sink: &TokenSink,
        tools: &[ToolSpec],
        allow_tool_calls: bool,
    ) -> Result<(TurnResult, usize), BackendError> {
        use futures::StreamExt;

        let mut stream = response.bytes_stream();
        // Newlines are ASCII, so splitting raw bytes on `\n` never bisects a
        // multibyte UTF-8 sequence; we only lossily decode whole lines.
        let mut buffer: Vec<u8> = Vec::new();
        let mut text = String::new();
        let mut tool_accum: Vec<StreamingToolCall> = Vec::new();
        let mut done = false;
        // Arrives on a late chunk, usually one with an empty delta.
        let mut finish = FinishReason::default();
        // Recovers a tool call the model wrote as literal `<tool_call>` text
        // instead of a structured delta. Scanning here (rather than after the
        // stream) keeps the tag off the UI: content goes straight to `sink` as
        // it arrives, so by the time a whole turn is assembled the tag has
        // already been rendered. See `inline_tool_calls`.
        let mut scanner = crate::inline_tool_calls::StreamScanner::new(tools);
        let mut recovered: Vec<ToolCall> = Vec::new();
        let mut malformed = 0usize;

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
                if let Some(reason) = choice.finish_reason.as_deref() {
                    finish = FinishReason::from_wire(Some(reason));
                }
                if let Some(content) = choice.delta.content
                    && !content.is_empty()
                {
                    let mut cancelled = false;
                    for event in scanner.push(&content) {
                        match event {
                            crate::inline_tool_calls::ScanEvent::Text(chunk) => {
                                text.push_str(&chunk);
                                if sink.send(chunk).is_err() {
                                    // Consumer dropped (turn cancelled).
                                    cancelled = true;
                                    break;
                                }
                            }
                            crate::inline_tool_calls::ScanEvent::ToolCall(call) => {
                                if allow_tool_calls {
                                    log::warn!(
                                        "recovered tool call '{}' the model emitted as text instead of a structured call",
                                        call.name
                                    );
                                }
                                recovered.push(ToolCall {
                                    id: format!("call_recovered_{}", recovered.len()),
                                    name: call.name,
                                    arguments: call.arguments,
                                });
                            }
                            crate::inline_tool_calls::ScanEvent::Malformed(block) => {
                                log_malformed_block(&block);
                                malformed += 1;
                            }
                        }
                    }
                    if cancelled {
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

        // Flush what was held back. A partial marker is just text; a
        // tool-call block that never closed is dropped like any other
        // unparseable one.
        match scanner.finish() {
            Some(crate::inline_tool_calls::ScanEvent::Text(leftover)) => {
                text.push_str(&leftover);
                let _ = sink.send(leftover);
            }
            Some(crate::inline_tool_calls::ScanEvent::Malformed(block)) => {
                log_malformed_block(&block);
                malformed += 1;
            }
            Some(crate::inline_tool_calls::ScanEvent::ToolCall(_)) | None => {}
        }

        let mut tool_calls: Vec<ToolCall> = tool_accum
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
        tool_calls.extend(recovered);

        if !allow_tool_calls && !tool_calls.is_empty() {
            let names = tool_calls
                .iter()
                .map(|call| call.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            log::warn!(
                "suppressed {} tool call(s) from a forced-text response: {names}",
                tool_calls.len()
            );
            tool_calls.clear();
        }

        // Record the assistant turn so later tool results have context.
        if malformed > 0 && tool_calls.is_empty() {
            self.push_reply_without_calls(&text).await;
        } else {
            self.history
                .lock()
                .await
                .push(streamed_assistant_history(&text, &tool_calls));
        }

        Ok((
            TurnResult {
                text,
                tool_calls,
                finish,
            },
            malformed,
        ))
    }
}

/// Sent as a user turn after a reply whose tool call couldn't be parsed.
const MALFORMED_TOOL_CALL_RETRY: &str = "[siGit Code] Your last reply tried to call a tool by \
    writing the call out as text, and it could not be parsed, so nothing ran and the user did \
    not see it. Do not write tool calls or tool results as text. Make the call again using the \
    tool-calling interface, or answer in plain prose if no tool is needed.";

fn log_malformed_block(block: &str) {
    log::warn!(
        "dropped an unparseable inline tool call ({} chars): {}",
        block.len(),
        block.chars().take(200).collect::<String>()
    );
}

/// Join the visible text of a reply and its retry the way the stream showed
/// them: as separate paragraphs.
fn join_reply_text(first: &str, retry: &str) -> String {
    match (first.trim().is_empty(), retry.trim().is_empty()) {
        (true, _) => retry.to_string(),
        (false, true) => first.to_string(),
        (false, false) => format!("{first}\n\n{retry}"),
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
        self.complete(tools, true, sink).await
    }

    async fn send_tool_results(
        &self,
        results: Vec<ToolResult>,
        tools: &[ToolSpec],
        allow_tool_calls: bool,
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
        self.complete(tools, allow_tool_calls, sink).await
    }

    async fn record_cancelled_tool_results(&self, results: Vec<ToolResult>) {
        let mut history = self.history.lock().await;
        for result in results {
            // Cancellation races the in-flight continuation request. That
            // request appends its tool results before awaiting HTTP, so cleanup
            // must be safe whether cancellation won just before or just after
            // the append.
            let matching_call = history.iter().rposition(|message| {
                message["role"] == "assistant"
                    && message["tool_calls"].as_array().is_some_and(|calls| {
                        calls.iter().any(|call| call["id"] == result.tool_call_id)
                    })
            });
            let already_recorded = matching_call.is_some_and(|call_index| {
                history[call_index + 1..].iter().any(|message| {
                    message["role"] == "tool" && message["tool_call_id"] == result.tool_call_id
                })
            });
            if already_recorded {
                continue;
            }
            history.push(serde_json::json!({
                "role": "tool",
                "tool_call_id": result.tool_call_id,
                "content": result.content,
            }));
        }
    }

    async fn send_message_with_images(
        &self,
        text: &str,
        images: &[ImageInput],
        tools: &[ToolSpec],
        sink: Option<&TokenSink>,
    ) -> Result<TurnResult, BackendError> {
        self.history.lock().await.push(user_message(text, images));
        self.complete(tools, true, sink).await
    }

    fn accepts_images(&self) -> bool {
        self.accepts_images
    }

    fn is_remote(&self) -> bool {
        true
    }

    fn fresh(&self) -> Option<Arc<dyn InferenceBackend>> {
        Some(Arc::new(Self {
            base_url: self.base_url.clone(),
            api_key: self.api_key.clone(),
            model: self.model.clone(),
            // The client is a handle on one connection pool; sharing it keeps
            // connections to the endpoint warm across sessions.
            http: self.http.clone(),
            history: Mutex::new(Vec::new()),
            accepts_images: self.accepts_images,
        }))
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

        // Leading system messages carry session context (the seeded prompt,
        // pushed context); keep them all, like the summarization-success path.
        let system: Vec<serde_json::Value> = snapshot
            .iter()
            .take_while(|message| message["role"] == "system")
            .cloned()
            .collect();

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

        // Ask the endpoint for a summary of the conversation so far, through a
        // direct completion call. The request carries the conversation as a
        // flattened transcript in a single user message rather than the live
        // history: this round offers no tools, and a tool-shaped history sent
        // without a tool schema is rejected upstream (see
        // `transcript_for_summary`). The transcript is capped so the request
        // cannot be as large as the over-budget conversation that triggered
        // compaction (see `SUMMARY_TRANSCRIPT_TOKEN_CAP`).
        let transcript = truncate_transcript_middle(
            &transcript_for_summary(&snapshot),
            SUMMARY_TRANSCRIPT_TOKEN_CAP,
        );
        let summary = match self.summarize(&system, &transcript).await {
            Ok(summary) => summary,
            Err(error) => {
                log::warn!(
                    "summarization round failed ({error}); falling back to plain truncation"
                );
                // Compaction must not fail the session: fold the overflow into
                // a placeholder and drop the oldest messages until the rebuild
                // fits the budget. Needs no endpoint, so it cannot fail the way
                // the inference round just did (issue #125).
                let rebuilt = truncate_history_to_budget(&system, tail.clone()).ok_or(error)?;
                *self.history.lock().await = rebuilt;
                return Ok(());
            }
        };

        let mut rebuilt = Vec::with_capacity(system.len() + tail.len() + 1);
        rebuilt.extend(system);
        rebuilt.push(serde_json::json!({
            "role": "user",
            "content": format!("[Conversation summary]\n{summary}"),
        }));
        rebuilt.extend(tail);
        *self.history.lock().await = rebuilt;
        Ok(())
    }
}

/// The placeholder message replacing dropped history when compaction falls
/// back to plain truncation (the model reads it, so it explains the gap).
const TRUNCATION_PLACEHOLDER: &str = "[Older conversation omitted: the session outgrew \
     the context window and the summarization service was unavailable. Earlier \
     messages were dropped, so answers may lack that context.]";

/// Last-resort compaction when the summarization round cannot run: rebuild the
/// history as `[system, placeholder, newest messages that fit the budget]`.
///
/// Returns `None` when even the smallest rebuild exceeds the budget (a
/// pathological case — e.g. one message larger than the whole window); the
/// caller then reports the original summarization error and leaves history
/// untouched.
fn truncate_history_to_budget(
    system: &[serde_json::Value],
    mut tail: Vec<serde_json::Value>,
) -> Option<Vec<serde_json::Value>> {
    /// A `role: "tool"` entry only makes sense behind the assistant message
    /// that requested it; dropping that message strands it.
    fn drop_orphaned_tool_results(tail: &mut Vec<serde_json::Value>) {
        while tail
            .first()
            .is_some_and(|message| message["role"] == "tool")
        {
            tail.remove(0);
        }
    }

    // A caller splitting mid-round can hand in a tail that starts with tool
    // results whose call message already fell outside the kept window.
    drop_orphaned_tool_results(&mut tail);

    loop {
        let mut candidate = Vec::with_capacity(tail.len() + system.len() + 1);
        candidate.extend(system.iter().cloned());
        candidate.push(serde_json::json!({
            "role": "user",
            "content": TRUNCATION_PLACEHOLDER,
        }));
        candidate.extend(tail.iter().cloned());
        if estimate_tokens(&candidate) <= DEFAULT_CONTEXT_TOKEN_BUDGET {
            return Some(candidate);
        }

        // Nothing left to drop: even the minimal rebuild (system + placeholder,
        // no conversation) overflows — the session is genuinely unrecoverable.
        if tail.is_empty() {
            return None;
        }
        if tail[0]["role"] == "assistant"
            && tail[0]["tool_calls"]
                .as_array()
                .is_some_and(|calls| !calls.is_empty())
            && tail.get(1).is_some_and(|message| message["role"] == "tool")
        {
            // Dropping an assistant tool-call message would orphan its
            // results; drop the whole round (call + results) together.
            let mut end = 1;
            while tail
                .get(end)
                .is_some_and(|message| message["role"] == "tool")
            {
                end += 1;
            }
            tail.drain(..end);
        } else {
            tail.remove(0);
            drop_orphaned_tool_results(&mut tail);
        }
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
/// on — except an HTML page, which is a proxy or Rails error page and tells
/// the person nothing the status doesn't (pasting it into chat is the dump
/// seen in issue #125's screenshots).
fn describe_api_error(status: reqwest::StatusCode, body: &str) -> String {
    if let Some(message) = api_error_message(body) {
        return message;
    }

    let body = body.trim();
    if body.is_empty() || is_probable_html(body) {
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

/// Whether `body` looks like an HTML page rather than prose: a doctype or an
/// opening tag in the first chunk. Gateways and Rails apps answer failures
/// with full pages; none of that markup is worth showing.
fn is_probable_html(body: &str) -> bool {
    let head: String = body.chars().take(512).collect();
    let head = head.trim_start().to_ascii_lowercase();
    head.starts_with("<!doctype") || head.starts_with("<html") || head.starts_with("<head")
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
    #[serde(default)]
    finish_reason: Option<String>,
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
    #[serde(default)]
    finish_reason: Option<String>,
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
                        if message_text(&message).is_empty() {
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

/// Take a cancelled prompt's user message back out of the live history.
///
/// The client drops a cancelled prompt from its thread, so the model must not
/// keep answering it. Only the first inference round can cancel with the user
/// message as the last entry; later rounds end on tool or assistant output.
/// A prompt that fails with an error is not taken back: the client keeps it,
/// so the history keeps it too and a model switch carries it over.
pub async fn forget_trailing_user_message(backend: &dyn InferenceBackend) {
    let mut history = backend.history_snapshot().await;
    if history
        .last()
        .is_some_and(|message| message["role"] == "user")
    {
        history.pop();
        backend.restore_history(history).await;
    }
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
        let described = describe_api_error(reqwest::StatusCode::BAD_GATEWAY, "upstream said no");

        assert!(described.contains("502"), "{described}");
        assert!(described.contains("upstream said no"), "{described}");
    }

    /// The issue-#125 screenshots: a gateway's HTML error page pasted into
    /// chat tells the person nothing the status doesn't already say.
    #[test]
    fn an_html_error_page_is_not_pasted_into_the_message() {
        let page = "<!doctype html>\n<html lang=\"en\">\n<head>\n\
                    <title>We're sorry, but something went wrong (500 Internal Server Error)</title>\n\
                    </head>\n<body>…</body>\n</html>";

        let described = describe_api_error(reqwest::StatusCode::GATEWAY_TIMEOUT, page);

        assert_eq!(described, "endpoint returned 504 Gateway Timeout");
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

        drop(history);
        backend
            .record_cancelled_tool_results(vec![ToolResult {
                tool_call_id: "call_9".to_string(),
                content: "duplicate cleanup".to_string(),
            }])
            .await;
        let history = backend.history.lock().await;
        assert_eq!(
            history
                .iter()
                .filter(|message| message["tool_call_id"] == "call_9")
                .count(),
            1
        );
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
    fn carryover_keeps_a_trailing_user_message_from_a_failed_turn() {
        // The client keeps a prompt whose inference errored, so a switch must
        // not drop it (issue #124).
        let carried = carryover_history(vec![
            serde_json::json!({ "role": "system", "content": "prompt" }),
            serde_json::json!({ "role": "user", "content": "completed question" }),
            serde_json::json!({ "role": "assistant", "content": "completed answer" }),
            serde_json::json!({ "role": "user", "content": "failed question" }),
        ]);

        assert_eq!(carried.len(), 3, "{carried:#?}");
        assert_eq!(carried.last().unwrap()["content"], "failed question");
    }

    #[tokio::test]
    async fn forget_trailing_user_message_drops_only_a_cancelled_prompt() {
        let backend = OpenAiBackend::new("http://localhost", "", "m", None);
        backend
            .restore_history(vec![
                serde_json::json!({ "role": "user", "content": "completed question" }),
                serde_json::json!({ "role": "assistant", "content": "completed answer" }),
                serde_json::json!({ "role": "user", "content": "cancelled question" }),
            ])
            .await;

        forget_trailing_user_message(&backend).await;

        let history = backend.history_snapshot().await;
        assert_eq!(history.len(), 2, "{history:#?}");
        assert_eq!(history[1]["content"], "completed answer");

        forget_trailing_user_message(&backend).await;
        assert_eq!(backend.history_snapshot().await.len(), 2);
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

    fn user_with_image(text: &str, payload_chars: usize) -> serde_json::Value {
        serde_json::json!({
            "role": "user",
            "content": [
                { "type": "text", "text": text },
                { "type": "image_url", "image_url": {
                    "url": format!("data:image/png;base64,{}", "A".repeat(payload_chars)),
                }},
            ],
        })
    }

    #[test]
    fn user_message_uses_content_parts_only_when_there_is_an_image() {
        assert_eq!(
            user_message("hello", &[]),
            serde_json::json!({ "role": "user", "content": "hello" })
        );

        let image = ImageInput {
            mime_type: "image/png".to_string(),
            data: "AAAA".to_string(),
        };
        assert_eq!(
            user_message("what is this?", std::slice::from_ref(&image)),
            serde_json::json!({
                "role": "user",
                "content": [
                    { "type": "text", "text": "what is this?" },
                    { "type": "image_url", "image_url": { "url": "data:image/png;base64,AAAA" } },
                ],
            })
        );
        // An image with no text sends no empty text part.
        let only_image = user_message("", &[image]);
        assert_eq!(only_image["content"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn a_text_only_model_is_sent_a_note_in_place_of_each_image() {
        let history = vec![
            serde_json::json!({ "role": "system", "content": "prompt" }),
            user_with_image("what is this?", 16),
            serde_json::json!({ "role": "assistant", "content": "a cat" }),
        ];
        let sent = without_images(&history);

        assert_eq!(sent[0], history[0]);
        assert_eq!(sent[2], history[2]);
        assert_eq!(
            sent[1]["content"],
            format!("what is this?\n{IMAGE_OMITTED_NOTE}")
        );
        // History keeps the image for a model that can read it later.
        assert_eq!(message_image_count(&history[1]), 1);
    }

    #[test]
    fn a_remote_backend_reads_images_by_model_id() {
        let own_endpoint = OpenAiBackend::new("http://localhost", "", "gpt-4o-mini", None);
        assert!(own_endpoint.accepts_images());
        let text_tier = OpenAiBackend::new("http://localhost", "", "onde-nova", None);
        assert!(!text_tier.accepts_images());
        let image_tier = OpenAiBackend::new("http://localhost", "", "onde-large", None);
        assert!(image_tier.accepts_images());
    }

    #[test]
    fn message_text_reads_both_content_shapes() {
        assert_eq!(
            message_text(&serde_json::json!({ "role": "user", "content": "hello" })),
            "hello"
        );
        let with_image = user_with_image("what is this?", 16);
        assert_eq!(message_text(&with_image), "what is this?");
        assert_eq!(message_image_count(&with_image), 1);
        // A tool-call-only assistant message has no content at all.
        let no_content = serde_json::json!({ "role": "assistant", "content": null });
        assert_eq!(message_text(&no_content), "");
        assert_eq!(message_image_count(&no_content), 0);
    }

    #[test]
    fn estimate_tokens_does_not_count_an_image_payload_as_text() {
        // Two megabytes of base64 would read as half a million tokens.
        let history = vec![user_with_image("what is this?", 2_000_000)];
        let estimate = estimate_tokens(&history);
        assert!(
            (IMAGE_TOKEN_ESTIMATE..IMAGE_TOKEN_ESTIMATE + 100).contains(&estimate),
            "{estimate}"
        );
    }

    #[test]
    fn summary_transcript_keeps_the_text_of_a_message_with_an_image() {
        let transcript = transcript_for_summary(&[user_with_image("what is this?", 64)]);
        assert!(transcript.contains("what is this?"), "{transcript}");
        assert!(transcript.contains("[attached an image]"), "{transcript}");
        assert!(!transcript.contains("base64"), "{transcript}");
    }

    #[test]
    fn carryover_keeps_a_user_message_that_carries_an_image() {
        let carried = carryover_history(vec![
            serde_json::json!({ "role": "system", "content": "prompt" }),
            user_with_image("what is this?", 16),
            serde_json::json!({ "role": "assistant", "content": "a cat" }),
        ]);
        assert_eq!(carried.len(), 2);
        assert_eq!(message_image_count(&carried[0]), 1);
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

    #[tokio::test]
    async fn local_restore_replaces_engine_history_and_system_context() {
        let engine = Arc::new(ChatEngine::new());
        let backend = LocalBackend::new(Arc::clone(&engine));
        backend
            .restore_history(vec![
                serde_json::json!({ "role": "system", "content": "thread A context" }),
                serde_json::json!({ "role": "user", "content": "thread A question" }),
            ])
            .await;

        backend
            .restore_history(vec![
                serde_json::json!({ "role": "system", "content": "thread B context" }),
                serde_json::json!({ "role": "user", "content": "thread B question" }),
            ])
            .await;

        let restored = backend.history_snapshot().await;
        assert_eq!(restored.len(), 2, "{restored:#?}");
        assert_eq!(restored[0]["content"], "thread B context");
        assert_eq!(restored[1]["content"], "thread B question");
        assert!(
            restored.iter().all(|message| !message["content"]
                .as_str()
                .unwrap_or_default()
                .contains("thread A")),
            "restore_history must replace, not append to, the local engine: {restored:#?}"
        );

        let error = backend
            .send_message_with_tools("must not run", &[], None)
            .await
            .unwrap_err();
        assert!(error.contains("not ready"), "{error}");
        assert_eq!(backend.history_snapshot().await, restored);
    }

    /// Minimal scripted OpenAI-compatible endpoint: accepts one HTTP request on
    /// a std listener and answers with a fixed non-streaming completion. The
    /// receiver yields the request body the backend actually put on the wire.
    fn spawn_completion_stub(
        summary: &str,
    ) -> (std::net::SocketAddr, std::sync::mpsc::Receiver<String>) {
        spawn_message_stub(serde_json::json!({ "role": "assistant", "content": summary }))
    }

    /// Like [`spawn_completion_stub`], answering with an arbitrary message.
    fn spawn_message_stub(
        message: serde_json::Value,
    ) -> (std::net::SocketAddr, std::sync::mpsc::Receiver<String>) {
        spawn_choice_stub(serde_json::json!({ "message": message }))
    }

    /// Like [`spawn_message_stub`], answering with a whole choice, so a test
    /// can set what sits beside the message.
    fn spawn_choice_stub(
        choice: serde_json::Value,
    ) -> (std::net::SocketAddr, std::sync::mpsc::Receiver<String>) {
        use std::io::{Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (sender, receiver) = std::sync::mpsc::channel();
        let body = serde_json::json!({ "choices": [choice] }).to_string();
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
                        let _ = sender.send(
                            String::from_utf8_lossy(&request[headers_end + 4..]).into_owned(),
                        );
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
        (addr, receiver)
    }

    #[test]
    fn finish_reasons_map_from_the_wire() {
        for reason in [None, Some("stop"), Some("tool_calls"), Some("unheard-of")] {
            assert_eq!(FinishReason::from_wire(reason), FinishReason::Complete);
        }
        assert_eq!(
            FinishReason::from_wire(Some("length")),
            FinishReason::Length
        );
        assert_eq!(
            FinishReason::from_wire(Some("content_filter")),
            FinishReason::ContentFilter
        );
    }

    /// The non-streaming path (headless runs, subagents) reads the reason off
    /// the choice; a reply cut off at the token limit must not look finished.
    #[tokio::test]
    async fn a_truncated_json_reply_reports_length() {
        let (addr, _requests) = spawn_choice_stub(serde_json::json!({
            "message": { "role": "assistant", "content": "The answer is" },
            "finish_reason": "length",
        }));
        let backend =
            OpenAiBackend::new(format!("http://{addr}/v1"), "test-key", "test-model", None);

        let result = backend
            .send_message_with_tools("explain", &[], None)
            .await
            .unwrap();

        assert_eq!(result.text, "The answer is");
        assert_eq!(result.finish, FinishReason::Length);
    }

    /// A structured call can come back with a broken inline block in the same
    /// message. The call runs; the block reaches neither the reply nor the
    /// history, where the model would read back a call that never ran.
    #[tokio::test]
    async fn a_broken_inline_block_beside_a_structured_call_is_dropped() {
        let (addr, _requests) = spawn_message_stub(serde_json::json!({
            "role": "assistant",
            "content": "Checking.<tool_call>run_command CheckStatus=nope</tool_call>",
            "tool_calls": [{
                "id": "call_1",
                "type": "function",
                "function": { "name": "run_command", "arguments": "{\"command\":\"pwd\"}" },
            }],
        }));
        let backend =
            OpenAiBackend::new(format!("http://{addr}/v1"), "test-key", "test-model", None);
        let tools = vec![ToolSpec {
            name: "run_command".to_string(),
            description: "Run a command".to_string(),
            parameters_schema: r#"{"type":"object","properties":{"command":{"type":"string"}}}"#
                .to_string(),
        }];

        let result = backend
            .send_message_with_tools("where am I", &tools, None)
            .await
            .unwrap();

        assert_eq!(result.text, "Checking.");
        assert_eq!(result.tool_calls.len(), 1);
        assert_eq!(result.tool_calls[0].id, "call_1");
        let history = backend.history_snapshot().await;
        let reply = history.last().unwrap();
        assert_eq!(reply["content"], "Checking.");
        assert_eq!(reply["tool_calls"][0]["id"], "call_1");
    }

    /// A structured call plus a well-formed inline block: both run, and the
    /// inline markup leaves the reply and history, as on the streaming path.
    #[tokio::test]
    async fn a_well_formed_inline_block_beside_a_structured_call_also_runs() {
        let (addr, _requests) = spawn_message_stub(serde_json::json!({
            "role": "assistant",
            "content": "Checking.<tool_call>run_command<arg_key>command</arg_key><arg_value>ls</arg_value></tool_call>",
            "tool_calls": [{
                "id": "call_1",
                "type": "function",
                "function": { "name": "run_command", "arguments": "{\"command\":\"pwd\"}" },
            }],
        }));
        let backend =
            OpenAiBackend::new(format!("http://{addr}/v1"), "test-key", "test-model", None);
        let tools = vec![ToolSpec {
            name: "run_command".to_string(),
            description: "Run a command".to_string(),
            parameters_schema: r#"{"type":"object","properties":{"command":{"type":"string"}}}"#
                .to_string(),
        }];

        let result = backend
            .send_message_with_tools("where am I", &tools, None)
            .await
            .unwrap();

        assert_eq!(result.text, "Checking.");
        let ids: Vec<&str> = result.tool_calls.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["call_1", "call_recovered_1"]);
        assert_eq!(result.tool_calls[1].arguments, r#"{"command":"ls"}"#);
        let history = backend.history_snapshot().await;
        let reply = history.last().unwrap();
        assert_eq!(reply["content"], "Checking.");
        assert_eq!(reply["tool_calls"].as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn compact_history_rebuilds_system_summary_and_tail() {
        let (addr, _requests) = spawn_completion_stub("We refactored backend.rs; tests pass.");
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

    /// Compacting a tool-heavy session must not put tool shapes on the wire.
    /// The summarization round offers no `tools`, and endpoints reject tool
    /// calls and tool results that arrive without a schema — which used to make
    /// compaction fail forever in any session that had run a single tool.
    #[tokio::test]
    async fn compact_history_sends_no_tool_artifacts() {
        let (addr, requests) = spawn_completion_stub("Ran git status on main.");
        let backend = OpenAiBackend::new(
            format!("http://{addr}/v1"),
            "test-key",
            "test-model",
            Some("be helpful".into()),
        );
        {
            let mut history = backend.history.lock().await;
            history.push(serde_json::json!({ "role": "user", "content": "check the repo" }));
            history.push(serde_json::json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": { "name": "run_command", "arguments": "{\"command\":\"git status\"}" },
                }],
            }));
            history.push(serde_json::json!({
                "role": "tool",
                "tool_call_id": "call_1",
                "content": "on branch main",
            }));
        }

        backend.compact_history(2).await.unwrap();

        let body: serde_json::Value =
            serde_json::from_str(&requests.recv().unwrap()).expect("request body is JSON");
        assert!(
            body.get("tools").is_none(),
            "summarization offers no tools: {body}"
        );
        for message in body["messages"].as_array().unwrap() {
            assert!(
                message.get("tool_calls").is_none(),
                "no tool_calls may be sent without a schema: {message}"
            );
            assert_ne!(
                message["role"], "tool",
                "no tool results may be sent without a schema: {message}"
            );
        }

        // The tool round still has to survive into the summary request as prose,
        // or the summary loses the work the session actually did.
        let transcript = body["messages"].as_array().unwrap().last().unwrap()["content"]
            .as_str()
            .unwrap();
        assert!(transcript.contains("called run_command({\"command\":\"git status\"})"));
        assert!(transcript.contains("tool result: on branch main"));
    }

    /// Compaction only fails when even the truncation fallback cannot fit the
    /// history in the budget — here the newest message alone is bigger than
    /// the whole window.
    #[tokio::test]
    async fn compact_history_failure_leaves_history_intact() {
        // No listener at this address, so summarization fails; the system
        // prompt alone is bigger than the window, so even the minimal
        // fallback rebuild cannot fit — the genuinely unrecoverable case.
        let backend = OpenAiBackend::new(
            "http://127.0.0.1:9",
            "",
            "test-model",
            Some("s".repeat(DEFAULT_CONTEXT_TOKEN_BUDGET * 4 + 1000)),
        );
        backend
            .history
            .lock()
            .await
            .push(serde_json::json!({ "role": "user", "content": "hello" }));
        let before = backend.history_snapshot().await;

        assert!(backend.compact_history(2).await.is_err());
        assert_eq!(backend.history_snapshot().await, before);
    }

    /// The issue-#125 path: the summarization endpoint is down, and the
    /// session must still shrink rather than die with "start a new thread".
    #[tokio::test]
    async fn failed_summarization_falls_back_to_truncation() {
        // Nothing listens here, so the summarization round always fails.
        let backend =
            OpenAiBackend::new("http://127.0.0.1:9", "", "test-model", Some("sys".into()));
        {
            let mut history = backend.history.lock().await;
            for i in 0..20 {
                history.push(serde_json::json!({
                    "role": "user",
                    "content": format!("message {i}: {}", "padding ".repeat(200)),
                }));
            }
        }

        backend.compact_history(4).await.unwrap();

        let history = backend.history_snapshot().await;
        assert_eq!(history[0]["role"], "system", "{history:?}");
        assert_eq!(history[1]["content"], TRUNCATION_PLACEHOLDER, "{history:?}");
        // The newest messages survive; the oldest were dropped to fit.
        assert_eq!(
            history.last().unwrap()["content"]
                .as_str()
                .unwrap()
                .split(':')
                .next()
                .unwrap(),
            "message 19",
            "{history:?}"
        );
        assert!(
            estimate_tokens(&history) <= DEFAULT_CONTEXT_TOKEN_BUDGET,
            "fallback result must fit the budget: ~{} tokens",
            estimate_tokens(&history)
        );
    }

    /// A history small enough to begin with keeps its whole tail: the
    /// fallback drops nothing beyond what `keep_last` already folded away.
    #[tokio::test]
    async fn truncation_fallback_keeps_the_newest_messages_that_fit() {
        let system = vec![serde_json::json!({ "role": "system", "content": "sys" })];
        let tail: Vec<serde_json::Value> = (0..4)
            .map(|i| serde_json::json!({ "role": "user", "content": format!("m{i}") }))
            .collect();

        let rebuilt = truncate_history_to_budget(&system, tail).unwrap();

        assert_eq!(rebuilt[0]["role"], "system");
        assert_eq!(rebuilt[1]["content"], TRUNCATION_PLACEHOLDER);
        assert_eq!(rebuilt.len(), 6, "{rebuilt:?}");
        assert_eq!(rebuilt[2]["content"], "m0");
        assert_eq!(rebuilt[5]["content"], "m3");
    }

    /// A multi-system-message session keeps every leading system message in
    /// the fallback, exactly like the summarization-success path.
    #[test]
    fn truncation_fallback_keeps_all_leading_system_messages() {
        let system = vec![
            serde_json::json!({ "role": "system", "content": "seed prompt" }),
            serde_json::json!({ "role": "system", "content": "session context" }),
        ];
        let tail = vec![serde_json::json!({ "role": "user", "content": "hi" })];

        let rebuilt = truncate_history_to_budget(&system, tail).unwrap();

        assert_eq!(
            rebuilt[0]["content"], "seed prompt",
            "first system message kept: {rebuilt:?}"
        );
        assert_eq!(
            rebuilt[1]["content"], "session context",
            "second system message kept: {rebuilt:?}"
        );
    }

    #[test]
    fn truncation_fallback_drops_orphaned_tool_results_with_their_call() {
        // A caller that split mid-round hands in a tail starting with tool
        // results whose assistant call message already fell outside the kept
        // window; those orphans must go before anything else, or strict
        // endpoints reject the next request.
        let tail = vec![
            serde_json::json!({ "role": "tool", "tool_call_id": "call_1", "content": "ok" }),
            serde_json::json!({ "role": "user", "content": "thanks" }),
            serde_json::json!({ "role": "assistant", "content": "done" }),
        ];

        let rebuilt = truncate_history_to_budget(&[], tail).unwrap();

        let roles: Vec<&str> = rebuilt.iter().filter_map(|m| m["role"].as_str()).collect();
        assert_eq!(
            roles,
            ["user", "user", "assistant"],
            "placeholder + the surviving exchange, orphan gone: {rebuilt:?}"
        );
    }

    /// Review follow-up: a tail that drains to empty (one oversized tool
    /// round) must still test the minimal rebuild — system + placeholder fits,
    /// so the session is recoverable, not "unfixable".
    #[test]
    fn truncation_fallback_recovers_when_the_whole_tail_was_one_oversized_round() {
        let system = vec![serde_json::json!({ "role": "system", "content": "sys" })];
        let tail = vec![
            serde_json::json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call_1", "type": "function",
                    "function": { "name": "read_file", "arguments": "{}" },
                }],
            }),
            serde_json::json!({
                "role": "tool",
                "tool_call_id": "call_1",
                "content": "y".repeat(DEFAULT_CONTEXT_TOKEN_BUDGET * 4 + 1000),
            }),
        ];

        let rebuilt = truncate_history_to_budget(&system, tail).unwrap();

        assert_eq!(rebuilt.len(), 2, "{rebuilt:?}");
        assert_eq!(rebuilt[0]["role"], "system");
        assert_eq!(rebuilt[1]["content"], TRUNCATION_PLACEHOLDER);
    }

    #[test]
    fn truncation_fallback_reports_when_nothing_fits() {
        // Even the minimal rebuild (system + placeholder, no conversation)
        // overflows: the system messages alone are bigger than the window.
        let system = vec![serde_json::json!({
            "role": "system",
            "content": "s".repeat(DEFAULT_CONTEXT_TOKEN_BUDGET * 4 + 1000),
        })];

        assert!(truncate_history_to_budget(&system, Vec::new()).is_none());
    }

    /// Review follow-up: the local fallback recovers from a newest message
    /// that overflows the window on its own, the same way the remote one does.
    #[test]
    fn local_truncation_fallback_drops_an_oversized_final_message() {
        let system = vec![ChatMessage::system("sys")];
        let tail = vec![
            ChatMessage::user("hi"),
            ChatMessage::user("y".repeat(DEFAULT_CONTEXT_TOKEN_BUDGET * 4 + 1000)),
        ];

        let rebuilt = truncate_chat_messages_to_budget(system, tail).unwrap();

        assert_eq!(rebuilt.len(), 2, "{rebuilt:?}");
        assert_eq!(rebuilt[0].content, "sys");
        assert_eq!(rebuilt[1].content, TRUNCATION_PLACEHOLDER);
    }

    #[test]
    fn local_truncation_fallback_keeps_the_newest_messages_that_fit() {
        let big = DEFAULT_CONTEXT_TOKEN_BUDGET * 4 / 2;
        let tail = vec![
            ChatMessage::user("a".repeat(big)),
            ChatMessage::user("b".repeat(big)),
            ChatMessage::user("newest"),
        ];

        let rebuilt = truncate_chat_messages_to_budget(Vec::new(), tail).unwrap();

        assert_eq!(rebuilt.len(), 3, "oldest dropped, rest kept");
        assert_eq!(rebuilt[0].content, TRUNCATION_PLACEHOLDER);
        assert!(rebuilt[1].content.starts_with('b'));
        assert_eq!(rebuilt[2].content, "newest");
    }

    #[test]
    fn local_truncation_fallback_reports_when_the_system_messages_overflow() {
        let system = vec![ChatMessage::system(
            "s".repeat(DEFAULT_CONTEXT_TOKEN_BUDGET * 4 + 1000),
        )];

        assert!(truncate_chat_messages_to_budget(system, Vec::new()).is_none());
    }

    /// Review follow-up: head and tail share the budget, so the trimmed
    /// transcript stays near the cap instead of reaching twice it.
    #[test]
    fn truncate_transcript_middle_stays_within_the_cap() {
        let lines: Vec<String> = (0..2_000)
            .map(|i| format!("user: message {i} with some padding text"))
            .collect();
        let transcript = lines.join("\n");

        let trimmed = truncate_transcript_middle(&transcript, 200); // ~800 chars

        assert!(trimmed.contains("message 0 "), "opening kept: {trimmed}");
        assert!(trimmed.contains("message 1999"), "closing kept: {trimmed}");
        assert!(
            trimmed.len() <= 800 + 80,
            "cap plus the omission marker, got {} chars",
            trimmed.len()
        );
    }

    #[test]
    fn truncate_transcript_middle_keeps_short_transcripts_verbatim() {
        let transcript = "user: hello\n\nassistant: hi";

        assert_eq!(truncate_transcript_middle(transcript, 1_000), transcript);
    }

    /// Review follow-up: a huge final message with no newline in the tail
    /// half must not be silently dropped — the closing exchange is what the
    /// summary most needs. The head gives up its share instead.
    #[test]
    fn truncate_transcript_middle_keeps_a_newlineless_final_message() {
        let mut lines: Vec<String> = (0..100).map(|i| format!("user: message {i}")).collect();
        lines.push(format!(
            "user: {}current state: pending review",
            "noise ".repeat(600)
        ));
        let transcript = lines.join("\n");

        let trimmed = truncate_transcript_middle(&transcript, 400); // ~1600 chars

        assert!(
            trimmed.contains("current state: pending review"),
            "the closing exchange survives: {trimmed}"
        );
        assert!(
            trimmed.contains("older messages omitted"),
            "the gap is marked: {trimmed}"
        );
        assert!(
            !trimmed.contains("message 0"),
            "the head gives up its share to the unbreakable tail: {trimmed}"
        );
    }

    /// Review follow-up: no newline anywhere in reach — keep the newest
    /// budget, not the oldest (which would invert the contract while the
    /// placeholder claims older messages were dropped).
    #[test]
    fn truncate_transcript_middle_without_any_newline_keeps_the_newest() {
        let transcript = format!("old decisions {} current state", "x".repeat(4_000));

        let trimmed = truncate_transcript_middle(&transcript, 200); // ~800 chars

        assert!(
            trimmed.contains("current state"),
            "the newest content survives: {trimmed}"
        );
        assert!(
            !trimmed.contains("old decisions"),
            "the oldest goes when only one side can survive: {trimmed}"
        );
    }

    #[test]
    fn truncate_transcript_middle_cuts_the_middle_not_the_ends() {
        let mut lines: Vec<String> = Vec::new();
        for i in 0..200 {
            lines.push(format!("user: message {i} with some padding text"));
        }
        let transcript = lines.join("\n");

        let trimmed = truncate_transcript_middle(&transcript, 200); // ~800 chars

        assert!(trimmed.contains("message 0"), "opening kept: {trimmed}");
        assert!(trimmed.contains("message 199"), "closing kept: {trimmed}");
        assert!(
            trimmed.contains("older messages omitted"),
            "gap is marked: {trimmed}"
        );
        assert!(
            trimmed.len() <= transcript.len(),
            "never grows the transcript"
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
