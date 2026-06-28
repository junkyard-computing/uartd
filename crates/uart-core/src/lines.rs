// SPDX-License-Identifier: Apache-2.0
//
// Line framing for the forensic log and the --json per-line view.
//
// Bytes arrive in arbitrary chunks; this reassembles them into whole lines, each stamped
// with the time its FIRST byte was seen (so a line split across two reads is timed by when
// it started, which is what matters during bring-up). A trailing partial line (e.g. a bare
// `login: ` prompt with no newline) is held until completed; `flush` force-emits it.
//
// Text is decoded lossily: a booting kernel spews non-UTF-8 bytes and the daemon must never
// panic on them.

/// A completed line of console output, with the timestamps of its first byte.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub mono_ns: u64,
    pub wall_ms: u64,
    pub text: String,
}

/// Reassembles a byte stream into [`Line`]s split on `\n` (a trailing `\r` is stripped).
#[derive(Debug, Default)]
pub struct LineFramer {
    pending: Vec<u8>,
    pending_ts: Option<(u64, u64)>,
}

impl LineFramer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed a chunk; returns every line completed by it. Lines starting in this chunk are
    /// stamped with (mono_ns, wall_ms); a line continued from a previous chunk keeps the
    /// timestamp of where it began.
    pub fn push(&mut self, mono_ns: u64, wall_ms: u64, data: &[u8]) -> Vec<Line> {
        let mut out = Vec::new();
        for &b in data {
            if self.pending.is_empty() && self.pending_ts.is_none() {
                self.pending_ts = Some((mono_ns, wall_ms));
            }
            if b == b'\n' {
                let (m, w) = self.pending_ts.take().unwrap_or((mono_ns, wall_ms));
                out.push(Line {
                    mono_ns: m,
                    wall_ms: w,
                    text: decode_line(&self.pending),
                });
                self.pending.clear();
            } else {
                self.pending.push(b);
            }
        }
        out
    }

    /// Force-emit the buffered partial line, if any (e.g. on shutdown or to log a prompt).
    pub fn flush(&mut self) -> Option<Line> {
        if self.pending.is_empty() && self.pending_ts.is_none() {
            return None;
        }
        let (m, w) = self.pending_ts.take().unwrap_or((0, 0));
        let line = Line {
            mono_ns: m,
            wall_ms: w,
            text: decode_line(&self.pending),
        };
        self.pending.clear();
        Some(line)
    }

    /// The current incomplete line's bytes (no newline yet).
    pub fn pending(&self) -> &[u8] {
        &self.pending
    }
}

/// Decode a line's bytes to text (lossy) and strip a single trailing carriage return.
fn decode_line(bytes: &[u8]) -> String {
    let end = if bytes.last() == Some(&b'\r') {
        bytes.len() - 1
    } else {
        bytes.len()
    };
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

/// Forensic log rendering of a line: grep-friendly, with both clocks. Wall is epoch ms
/// (no date dependency); monotonic is ns since daemon start.
pub fn format_log_line(line: &Line) -> String {
    format!("[w={} m={}] {}", line.wall_ms, line.mono_ns, line.text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(lines: &[Line]) -> Vec<&str> {
        lines.iter().map(|l| l.text.as_str()).collect()
    }

    #[test]
    fn single_complete_line() {
        let mut f = LineFramer::new();
        let lines = f.push(1, 100, b"abc\n");
        assert_eq!(texts(&lines), vec!["abc"]);
        assert!(f.pending().is_empty());
    }

    #[test]
    fn partial_then_completed() {
        let mut f = LineFramer::new();
        assert!(f.push(10, 100, b"ab").is_empty());
        assert_eq!(f.pending(), b"ab");
        let lines = f.push(20, 200, b"c\n");
        assert_eq!(texts(&lines), vec!["abc"]);
        // timestamp is from the first byte (the first push)
        assert_eq!((lines[0].mono_ns, lines[0].wall_ms), (10, 100));
    }

    #[test]
    fn multiple_lines_one_push() {
        let mut f = LineFramer::new();
        let lines = f.push(1, 100, b"a\nb\nc\n");
        assert_eq!(texts(&lines), vec!["a", "b", "c"]);
    }

    #[test]
    fn crlf_is_stripped() {
        let mut f = LineFramer::new();
        let lines = f.push(1, 100, b"x\r\ny\r\n");
        assert_eq!(texts(&lines), vec!["x", "y"]);
    }

    #[test]
    fn trailing_partial_held_then_flushed() {
        let mut f = LineFramer::new();
        let lines = f.push(1, 100, b"a\nbc");
        assert_eq!(texts(&lines), vec!["a"]);
        assert_eq!(f.pending(), b"bc");
        let flushed = f.flush().unwrap();
        assert_eq!(flushed.text, "bc");
        assert!(f.flush().is_none());
    }

    #[test]
    fn flush_empty_is_none() {
        let mut f = LineFramer::new();
        assert!(f.flush().is_none());
    }

    #[test]
    fn invalid_utf8_does_not_panic() {
        let mut f = LineFramer::new();
        let lines = f.push(1, 100, &[0xff, 0xfe, b'k', b'\n']);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].text.ends_with('k'));
    }

    #[test]
    fn log_format_has_both_clocks() {
        let l = Line {
            mono_ns: 42,
            wall_ms: 99,
            text: "hi".into(),
        };
        assert_eq!(format_log_line(&l), "[w=99 m=42] hi");
    }
}
