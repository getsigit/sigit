//! Spots a streamed reply that has degenerated into a repetition loop.
//!
//! A model that falls into degenerate decoding keeps producing words from a
//! tiny vocabulary and never emits a tool call or a stop (issue #123: thousands
//! of words of `find: list: check: get: …`). It is not an exact repeat, so the
//! check looks at how few distinct words a recent window holds, not at literal
//! repeated strings.

use std::collections::{HashMap, VecDeque};

/// Words in the sliding window.
const WINDOW: usize = 200;
/// The window is re-checked after this many new words, so the cost stays flat
/// on a long reply.
const CHECK_EVERY: usize = 25;
/// A full window with at most this many distinct words is degenerate. Ordinary
/// prose of 200 words holds roughly 90 to 120 distinct ones; the loop in the
/// issue holds about 30 and an exact repeat far fewer, so there is room on both
/// sides.
const MAX_DISTINCT: usize = 40;

/// Fed the same visible text the user is shown, in order. Words inside fenced
/// code blocks are skipped: code, tables and data are legitimately repetitive.
#[derive(Default)]
pub struct RepetitionGuard {
    /// Bytes fed so far; the offset of the next char in the caller's text.
    offset: usize,
    window: VecDeque<(String, usize)>,
    counts: HashMap<String, usize>,
    since_check: usize,
    word: String,
    word_start: usize,
    /// The first up-to-3 non-blank chars of the current line.
    line_head: String,
    fence_line: bool,
    in_code: bool,
    tripped_at: Option<usize>,
}

impl RepetitionGuard {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed the next piece of text. Returns true once the reply has tripped,
    /// and keeps returning true after that.
    pub fn push(&mut self, text: &str) -> bool {
        for (i, c) in text.char_indices() {
            if self.tripped_at.is_some() {
                break;
            }
            self.feed(self.offset + i, c);
        }
        self.offset += text.len();
        self.tripped_at.is_some()
    }

    /// Byte offset in the fed text where the offending window began. The text
    /// before it is the reply worth keeping.
    pub fn loop_start(&self) -> Option<usize> {
        self.tripped_at
    }

    fn feed(&mut self, at: usize, c: char) {
        if c == '\n' {
            self.end_word();
            self.line_head.clear();
            self.fence_line = false;
            return;
        }
        if self.line_head.chars().count() < 3 && !(c.is_whitespace() && self.line_head.is_empty()) {
            self.line_head.push(c);
            if self.line_head == "```" {
                self.in_code = !self.in_code;
                self.fence_line = true;
            }
        }
        if self.in_code || self.fence_line || !c.is_alphanumeric() {
            self.end_word();
            return;
        }
        if self.word.is_empty() {
            self.word_start = at;
        }
        self.word.extend(c.to_lowercase());
    }

    /// Where the loop began inside the window. The window can hold a few words
    /// of real reply ahead of it, so walk back from the end while words still
    /// belong to the vocabulary of the window's second half.
    fn loop_origin(&self) -> usize {
        let vocab: std::collections::HashSet<&str> = self
            .window
            .iter()
            .skip(WINDOW / 2)
            .map(|(word, _)| word.as_str())
            .collect();
        let mut origin = self.window.front().map_or(0, |(_, start)| *start);
        for (word, start) in self.window.iter().rev() {
            if !vocab.contains(word.as_str()) {
                break;
            }
            origin = *start;
        }
        origin
    }

    fn end_word(&mut self) {
        if self.word.is_empty() {
            return;
        }
        let word = std::mem::take(&mut self.word);
        *self.counts.entry(word.clone()).or_default() += 1;
        self.window.push_back((word, self.word_start));
        if self.window.len() > WINDOW
            && let Some((old, _)) = self.window.pop_front()
            && let Some(count) = self.counts.get_mut(&old)
        {
            *count -= 1;
            if *count == 0 {
                self.counts.remove(&old);
            }
        }
        self.since_check += 1;
        if self.window.len() == WINDOW
            && self.since_check >= CHECK_EVERY
            && self.counts.len() <= MAX_DISTINCT
        {
            self.tripped_at = Some(self.loop_origin());
        }
        if self.since_check >= CHECK_EVERY {
            self.since_check = 0;
        }
    }
}

/// The shape of the loop in issue #123: a random walk over about 30 words.
#[cfg(test)]
pub(crate) fn issue_loop_text() -> String {
    let vocab = [
        "find:", "list:", "check:", "get:", "find", "them:", "list", "commits", "with", "that",
        "key:", "show:", "read:", "search:", "open:", "match:", "run:", "see:", "look:", "take:",
        "grab:", "pull:", "fetch:", "scan:", "test:", "view:", "load:", "parse:", "walk:", "dump:",
    ];
    let mut state = 7u32;
    let mut out = String::new();
    for _ in 0..1500 {
        state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
        out.push_str(vocab[(state >> 16) as usize % vocab.len()]);
        out.push(' ');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prose() -> String {
        // Distinct words throughout, like a real paragraph.
        (0..400)
            .map(|n| format!("word{n}"))
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn feed_in_chunks(guard: &mut RepetitionGuard, text: &str) -> bool {
        let mut tripped = false;
        for piece in text.as_bytes().chunks(7) {
            tripped = guard.push(&String::from_utf8_lossy(piece));
        }
        tripped
    }

    #[test]
    fn the_issue_loop_trips() {
        let mut guard = RepetitionGuard::new();
        assert!(feed_in_chunks(&mut guard, &issue_loop_text()));
    }

    #[test]
    fn a_short_exact_loop_trips_and_reports_where_it_began() {
        let intro = "Let me check the history first. ";
        let text = format!("{intro}{}", "a b a b ".repeat(200));
        let mut guard = RepetitionGuard::new();
        assert!(guard.push(&text));
        let start = guard.loop_start().unwrap();
        assert!(start >= intro.len(), "cut {start} is inside the intro");
        assert!(text.is_char_boundary(start));
    }

    #[test]
    fn ordinary_prose_does_not_trip() {
        let mut guard = RepetitionGuard::new();
        assert!(!feed_in_chunks(&mut guard, &prose()));
        assert_eq!(guard.loop_start(), None);
    }

    #[test]
    fn a_fenced_block_of_repeated_lines_does_not_trip() {
        let text = format!("Here:\n```\n{}```\nDone.\n", "foo bar\n".repeat(400));
        let mut guard = RepetitionGuard::new();
        assert!(!feed_in_chunks(&mut guard, &text));
    }

    #[test]
    fn a_list_of_similar_bullets_does_not_trip() {
        let text: String = (0..60)
            .map(|n| format!("- Update the handler for route{n} so it returns item{n} quickly\n"))
            .collect();
        let mut guard = RepetitionGuard::new();
        assert!(!feed_in_chunks(&mut guard, &text));
    }

    #[test]
    fn text_after_a_closed_fence_counts_again() {
        let text = format!("```\nx\n```\n{}", "a b a b ".repeat(200));
        let mut guard = RepetitionGuard::new();
        assert!(guard.push(&text));
    }
}
