// SPDX-License-Identifier: Apache-2.0
//
// Incremental regex matching over the live stream, behind `uart wait` and
// `uart send --expect`. Bytes are fed as they arrive; the matcher accumulates them and
// reports the first match — which may span chunk boundaries (e.g. a prompt split across two
// reads) and may span line boundaries (the pattern controls its own anchors).
//
// This does NOT consume the drain buffer (see plan.md): wait/expect observe a private rolling
// window, so a subsequent `uart read` still sees the same bytes.

use regex::Regex;

/// Default cap on the private accumulation window. A pattern match spanning more than this is
/// implausible for prompts/markers; bounding it keeps a long wait under a chatty device from
/// growing without limit.
pub const DEFAULT_CAP: usize = 256 * 1024;

/// The result of a successful match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Match {
    /// The exact text the regex matched.
    pub matched: String,
    /// Everything accumulated up to and including the match — the "what happened in between".
    pub context: String,
}

/// Feeds bytes incrementally and reports the first regex match.
pub struct ExpectMatcher {
    re: Regex,
    acc: String,
    cap: usize,
}

impl ExpectMatcher {
    pub fn new(pattern: &str) -> Result<Self, regex::Error> {
        Self::with_cap(pattern, DEFAULT_CAP)
    }

    pub fn with_cap(pattern: &str, cap: usize) -> Result<Self, regex::Error> {
        Ok(ExpectMatcher {
            re: Regex::new(pattern)?,
            acc: String::new(),
            cap: cap.max(1),
        })
    }

    /// Append a chunk and test for a match. Returns `Some(Match)` the first time the pattern
    /// is found anywhere in the accumulated stream.
    pub fn feed(&mut self, data: &[u8]) -> Option<Match> {
        self.acc.push_str(&String::from_utf8_lossy(data));
        self.trim();
        if let Some(m) = self.re.find(&self.acc) {
            Some(Match {
                matched: m.as_str().to_string(),
                context: self.acc[..m.end()].to_string(),
            })
        } else {
            None
        }
    }

    /// Everything accumulated so far (for returning partial context on timeout).
    pub fn buffer(&self) -> &str {
        &self.acc
    }

    /// Keep the window bounded: once it exceeds `cap`, retain only the most recent half so
    /// recent data (where prompts appear) is still matchable.
    fn trim(&mut self) {
        if self.acc.len() <= self.cap {
            return;
        }
        let keep = self.cap / 2;
        // find a char boundary at or after (len - keep)
        let mut start = self.acc.len() - keep;
        while start < self.acc.len() && !self.acc.is_char_boundary(start) {
            start += 1;
        }
        self.acc = self.acc[start..].to_string();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_within_one_feed() {
        let mut m = ExpectMatcher::new("login:").unwrap();
        let hit = m.feed(b"raspberrypi login: ").unwrap();
        assert_eq!(hit.matched, "login:");
        assert!(hit.context.contains("login:"));
    }

    #[test]
    fn matches_across_feeds() {
        let mut m = ExpectMatcher::new("hello").unwrap();
        assert!(m.feed(b"hel").is_none());
        let hit = m.feed(b"lo world").unwrap();
        assert_eq!(hit.matched, "hello");
    }

    #[test]
    fn no_match_returns_none() {
        let mut m = ExpectMatcher::new("xyz").unwrap();
        assert!(m.feed(b"nothing here").is_none());
        assert_eq!(m.buffer(), "nothing here");
    }

    #[test]
    fn shell_prompt_regex() {
        let mut m = ExpectMatcher::new(r"\$ ").unwrap();
        assert!(m.feed(b"root@host:~# ").is_none());
        assert!(m.feed(b"user@host:~$ ").is_some());
    }

    #[test]
    fn matches_across_line_boundary() {
        let mut m = ExpectMatcher::new("b\nc").unwrap();
        let hit = m.feed(b"a\nb\nc\n").unwrap();
        assert_eq!(hit.matched, "b\nc");
    }

    #[test]
    fn invalid_pattern_errors() {
        assert!(ExpectMatcher::new("(unclosed").is_err());
    }

    #[test]
    fn trims_but_still_matches_recent() {
        let mut m = ExpectMatcher::with_cap("END", 64).unwrap();
        for _ in 0..100 {
            assert!(m.feed(b"xxxxxxxxxx").is_none());
        }
        // buffer stayed bounded
        assert!(m.buffer().len() <= 64);
        let hit = m.feed(b"END").unwrap();
        assert_eq!(hit.matched, "END");
    }

    #[test]
    fn trim_on_multibyte_does_not_panic() {
        let mut m = ExpectMatcher::with_cap("Z", 8).unwrap();
        for _ in 0..20 {
            // 'é' is two bytes; trimming must land on a char boundary
            assert!(m.feed("éééé".as_bytes()).is_none());
        }
        assert!(m.feed(b"Z").is_some());
    }
}
