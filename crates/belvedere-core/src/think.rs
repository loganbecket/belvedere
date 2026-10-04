//! Some models (Qwen3 among them) begin a reply with a `<think>…</think>`
//! block of private reasoning. People shouldn't see it, and it shouldn't
//! be saved as the answer. This filter removes those blocks from a stream
//! of text that arrives in arbitrary pieces.

/// Streaming filter: feed it pieces, get back only the visible text.
#[derive(Debug, Default)]
pub struct ThinkFilter {
    inside: bool,
    /// Text held back because it might be the start of a tag.
    held: String,
    /// After a block closes, models usually emit blank lines before the
    /// answer; swallow whitespace until real text arrives.
    skipping_whitespace: bool,
}

const OPEN: &str = "<think>";
const CLOSE: &str = "</think>";

impl ThinkFilter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the model is currently inside a thinking block.
    pub fn thinking(&self) -> bool {
        self.inside
    }

    /// Feeds one piece of streamed text; returns the part to show now.
    pub fn push(&mut self, piece: &str) -> String {
        self.held.push_str(piece);
        let mut out = String::new();
        loop {
            let tag = if self.inside { CLOSE } else { OPEN };
            match self.held.find(tag) {
                Some(at) => {
                    if !self.inside {
                        out.push_str(&self.held[..at]);
                    }
                    self.held.drain(..at + tag.len());
                    self.inside = !self.inside;
                    if !self.inside {
                        self.skipping_whitespace = true;
                    }
                }
                None => {
                    // Keep back any suffix that could be the beginning of
                    // the tag we're looking for; release the rest.
                    let keep = longest_tag_prefix_at_end(&self.held, tag);
                    let release = self.held.len() - keep;
                    if !self.inside {
                        out.push_str(self.release(release));
                    }
                    self.held.drain(..release);
                    return out;
                }
            }
        }
    }

    /// Call at the end of the stream: anything held back is released
    /// (if we were outside a block) since no tag is coming.
    pub fn finish(&mut self) -> String {
        if self.inside {
            self.held.clear();
            return String::new();
        }
        let all = self.held.len();
        let text = self.release(all).to_string();
        self.held.clear();
        text
    }

    /// The first `len` bytes of `held`, minus leading whitespace if we are
    /// still skipping it after a closed block.
    fn release(&mut self, len: usize) -> &str {
        let text = &self.held[..len];
        if !self.skipping_whitespace {
            return text;
        }
        let trimmed = text.trim_start();
        if !trimmed.is_empty() {
            self.skipping_whitespace = false;
        }
        trimmed
    }
}

/// Length of the longest suffix of `s` that is a proper prefix of `tag`.
fn longest_tag_prefix_at_end(s: &str, tag: &str) -> usize {
    let max = tag.len().min(s.len());
    for n in (1..=max).rev() {
        if s.is_char_boundary(s.len() - n) && tag.starts_with(&s[s.len() - n..]) {
            return n;
        }
    }
    0
}

/// Removes every `<think>…</think>` block from complete text.
pub fn strip(text: &str) -> String {
    let mut f = ThinkFilter::new();
    let mut out = f.push(text);
    out.push_str(&f.finish());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stream(pieces: &[&str]) -> String {
        let mut f = ThinkFilter::new();
        let mut out = String::new();
        for p in pieces {
            out.push_str(&f.push(p));
        }
        out.push_str(&f.finish());
        out
    }

    #[test]
    fn plain_text_passes_through_untouched() {
        assert_eq!(stream(&["Hello", ", ", "world"]), "Hello, world");
        assert_eq!(stream(&["a < b and c > d"]), "a < b and c > d");
    }

    #[test]
    fn whole_block_in_one_piece_is_removed() {
        assert_eq!(
            strip("<think>Let me reason.</think>\n\nThe answer is 4."),
            "The answer is 4."
        );
    }

    #[test]
    fn block_split_across_pieces_is_removed() {
        assert_eq!(
            stream(&["<thi", "nk>pond", "ering</th", "ink>", "\n", "Yes."]),
            "Yes."
        );
        assert_eq!(
            stream(&["<", "think", ">", "x", "<", "/think>", "Done"]),
            "Done"
        );
    }

    #[test]
    fn text_before_and_between_blocks_is_kept() {
        assert_eq!(
            strip("Hi. <think>a</think>First. <think>b</think>Second."),
            "Hi. First. Second."
        );
    }

    #[test]
    fn unfinished_block_at_end_shows_nothing() {
        assert_eq!(stream(&["<think>still going"]), "");
    }

    #[test]
    fn a_lone_angle_bracket_at_the_end_is_released_on_finish() {
        let mut f = ThinkFilter::new();
        assert_eq!(f.push("x <"), "x ");
        assert_eq!(f.finish(), "<");
    }

    #[test]
    fn thinking_flag_tracks_state() {
        let mut f = ThinkFilter::new();
        f.push("<think>");
        assert!(f.thinking());
        f.push("…</think>");
        assert!(!f.thinking());
    }
}
