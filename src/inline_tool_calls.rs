//! Recovery for tool calls a model emits as literal text instead of the
//! endpoint's structured `tool_calls` field.
//!
//! Several open-weight families siGit Code can drive — Qwen 3, GLM, DeepSeek — are
//! fine-tuned on a chat template that renders a tool call as
//! `<tool_call>NAME<arg_key>K</arg_key><arg_value>V</arg_value>...</tool_call>`.
//! A serving stack is supposed to parse that back into the OpenAI-shaped
//! `tool_calls` field before it reaches us. When it doesn't — seen against the
//! hosted GLM tiers once the model has just been told a call was rejected as a
//! repeat (getsigit/sigit#73) — the raw tag arrives as ordinary content.
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
//! Only the well-formed shape is recovered: a flat sequence of key/value pairs
//! with no nesting. A block that doesn't match is left in the text untouched,
//! deliberately. Reissuing a `run_command` is cheap, but guessing wrong at a
//! half-parsed `edit_file` would write the wrong change to a file, so a
//! malformed block stays visible rather than being silently misinterpreted.

use crate::backend::ToolSpec;

const OPEN_TAG: &str = "<tool_call>";
const CLOSE_TAG: &str = "</tool_call>";

/// One tool call recovered from inline text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recovered {
    pub name: String,
    /// Arguments as a JSON-encoded string, matching `ToolCall::arguments`.
    pub arguments: String,
}

/// Scan a complete text blob for inline tool-call blocks. Returns the text
/// with recovered blocks removed, plus the calls recovered from them. Blocks
/// that don't parse cleanly, and an unterminated trailing tag, are left in the
/// returned text untouched.
///
/// `tools` are the specs offered for this turn, used to type each argument
/// value: a field the schema declares as an integer has to arrive as a JSON
/// number or the tool's own argument parsing rejects it (`command_output`
/// reads `task_id` with `as_u64`, which refuses the string `"2"`). Anything
/// the schema doesn't cover stays a string.
pub fn extract(text: &str, tools: &[ToolSpec]) -> (String, Vec<Recovered>) {
    let mut out = String::with_capacity(text.len());
    let mut calls = Vec::new();
    let mut cursor = text;

    while let Some(open_idx) = cursor.find(OPEN_TAG) {
        let Some(close_rel) = cursor[open_idx..].find(CLOSE_TAG) else {
            // Unterminated tag: leave the rest alone rather than guess at a
            // block we never saw the end of.
            break;
        };
        let close_idx = open_idx + close_rel;
        let inner = &cursor[open_idx + OPEN_TAG.len()..close_idx];

        match parse_block(inner, tools) {
            Some(call) => {
                out.push_str(&cursor[..open_idx]);
                calls.push(call);
            }
            // Didn't parse cleanly: keep the whole tag as visible text.
            None => out.push_str(&cursor[..close_idx + CLOSE_TAG.len()]),
        }
        cursor = &cursor[close_idx + CLOSE_TAG.len()..];
    }
    out.push_str(cursor);
    (out, calls)
}

/// Parse one block's inner text: a name, then zero or more
/// `<arg_key>K</arg_key><arg_value>V</arg_value>` pairs. Returns `None` the
/// moment anything departs from that shape.
fn parse_block(inner: &str, tools: &[ToolSpec]) -> Option<Recovered> {
    let (name, mut rest) = match inner.find("<arg_key>") {
        Some(idx) => (inner[..idx].trim(), &inner[idx..]),
        None => (inner.trim(), ""),
    };
    if name.is_empty() || name.contains(['<', '>']) {
        return None;
    }

    let mut args = serde_json::Map::new();
    while !rest.is_empty() {
        let (key, after_key) = split_once(rest.strip_prefix("<arg_key>")?, "</arg_key>")?;
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
    }

    Some(Recovered {
        name: name.to_string(),
        arguments: serde_json::Value::Object(args).to_string(),
    })
}

fn split_once<'a>(s: &'a str, delim: &str) -> Option<(&'a str, &'a str)> {
    let idx = s.find(delim)?;
    Some((&s[..idx], &s[idx + delim.len()..]))
}

/// The JSON Schema `type` declared for `tool_name`'s `key` parameter — e.g.
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

/// Convert a raw (unescaped) text value to JSON per its declared schema type.
/// Falls back to a plain string for `"string"`, an unknown type, or a value
/// that doesn't parse as what it claims to be. This only ever narrows a value
/// to what the schema already promises — it never invents structure the text
/// doesn't have, since a bad coercion would corrupt a call that was otherwise
/// recoverable.
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
/// Holds back only the text that could still turn into a `<tool_call>` tag, so
/// ordinary answers — the overwhelming majority of turns, which never contain
/// one — still reach the UI token by token. Buffering starts only once an
/// opening tag actually appears, and lasts only until it closes.
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

    /// Take whatever is still held back — e.g. a `<tool_call` prefix that
    /// never closed. Text we never saw the end of can't be interpreted
    /// safely, so the honest thing is to hand it back as-is, exactly as if
    /// recovery had never run. Call this wherever the stream can end, or
    /// held-back text is lost.
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
            let Some(open_idx) = self.pending.find(OPEN_TAG) else {
                // No tag yet: flush all but a suffix that could still grow
                // into "<tool_call>" once the next chunk lands.
                let keep = partial_prefix_len(&self.pending, OPEN_TAG);
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
            let Some(close_rel) = self.pending[OPEN_TAG.len()..].find(CLOSE_TAG) else {
                break; // opened but not closed: wait for more
            };
            let close_idx = OPEN_TAG.len() + close_rel;
            let block: String = self.pending.drain(..close_idx + CLOSE_TAG.len()).collect();
            let inner = &block[OPEN_TAG.len()..block.len() - CLOSE_TAG.len()];
            match parse_block(inner, self.tools) {
                Some(call) => events.push(ScanEvent::ToolCall(call)),
                None => events.push(ScanEvent::Text(block)),
            }
        }
        events
    }
}

/// Length of the longest suffix of `buf` that is a prefix of `needle` — how
/// much of the tail could still become `needle`. Lets a split opening tag
/// (one chunk ending `"<tool_c"`, the next starting `"all>"`) be caught
/// without delaying text that can't possibly be part of one.
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

    #[test]
    fn an_integer_argument_is_recovered_as_a_json_number() {
        // `exec_command_output` reads task_id with `as_u64`, so a string "2"
        // here would parse as JSON and then fail at the tool.
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
        let tools = vec![spec(
            "run_command",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string" },
                    "cwd": { "type": "string" },
                    "run_in_background": { "type": "boolean" }
                }
            }),
        )];
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
    fn a_malformed_block_is_left_alone_rather_than_guessed_at() {
        // The mis-tagged shape seen in practice: an opening `<arg_value>`
        // where `<arg_key>` was meant. Guessing at an edit_file call is worse
        // than leaving it visible.
        let text = "<tool_call>edit_file<arg_value>new_text</arg_key><arg_value>def x; end</arg_value></tool_call>";
        let (out, calls) = extract(text, &[]);
        assert_eq!(out, text);
        assert!(calls.is_empty());
    }

    #[test]
    fn an_unterminated_tag_is_left_alone() {
        let text = "working on it <tool_call>run_command<arg_key>command</arg_key>";
        let (out, calls) = extract(text, &[]);
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
    fn scanner_recovers_a_tag_split_across_chunk_boundaries() {
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
        // "<tool" could still become "<tool_call>", so it is held back...
        assert_eq!(
            scanner.push("see <tool"),
            vec![ScanEvent::Text("see ".to_string())]
        );
        // ...and released as soon as the next chunk rules that out.
        assert_eq!(
            scanner.push("box for details"),
            vec![ScanEvent::Text("<toolbox for details".to_string())]
        );
        assert!(scanner.take_pending().is_none());
    }

    #[test]
    fn scanner_hands_back_an_unclosed_tag_at_stream_end() {
        let mut scanner = StreamScanner::new(&[]);
        let events = scanner.push("partial <tool_call>run_command<arg_key>cmd");
        assert_eq!(events, vec![ScanEvent::Text("partial ".to_string())]);
        assert_eq!(
            scanner.take_pending().as_deref(),
            Some("<tool_call>run_command<arg_key>cmd")
        );
    }
}
