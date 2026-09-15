//! Recovery for tool calls a model emits as literal text instead of the
//! endpoint's structured `tool_calls` field.
//!
//! Several open-weight families siGit Code can drive are fine-tuned on chat
//! templates that render a tool call inside assistant content. A serving stack
//! is supposed to parse that back into the OpenAI-shaped `tool_calls` field
//! before it reaches us. When it doesn't, the raw tag arrives as ordinary
//! content.
//!
//! Nothing then executes it. Worse, `consume_stream` forwards content to the
//! UI as it arrives, so the tag is rendered verbatim in the editor and the
//! turn ends with no tool calls, which reads to the agent loop as "the model
//! chose to answer in prose". Whatever it was actually trying to do is
//! dropped.
//!
//! This lives here rather than only in the gateway because siGit Code talks to
//! more than one kind of endpoint: siGit Code Cloud, an arbitrary
//! OpenAI-compatible base URL from `providers.toml` or `OPENAI_BASE_URL`, and
//! on-device models. A fix in any single upstream leaves the others exposed.
//!
//! Only well-formed shapes are recovered. A block that doesn't match is left
//! in the text untouched, deliberately. Reissuing a `run_command` is cheap,
//! but guessing wrong at a half-parsed `edit_file` would write the wrong
//! change to a file, so a malformed block stays visible rather than being
//! silently misinterpreted.

use std::collections::HashMap;

use crate::backend::ToolSpec;

const XML_OPEN_TAG: &str = "<tool_call>";
const XML_CLOSE_TAG: &str = "</tool_call>";

const K3_TOOLS_OPEN: &str = "<|open|>tools<|sep|>";
const K3_TOOLS_CLOSE: &str = "<|close|>tools<|sep|>";
const K3_CALL_OPEN: &str = "<|open|>call";
const K3_CALL_CLOSE: &str = "<|close|>call<|sep|>";
const K3_ARGUMENT_OPEN: &str = "<|open|>argument";
const K3_ARGUMENT_CLOSE: &str = "<|close|>argument<|sep|>";
const K3_RESPONSE_OPEN: &str = "<|open|>response<|sep|>";
const K3_RESPONSE_CLOSE: &str = "<|close|>response<|sep|>";
const K3_THINK_OPEN: &str = "<|open|>think<|sep|>";
const K3_THINK_CLOSE: &str = "<|close|>think<|sep|>";
const K3_SEP: &str = "<|sep|>";

const START_MARKERS: &[&str] = &[XML_OPEN_TAG, K3_TOOLS_OPEN, K3_RESPONSE_OPEN, K3_THINK_OPEN];

/// One tool call recovered from inline text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recovered {
    pub name: String,
    /// Arguments as a JSON-encoded string, matching `ToolCall::arguments`.
    pub arguments: String,
}

/// Scan a complete text blob for inline tool-call blocks. Returns the text
/// with recovered blocks removed, plus the calls recovered from them. Blocks
/// that don't parse cleanly, and unterminated trailing tags, are left in the
/// returned text untouched.
pub fn extract(text: &str, tools: &[ToolSpec]) -> (String, Vec<Recovered>) {
    let mut scanner = StreamScanner::new(tools);
    let mut out = String::with_capacity(text.len());
    let mut calls = Vec::new();

    for event in scanner.push(text) {
        match event {
            ScanEvent::Text(text) => out.push_str(&text),
            ScanEvent::ToolCall(call) => calls.push(call),
        }
    }
    if let Some(rest) = scanner.take_pending() {
        out.push_str(&rest);
    }

    (out, calls)
}

/// Parse one legacy XML block's inner text: an offered tool name, then zero or
/// more `<arg_key>K</arg_key><arg_value>V</arg_value>` pairs. The malformed
/// opening marker observed from GLM is accepted only for the first argument;
/// later arguments stay strict so text that merely resembles a call cannot be
/// recovered as one.
fn parse_xml_block(inner: &str, tools: &[ToolSpec]) -> Option<Recovered> {
    let key_idx = inner.find("<arg_key>");
    let malformed_key_idx = inner.find(XML_OPEN_TAG);
    let check_status_name = key_idx.and_then(|idx| parse_check_status_preamble(&inner[..idx]));
    let (name, mut rest, malformed_first_key) =
        if let (Some(idx), Some(name)) = (key_idx, check_status_name) {
            (name, &inner[idx..], false)
        } else {
            match (key_idx, malformed_key_idx) {
                (Some(key_idx), Some(malformed_key_idx)) if malformed_key_idx < key_idx => (
                    inner[..malformed_key_idx].trim(),
                    &inner[malformed_key_idx..],
                    true,
                ),
                (Some(idx), _) => (inner[..idx].trim(), &inner[idx..], false),
                (None, Some(idx)) => (inner[..idx].trim(), &inner[idx..], true),
                (None, None) => (inner.trim(), "", false),
            }
        };
    if name.is_empty() || name.contains(['<', '>']) || !offered_tool(tools, name) {
        return None;
    }

    let mut args = serde_json::Map::new();
    let mut first_key = true;
    while !rest.is_empty() {
        // The GLM alias is permitted exactly once, at the first key. A later
        // `<tool_call>` must reject the complete block rather than letting a
        // partial call escape recovery.
        if !first_key && rest.starts_with(XML_OPEN_TAG) {
            return None;
        }
        let key_open = if first_key && malformed_first_key {
            XML_OPEN_TAG
        } else {
            "<arg_key>"
        };
        let (key, after_key) = split_once(rest.strip_prefix(key_open)?, "</arg_key>")?;
        let (value, after_value) =
            split_once(after_key.strip_prefix("<arg_value>")?, "</arg_value>")?;
        let key = key.trim();
        if key.is_empty() {
            return None;
        }
        args.insert(
            key.to_string(),
            coerce(value, declared_type(tools, name, key).as_deref()),
        );
        rest = after_value;
        first_key = false;
    }

    Some(Recovered {
        name: name.to_string(),
        arguments: serde_json::Value::Object(args).to_string(),
    })
}

/// Some GLM-family responses prefix the real arguments with generation-control
/// metadata shaped like `tool_name CheckStatus=value</arg_value>`. It is not a
/// tool argument: accept that exact bounded preamble and discard it, while
/// leaving every other malformed prefix untouched.
fn parse_check_status_preamble(prefix: &str) -> Option<&str> {
    let (name, status) = prefix.trim().split_once(" CheckStatus=")?;
    let status = status.strip_suffix("</arg_value>")?;
    if name.is_empty()
        || status.is_empty()
        || !status
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
    {
        return None;
    }
    Some(name)
}

fn parse_k3_tools_block(inner: &str, tools: &[ToolSpec]) -> Option<Vec<Recovered>> {
    let mut calls = Vec::new();
    let mut rest = inner;

    while !rest.trim().is_empty() {
        rest = rest.trim_start();
        let header = rest.strip_prefix(K3_CALL_OPEN)?;
        let (attr_text, after_header) = split_once(header, K3_SEP)?;
        let attrs = parse_k3_attributes(attr_text)?;
        let name = attrs.get("tool")?;
        if name.is_empty() || !offered_tool(tools, name) {
            return None;
        }

        let (call_inner, after_call) = split_once(after_header, K3_CALL_CLOSE)?;
        let arguments = parse_k3_arguments(call_inner, tools, name)?;
        calls.push(Recovered {
            name: name.to_string(),
            arguments: serde_json::Value::Object(arguments).to_string(),
        });
        rest = after_call;
    }

    Some(calls)
}

fn parse_k3_arguments(
    mut rest: &str,
    tools: &[ToolSpec],
    tool_name: &str,
) -> Option<serde_json::Map<String, serde_json::Value>> {
    let mut args = serde_json::Map::new();

    while !rest.trim().is_empty() {
        rest = rest.trim_start();
        let header = rest.strip_prefix(K3_ARGUMENT_OPEN)?;
        let (attr_text, after_header) = split_once(header, K3_SEP)?;
        let attrs = parse_k3_attributes(attr_text)?;
        let key = attrs.get("key")?.trim();
        if key.is_empty() {
            return None;
        }
        // K3 normally emits `type` for every argument. If an endpoint omits
        // it, retain the legacy recovery path's schema-based typing instead of
        // turning e.g. an integer `task_id` into a JSON string.
        let schema_type = declared_type(tools, tool_name, key);
        let value_type = attrs
            .get("type")
            .map(String::as_str)
            .or(schema_type.as_deref())
            .unwrap_or("string");

        let (raw_value, after_argument) = split_once(after_header, K3_ARGUMENT_CLOSE)?;
        let value = if value_type == "string" {
            serde_json::Value::String(raw_value.to_string())
        } else {
            serde_json::from_str(raw_value).ok()?
        };
        args.insert(key.to_string(), value);
        rest = after_argument;
    }

    Some(args)
}

fn parse_k3_attributes(mut text: &str) -> Option<HashMap<String, String>> {
    let mut attrs = HashMap::new();
    text = text.trim();

    while !text.is_empty() {
        let eq_idx = text.find('=')?;
        let key = text[..eq_idx].trim();
        if key.is_empty()
            || !key
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
        {
            return None;
        }

        let mut value_part = text[eq_idx + 1..].trim_start();
        value_part = value_part.strip_prefix('"')?;
        let end_idx = value_part.find('"')?;
        let raw_value = &value_part[..end_idx];
        attrs.insert(key.to_string(), decode_k3_attribute(raw_value));

        text = value_part[end_idx + 1..].trim_start();
    }

    Some(attrs)
}

fn decode_k3_attribute(value: &str) -> String {
    // K3's encoder escapes only `&` and `"` in attributes. These replacements
    // deliberately mirror that format rather than implementing general HTML
    // entity decoding (which could alter literal argument names).
    value.replace("&quot;", "\"").replace("&amp;", "&")
}

fn split_once<'a>(s: &'a str, delim: &str) -> Option<(&'a str, &'a str)> {
    let idx = s.find(delim)?;
    Some((&s[..idx], &s[idx + delim.len()..]))
}

fn offered_tool(tools: &[ToolSpec], tool_name: &str) -> bool {
    tools.iter().any(|spec| spec.name == tool_name)
}

/// The JSON Schema `type` declared for `tool_name`'s `key` parameter, e.g.
/// `"integer"` for `command_output`'s `task_id`. [`ToolSpec`] carries the
/// schema as an unparsed string, so this parses it per lookup; the argument
/// counts involved are tiny and this only runs on the recovery path.
fn declared_type(tools: &[ToolSpec], tool_name: &str, key: &str) -> Option<String> {
    let spec = tools.iter().find(|spec| spec.name == tool_name)?;
    let schema: serde_json::Value = serde_json::from_str(&spec.parameters_schema).ok()?;
    Some(
        schema
            .get("properties")?
            .get(key)?
            .get("type")?
            .as_str()?
            .to_string(),
    )
}

/// Convert a raw legacy text value to JSON per its declared schema type.
/// Falls back to a plain string for `"string"`, an unknown type, or a value
/// that doesn't parse as what it claims to be.
fn coerce(raw: &str, declared: Option<&str>) -> serde_json::Value {
    match declared {
        Some("integer") => raw
            .trim()
            .parse::<i64>()
            .map(serde_json::Value::from)
            .unwrap_or_else(|_| serde_json::Value::String(raw.to_string())),
        Some("number") => raw
            .trim()
            .parse::<f64>()
            .ok()
            .and_then(serde_json::Number::from_f64)
            .map(serde_json::Value::Number)
            .unwrap_or_else(|| serde_json::Value::String(raw.to_string())),
        Some("boolean") => match raw.trim() {
            "true" => serde_json::Value::Bool(true),
            "false" => serde_json::Value::Bool(false),
            _ => serde_json::Value::String(raw.to_string()),
        },
        _ => serde_json::Value::String(raw.to_string()),
    }
}

/// One event from incrementally scanning a stream of content deltas.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScanEvent {
    /// Plain text, safe to forward to the UI.
    Text(String),
    /// A tool call recovered from a completed block.
    ToolCall(Recovered),
}

/// Incremental scanner for a live delta stream.
///
/// Holds back only the text that could still turn into a known inline protocol
/// marker, so ordinary answers still reach the UI token by token. Buffering
/// starts once an opening marker appears, and lasts only until the matching
/// close arrives.
pub struct StreamScanner<'a> {
    tools: &'a [ToolSpec],
    pending: String,
}

impl<'a> StreamScanner<'a> {
    pub fn new(tools: &'a [ToolSpec]) -> Self {
        Self {
            tools,
            pending: String::new(),
        }
    }

    /// Feed the next content delta. Returns events to emit now, in order; may
    /// be empty if the chunk was absorbed into a tag that hasn't closed yet.
    pub fn push(&mut self, chunk: &str) -> Vec<ScanEvent> {
        self.pending.push_str(chunk);
        self.drain()
    }

    /// Take whatever is still held back, e.g. a `<tool_call` or Kimi marker
    /// prefix that never closed. Text we never saw the end of can't be
    /// interpreted safely, so the honest thing is to hand it back as-is.
    pub fn take_pending(&mut self) -> Option<String> {
        if self.pending.is_empty() {
            None
        } else {
            Some(std::mem::take(&mut self.pending))
        }
    }

    fn drain(&mut self) -> Vec<ScanEvent> {
        let mut events = Vec::new();
        loop {
            let Some((open_idx, marker)) = find_next_marker(&self.pending) else {
                let keep = max_partial_prefix_len(&self.pending, START_MARKERS);
                let flush_len = self.pending.len() - keep;
                if flush_len > 0 {
                    events.push(ScanEvent::Text(
                        self.pending.drain(..flush_len).collect::<String>(),
                    ));
                }
                break;
            };

            if open_idx > 0 {
                events.push(ScanEvent::Text(
                    self.pending.drain(..open_idx).collect::<String>(),
                ));
            }

            match marker {
                XML_OPEN_TAG => {
                    let Some(event) = self.drain_xml() else {
                        break;
                    };
                    events.push(event);
                }
                K3_TOOLS_OPEN => match self.drain_k3_tools() {
                    Some(mut recovered) => events.append(&mut recovered),
                    None => break,
                },
                K3_RESPONSE_OPEN => {
                    let Some(mut unwrapped) =
                        self.drain_wrapped_text(K3_RESPONSE_OPEN, K3_RESPONSE_CLOSE, true)
                    else {
                        break;
                    };
                    events.append(&mut unwrapped);
                }
                K3_THINK_OPEN => {
                    let Some(mut unwrapped) =
                        self.drain_wrapped_text(K3_THINK_OPEN, K3_THINK_CLOSE, false)
                    else {
                        break;
                    };
                    events.append(&mut unwrapped);
                }
                _ => unreachable!("unknown inline marker"),
            }
        }
        events
    }

    fn drain_xml(&mut self) -> Option<ScanEvent> {
        let close_rel = self.pending[XML_OPEN_TAG.len()..].find(XML_CLOSE_TAG)?;
        let close_idx = XML_OPEN_TAG.len() + close_rel;
        let block: String = self
            .pending
            .drain(..close_idx + XML_CLOSE_TAG.len())
            .collect();
        let inner = &block[XML_OPEN_TAG.len()..block.len() - XML_CLOSE_TAG.len()];
        Some(match parse_xml_block(inner, self.tools) {
            Some(call) => ScanEvent::ToolCall(call),
            None => ScanEvent::Text(block),
        })
    }

    fn drain_k3_tools(&mut self) -> Option<Vec<ScanEvent>> {
        let close_rel = self.pending[K3_TOOLS_OPEN.len()..].find(K3_TOOLS_CLOSE)?;
        let close_idx = K3_TOOLS_OPEN.len() + close_rel;
        let block: String = self
            .pending
            .drain(..close_idx + K3_TOOLS_CLOSE.len())
            .collect();
        let inner = &block[K3_TOOLS_OPEN.len()..block.len() - K3_TOOLS_CLOSE.len()];

        Some(match parse_k3_tools_block(inner, self.tools) {
            Some(calls) => calls.into_iter().map(ScanEvent::ToolCall).collect(),
            None => vec![ScanEvent::Text(block)],
        })
    }

    fn drain_wrapped_text(
        &mut self,
        open: &str,
        close: &str,
        emit_inner: bool,
    ) -> Option<Vec<ScanEvent>> {
        let after_open = &self.pending[open.len()..];
        let close_rel = match after_open.find(close) {
            Some(close_rel) => close_rel,
            None => {
                // A malformed response/think wrapper must not swallow a later
                // valid tool block forever. Preserve the malformed prefix as
                // text, leave the next marker pending, and resume scanning.
                let (next_rel, _) = find_next_marker(after_open)?;
                return Some(vec![ScanEvent::Text(
                    self.pending
                        .drain(..open.len() + next_rel)
                        .collect::<String>(),
                )]);
            }
        };
        let close_idx = open.len() + close_rel;
        let block: String = self.pending.drain(..close_idx + close.len()).collect();

        if emit_inner {
            let inner = &block[open.len()..block.len() - close.len()];
            Some(scan_complete_text(inner, self.tools))
        } else {
            Some(Vec::new())
        }
    }
}

/// Scan text from a wrapper whose closing marker has already arrived. This
/// preserves event order when a response body itself contains an inline block.
fn scan_complete_text(text: &str, tools: &[ToolSpec]) -> Vec<ScanEvent> {
    let mut scanner = StreamScanner::new(tools);
    let mut events = scanner.push(text);
    if let Some(rest) = scanner.take_pending() {
        events.push(ScanEvent::Text(rest));
    }
    events
}

fn find_next_marker<'a>(text: &str) -> Option<(usize, &'a str)> {
    START_MARKERS
        .iter()
        .filter_map(|marker| text.find(marker).map(|idx| (idx, *marker)))
        .min_by_key(|(idx, _)| *idx)
}

/// Length of the longest suffix of `buf` that is a prefix of any known opening
/// marker. Lets a split opening tag be caught without delaying text that can't
/// possibly be part of one.
fn max_partial_prefix_len(buf: &str, needles: &[&str]) -> usize {
    needles
        .iter()
        .map(|needle| partial_prefix_len(buf, needle))
        .max()
        .unwrap_or(0)
}

fn partial_prefix_len(buf: &str, needle: &str) -> usize {
    let max = buf.len().min(needle.len() - 1);
    for len in (1..=max).rev() {
        let start = buf.len() - len;
        if buf.is_char_boundary(start) && needle.starts_with(&buf[start..]) {
            return len;
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(name: &str, schema: serde_json::Value) -> ToolSpec {
        ToolSpec {
            name: name.to_string(),
            description: String::new(),
            parameters_schema: schema.to_string(),
        }
    }

    fn command_output_spec() -> ToolSpec {
        spec(
            "command_output",
            serde_json::json!({
                "type": "object",
                "properties": { "task_id": { "type": "integer" } }
            }),
        )
    }

    fn run_command_spec() -> ToolSpec {
        spec(
            "run_command",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string" },
                    "cwd": { "type": "string" },
                    "run_in_background": { "type": "boolean" }
                }
            }),
        )
    }

    #[test]
    fn an_integer_argument_is_recovered_as_a_json_number() {
        let tools = vec![command_output_spec()];
        let (text, calls) = extract(
            "Checking.<tool_call>command_output<arg_key>task_id</arg_key><arg_value>2</arg_value></tool_call>",
            &tools,
        );
        assert_eq!(text, "Checking.");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "command_output");
        let args: serde_json::Value = serde_json::from_str(&calls[0].arguments).unwrap();
        assert_eq!(args["task_id"], 2);
        assert!(args["task_id"].is_number());
    }

    #[test]
    fn recovers_multiple_string_arguments_in_order() {
        let tools = vec![run_command_spec()];
        let (text, calls) = extract(
            "<tool_call>run_command<arg_key>command</arg_key><arg_value>cargo test --locked 2>&1 | tail -20</arg_value><arg_key>cwd</arg_key><arg_value>/Users/x/sigit</arg_value><arg_key>run_in_background</arg_key><arg_value>true</arg_value></tool_call>",
            &tools,
        );
        assert_eq!(text, "");
        let args: serde_json::Value = serde_json::from_str(&calls[0].arguments).unwrap();
        assert_eq!(args["command"], "cargo test --locked 2>&1 | tail -20");
        assert_eq!(args["cwd"], "/Users/x/sigit");
        assert_eq!(args["run_in_background"], true);
    }

    #[test]
    fn recovers_glm_call_with_a_malformed_first_argument_marker() {
        let tools = vec![run_command_spec()];
        let (text, calls) = extract(
            "<tool_call>run_command<tool_call>command</arg_key><arg_value>pwd</arg_value><arg_key>cwd</arg_key><arg_value>/tmp</arg_value></tool_call>",
            &tools,
        );

        assert_eq!(text, "");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "run_command");
        let args: serde_json::Value = serde_json::from_str(&calls[0].arguments).unwrap();
        assert_eq!(args["command"], "pwd");
        assert_eq!(args["cwd"], "/tmp");
    }

    #[test]
    fn recovers_glm_call_with_check_status_before_real_arguments() {
        let tools = vec![command_output_spec()];
        let (text, calls) = extract(
            "polling <tool_call>command_output CheckStatus=true_or_poll_again_with_different_params</arg_value><arg_key>task_id</arg_key><arg_value>1</arg_value></tool_call>",
            &tools,
        );

        assert_eq!(text, "polling ");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "command_output");
        let args: serde_json::Value = serde_json::from_str(&calls[0].arguments).unwrap();
        assert_eq!(args["task_id"], 1);
        assert!(args["task_id"].is_number());
        assert!(args.get("CheckStatus").is_none());
    }

    #[test]
    fn malformed_check_status_preamble_is_left_as_text() {
        let text = "<tool_call>command_output CheckStatus=keep going</arg_value><arg_key>task_id</arg_key><arg_value>1</arg_value></tool_call>";
        let (out, calls) = extract(text, &[command_output_spec()]);
        assert_eq!(out, text);
        assert!(calls.is_empty());
    }

    #[test]
    fn check_status_after_a_real_argument_is_left_as_text() {
        let text = "<tool_call>command_output<arg_key>task_id</arg_key><arg_value>1</arg_value> CheckStatus=true_or_poll_again_with_different_params</arg_value></tool_call>";
        let (out, calls) = extract(text, &[command_output_spec()]);
        assert_eq!(out, text);
        assert!(calls.is_empty());
    }

    #[test]
    fn check_status_for_an_unknown_tool_is_left_as_text() {
        let text = "<tool_call>not_offered CheckStatus=true_or_poll_again_with_different_params</arg_value><arg_key>task_id</arg_key><arg_value>1</arg_value></tool_call>";
        let (out, calls) = extract(text, &[command_output_spec()]);
        assert_eq!(out, text);
        assert!(calls.is_empty());
    }

    #[test]
    fn unknown_glm_tool_is_left_as_text() {
        let text = "<tool_call>not_offered<tool_call>command</arg_key><arg_value>pwd</arg_value></tool_call>";
        let (out, calls) = extract(text, &[run_command_spec()]);
        assert_eq!(out, text);
        assert!(calls.is_empty());
    }

    #[test]
    fn malformed_glm_marker_is_accepted_only_for_the_first_argument() {
        let text = "<tool_call>run_command<arg_key>command</arg_key><arg_value>pwd</arg_value><tool_call>cwd</arg_key><arg_value>/tmp</arg_value></tool_call>";
        let (out, calls) = extract(text, &[run_command_spec()]);
        assert_eq!(out, text);
        assert!(calls.is_empty());
    }

    #[test]
    fn a_malformed_block_is_left_alone_rather_than_guessed_at() {
        let text = "<tool_call>edit_file<arg_value>new_text</arg_key><arg_value>def x; end</arg_value></tool_call>";
        let (out, calls) = extract(text, &[run_command_spec()]);
        assert_eq!(out, text);
        assert!(calls.is_empty());
    }

    #[test]
    fn an_unterminated_tag_is_left_alone() {
        let text = "working on it <tool_call>run_command<arg_key>command</arg_key>";
        let (out, calls) = extract(text, &[run_command_spec()]);
        assert_eq!(out, text);
        assert!(calls.is_empty());
    }

    #[test]
    fn ordinary_text_passes_through_untouched() {
        let (out, calls) = extract("if x < y then a<b, nothing to recover", &[]);
        assert_eq!(out, "if x < y then a<b, nothing to recover");
        assert!(calls.is_empty());
    }

    #[test]
    fn kimi_k3_run_command_block_is_recovered_and_removed() {
        let tools = vec![run_command_spec()];
        let (text, calls) = extract(
            "Working <|open|>tools<|sep|><|open|>call tool=\"run_command\" index=\"1\"<|sep|><|open|>argument key=\"command\" type=\"string\"<|sep|>sleep 180; echo waited<|close|>argument<|sep|><|open|>argument key=\"cwd\" type=\"string\"<|sep|>/Users/setoelkahfi/Repositories/onde-ed<|close|>argument<|sep|><|close|>call<|sep|><|close|>tools<|sep|>",
            &tools,
        );
        assert_eq!(text, "Working ");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "run_command");
        let args: serde_json::Value = serde_json::from_str(&calls[0].arguments).unwrap();
        assert_eq!(args["command"], "sleep 180; echo waited");
        assert_eq!(args["cwd"], "/Users/setoelkahfi/Repositories/onde-ed");
    }

    #[test]
    fn kimi_k3_non_string_arguments_are_json_decoded() {
        let tools = vec![command_output_spec()];
        let (_, calls) = extract(
            "<|open|>tools<|sep|><|open|>call tool=\"command_output\" index=\"1\"<|sep|><|open|>argument key=\"task_id\" type=\"integer\"<|sep|>2<|close|>argument<|sep|><|close|>call<|sep|><|close|>tools<|sep|>",
            &tools,
        );
        let args: serde_json::Value = serde_json::from_str(&calls[0].arguments).unwrap();
        assert_eq!(args["task_id"], 2);
        assert!(args["task_id"].is_number());
    }

    #[test]
    fn kimi_k3_missing_argument_type_uses_the_tool_schema() {
        let tools = vec![command_output_spec()];
        let (_, calls) = extract(
            "<|open|>tools<|sep|><|open|>call tool=\"command_output\" index=\"1\"<|sep|><|open|>argument key=\"task_id\"<|sep|>2<|close|>argument<|sep|><|close|>call<|sep|><|close|>tools<|sep|>",
            &tools,
        );
        let args: serde_json::Value = serde_json::from_str(&calls[0].arguments).unwrap();
        assert_eq!(args["task_id"], 2);
        assert!(args["task_id"].is_number());
    }

    #[test]
    fn kimi_k3_multiple_calls_are_recovered_in_order() {
        let tools = vec![command_output_spec()];
        let (_, calls) = extract(
            "<|open|>tools<|sep|><|open|>call tool=\"command_output\" index=\"1\"<|sep|><|open|>argument key=\"task_id\" type=\"integer\"<|sep|>1<|close|>argument<|sep|><|close|>call<|sep|><|open|>call tool=\"command_output\" index=\"2\"<|sep|><|open|>argument key=\"task_id\" type=\"integer\"<|sep|>2<|close|>argument<|sep|><|close|>call<|sep|><|close|>tools<|sep|>",
            &tools,
        );
        let first: serde_json::Value = serde_json::from_str(&calls[0].arguments).unwrap();
        let second: serde_json::Value = serde_json::from_str(&calls[1].arguments).unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(first["task_id"], 1);
        assert_eq!(second["task_id"], 2);
    }

    #[test]
    fn kimi_k3_response_is_unwrapped_and_think_is_hidden() {
        let (text, calls) = extract(
            "a<|open|>think<|sep|>private<|close|>think<|sep|>b<|open|>response<|sep|>visible<|close|>response<|sep|>c",
            &[],
        );
        assert_eq!(text, "abvisiblec");
        assert!(calls.is_empty());
    }

    #[test]
    fn kimi_k3_response_content_is_scanned_for_nested_tool_calls() {
        let tools = vec![command_output_spec()];
        let (text, calls) = extract(
            "<|open|>response<|sep|>before <|open|>think<|sep|>private<|close|>think<|sep|><|open|>tools<|sep|><|open|>call tool=\"command_output\" index=\"1\"<|sep|><|open|>argument key=\"task_id\" type=\"integer\"<|sep|>2<|close|>argument<|sep|><|close|>call<|sep|><|close|>tools<|sep|> after<|close|>response<|sep|>",
            &tools,
        );
        assert_eq!(text, "before  after");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "command_output");
    }

    #[test]
    fn kimi_k3_attribute_entities_are_decoded() {
        let attrs = parse_k3_attributes(r#"tool="run&quot;command" key="a&amp;b""#).unwrap();
        assert_eq!(attrs["tool"], "run\"command");
        assert_eq!(attrs["key"], "a&b");
        assert_eq!(decode_k3_attribute("line&#10;break"), "line&#10;break");
    }

    #[test]
    fn malformed_kimi_k3_tools_block_is_left_alone() {
        let text = "<|open|>tools<|sep|><|open|>call tool=\"run_command\" index=\"1\"<|sep|><|open|>argument key=\"command\" type=\"string\"<|sep|>echo hi<|close|>argument<|sep|>";
        let (out, calls) = extract(text, &[run_command_spec()]);
        assert_eq!(out, text);
        assert!(calls.is_empty());
    }

    #[test]
    fn unknown_kimi_k3_tool_block_is_left_alone() {
        let text = "<|open|>tools<|sep|><|open|>call tool=\"delete_everything\" index=\"1\"<|sep|><|close|>call<|sep|><|close|>tools<|sep|>";
        let (out, calls) = extract(text, &[run_command_spec()]);
        assert_eq!(out, text);
        assert!(calls.is_empty());
    }

    #[test]
    fn invalid_kimi_k3_non_string_argument_is_left_alone() {
        let text = "<|open|>tools<|sep|><|open|>call tool=\"command_output\" index=\"1\"<|sep|><|open|>argument key=\"task_id\" type=\"integer\"<|sep|>not-json<|close|>argument<|sep|><|close|>call<|sep|><|close|>tools<|sep|>";
        let (out, calls) = extract(text, &[command_output_spec()]);
        assert_eq!(out, text);
        assert!(calls.is_empty());
    }

    #[test]
    fn scanner_recovers_a_legacy_tag_split_across_chunk_boundaries() {
        let tools = vec![command_output_spec()];
        let mut scanner = StreamScanner::new(&tools);
        let mut text = String::new();
        let mut calls = Vec::new();

        for chunk in [
            "polling ",
            "<tool_c",
            "all>command_output<arg_",
            "key>task_id</arg_key><arg_value>2</arg_v",
            "alue></tool_call>",
            " done",
        ] {
            for event in scanner.push(chunk) {
                match event {
                    ScanEvent::Text(t) => text.push_str(&t),
                    ScanEvent::ToolCall(c) => calls.push(c),
                }
            }
        }
        if let Some(rest) = scanner.take_pending() {
            text.push_str(&rest);
        }

        assert_eq!(text, "polling  done", "no tag fragment may reach the UI");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "command_output");
    }

    #[test]
    fn scanner_recovers_glm_call_with_malformed_marker_split_across_chunks() {
        let tools = vec![run_command_spec()];
        let mut scanner = StreamScanner::new(&tools);
        let mut text = String::new();
        let mut calls = Vec::new();

        for chunk in [
            "before ",
            "<tool_call>run_command<tool_",
            "call>command</arg_key><arg_value>pwd</arg_value></tool_call>",
            " after",
        ] {
            for event in scanner.push(chunk) {
                match event {
                    ScanEvent::Text(value) => text.push_str(&value),
                    ScanEvent::ToolCall(call) => calls.push(call),
                }
            }
        }
        if let Some(rest) = scanner.take_pending() {
            text.push_str(&rest);
        }

        assert_eq!(text, "before  after");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "run_command");
        let args: serde_json::Value = serde_json::from_str(&calls[0].arguments).unwrap();
        assert_eq!(args["command"], "pwd");
    }

    #[test]
    fn scanner_recovers_check_status_call_split_across_chunks() {
        let tools = vec![command_output_spec()];
        let mut scanner = StreamScanner::new(&tools);
        let mut text = String::new();
        let mut calls = Vec::new();

        for chunk in [
            "before <tool_call>command_output CheckStatus=true_or_poll_",
            "again_with_different_params</arg_",
            "value><arg_key>task_id</arg_key><arg_value>1</arg_value></tool_",
            "call> after",
        ] {
            for event in scanner.push(chunk) {
                match event {
                    ScanEvent::Text(value) => text.push_str(&value),
                    ScanEvent::ToolCall(call) => calls.push(call),
                }
            }
        }
        if let Some(rest) = scanner.take_pending() {
            text.push_str(&rest);
        }

        assert_eq!(text, "before  after");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "command_output");
    }

    #[test]
    fn scanner_recovers_kimi_k3_tools_split_across_chunk_boundaries() {
        let tools = vec![run_command_spec()];
        let mut scanner = StreamScanner::new(&tools);
        let mut text = String::new();
        let mut calls = Vec::new();

        for chunk in [
            "pre ",
            "<|open|>too",
            "ls<|sep|><|open|>call tool=\"run_command\" index=\"1\"<|sep|>",
            "<|open|>argument key=\"command\" type=\"string\"<|sep|>echo hi",
            "<|close|>argument<|sep|><|close|>call<|sep|><|close|>tools<|sep|>",
            " post",
        ] {
            for event in scanner.push(chunk) {
                match event {
                    ScanEvent::Text(t) => text.push_str(&t),
                    ScanEvent::ToolCall(c) => calls.push(c),
                }
            }
        }
        if let Some(rest) = scanner.take_pending() {
            text.push_str(&rest);
        }

        assert_eq!(text, "pre  post");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "run_command");
    }

    #[test]
    fn scanner_recovers_tools_after_an_unclosed_k3_wrapper() {
        let tools = vec![command_output_spec()];
        let mut scanner = StreamScanner::new(&tools);
        let mut text = String::new();
        let mut calls = Vec::new();

        for chunk in [
            "before <|open|>response<|sep|>broken ",
            "<|open|>tools<|sep|><|open|>call tool=\"command_output\" index=\"1\"<|sep|><|open|>argument key=\"task_id\" type=\"integer\"<|sep|>2<|close|>argument<|sep|><|close|>call<|sep|><|close|>tools<|sep|> after",
        ] {
            for event in scanner.push(chunk) {
                match event {
                    ScanEvent::Text(chunk) => text.push_str(&chunk),
                    ScanEvent::ToolCall(call) => calls.push(call),
                }
            }
        }
        if let Some(rest) = scanner.take_pending() {
            text.push_str(&rest);
        }

        assert_eq!(text, "before <|open|>response<|sep|>broken  after");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "command_output");
    }

    #[test]
    fn scanner_recovers_tools_after_an_unclosed_k3_think_wrapper() {
        let tools = vec![command_output_spec()];
        let (text, calls) = extract(
            "<|open|>think<|sep|>broken <|open|>tools<|sep|><|open|>call tool=\"command_output\" index=\"1\"<|sep|><|open|>argument key=\"task_id\" type=\"integer\"<|sep|>2<|close|>argument<|sep|><|close|>call<|sep|><|close|>tools<|sep|>",
            &tools,
        );
        assert_eq!(text, "<|open|>think<|sep|>broken ");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "command_output");
    }

    #[test]
    fn scanner_forwards_ordinary_text_without_holding_it_back() {
        let mut scanner = StreamScanner::new(&[]);
        let events = scanner.push("a normal streamed answer");
        assert_eq!(
            events,
            vec![ScanEvent::Text("a normal streamed answer".to_string())]
        );
        assert!(scanner.take_pending().is_none());
    }

    #[test]
    fn scanner_releases_a_near_miss_prefix_once_it_cannot_match() {
        let mut scanner = StreamScanner::new(&[]);
        assert_eq!(
            scanner.push("see <tool"),
            vec![ScanEvent::Text("see ".to_string())]
        );
        assert_eq!(
            scanner.push("box for details"),
            vec![ScanEvent::Text("<toolbox for details".to_string())]
        );
        assert!(scanner.take_pending().is_none());
    }

    #[test]
    fn scanner_hands_back_an_unclosed_tag_at_stream_end() {
        let tools = vec![run_command_spec()];
        let mut scanner = StreamScanner::new(&tools);
        let events = scanner.push("partial <tool_call>run_command<arg_key>cmd");
        assert_eq!(events, vec![ScanEvent::Text("partial ".to_string())]);
        assert_eq!(
            scanner.take_pending().as_deref(),
            Some("<tool_call>run_command<arg_key>cmd")
        );
    }
}
