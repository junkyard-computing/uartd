// SPDX-License-Identifier: Apache-2.0
//
// Wire framing. The UART line is shared with the interactive console, the shell echo, and
// async kernel printk, and it drops characters. So every uartfs message is a single ASCII
// line with a direction sentinel:
//
//     UFS> KIND arg arg ...      host -> device (commands)
//     UFS< KIND arg arg ...      device -> host (replies)
//
// The reader splits on newlines, resyncs to the *last* sentinel on each line (so a printk
// prefix sharing the line is stripped), and silently ignores anything that isn't a frame.
// Two distinct sentinels mean each side ignores the echo of its own traffic. Payloads are
// base64 (no spaces/newlines), so whitespace tokenisation is unambiguous.

pub const MAGIC_TO_DEVICE: &str = "UFS>";
pub const MAGIC_TO_HOST: &str = "UFS<";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    ToDevice,
    ToHost,
}

impl Dir {
    pub fn magic(self) -> &'static str {
        match self {
            Dir::ToDevice => MAGIC_TO_DEVICE,
            Dir::ToHost => MAGIC_TO_HOST,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub dir: Dir,
    pub kind: String,
    pub args: Vec<String>,
}

impl Frame {
    pub fn new(dir: Dir, kind: impl Into<String>, args: Vec<String>) -> Self {
        Frame {
            dir,
            kind: kind.into(),
            args,
        }
    }

    /// Render as a wire line (no trailing newline). Panics if any token contains whitespace,
    /// which would break tokenisation — callers build tokens from base64/numbers/hex only.
    pub fn encode(&self) -> String {
        let mut out = String::from(self.dir.magic());
        out.push(' ');
        out.push_str(&self.kind);
        for a in &self.args {
            debug_assert!(
                !a.bytes().any(|b| b == b' ' || b == b'\n' || b == b'\r'),
                "frame arg must not contain whitespace: {a:?}"
            );
            out.push(' ');
            out.push_str(a);
        }
        out
    }

    /// Render as a wire line terminated with `\n`.
    pub fn encode_line(&self) -> String {
        let mut s = self.encode();
        s.push('\n');
        s
    }
}

/// Parse one already-delimited line into a frame, resyncing to the last sentinel. Returns
/// `None` for console noise / non-frame lines.
pub fn parse_line(line: &str) -> Option<Frame> {
    let line = line.trim_end_matches(['\r', '\n']);
    let (dir, start) = last_sentinel(line)?;
    let body = &line[start + MAGIC_TO_DEVICE.len()..];
    let mut toks = body.split_whitespace();
    let kind = toks.next()?.to_string();
    let args = toks.map(|t| t.to_string()).collect();
    Some(Frame {
        dir,
        kind,
        args,
    })
}

/// Find the rightmost sentinel in a line (printk noise may share the physical line).
fn last_sentinel(line: &str) -> Option<(Dir, usize)> {
    let d = line.rfind(MAGIC_TO_DEVICE).map(|i| (Dir::ToDevice, i));
    let h = line.rfind(MAGIC_TO_HOST).map(|i| (Dir::ToHost, i));
    match (d, h) {
        (Some(a), Some(b)) => Some(if a.1 >= b.1 { a } else { b }),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

/// Accumulates a noisy byte stream and yields frames as complete lines arrive.
#[derive(Default)]
pub struct FrameReader {
    buf: String,
}

impl FrameReader {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed raw bytes; return every frame completed by them. Non-frame lines are dropped.
    /// The trailing partial line (no newline yet) is retained for the next call.
    pub fn push(&mut self, data: &[u8]) -> Vec<Frame> {
        self.buf.push_str(&String::from_utf8_lossy(data));
        let mut frames = Vec::new();
        // process complete lines, keep the remainder after the last newline
        let mut rest = String::new();
        let mut last_nl = None;
        for (i, c) in self.buf.char_indices() {
            if c == '\n' {
                last_nl = Some(i);
            }
        }
        if let Some(_idx) = last_nl {
            // split into complete-lines portion and remainder
            let mut consumed = 0;
            for line in self.buf.split_inclusive('\n') {
                if line.ends_with('\n') {
                    if let Some(f) = parse_line(line) {
                        frames.push(f);
                    }
                    consumed += line.len();
                } else {
                    rest.push_str(line);
                }
            }
            let _ = consumed;
            self.buf = rest;
        }
        frames
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_parse_roundtrip() {
        let f = Frame::new(
            Dir::ToDevice,
            "DATA",
            vec!["7".into(), "3".into(), "QUJD".into(), "deadbeef".into()],
        );
        let line = f.encode();
        assert_eq!(line, "UFS> DATA 7 3 QUJD deadbeef");
        assert_eq!(parse_line(&line), Some(f));
    }

    #[test]
    fn reply_direction() {
        let f = Frame::new(Dir::ToHost, "ACK", vec!["1".into(), "0".into()]);
        assert_eq!(f.encode(), "UFS< ACK 1 0");
        assert_eq!(parse_line("UFS< ACK 1 0").unwrap().dir, Dir::ToHost);
    }

    #[test]
    fn ignores_console_noise() {
        assert!(parse_line("kalm@fold:~$ ls -la").is_none());
        assert!(parse_line("[   12.345678] usb 1-1: new high-speed device").is_none());
        assert!(parse_line("").is_none());
    }

    #[test]
    fn resyncs_past_printk_prefix_on_same_line() {
        // a printk landed on the same physical line before our frame
        let f = parse_line("[ 3.21] random: crng init done UFS< ACK 2 5").unwrap();
        assert_eq!(f.kind, "ACK");
        assert_eq!(f.args, vec!["2", "5"]);
    }

    #[test]
    fn reader_extracts_frames_from_noisy_stream() {
        let mut r = FrameReader::new();
        let stream = "kalm@fold:~$ \nUFS< READY 1\n[ 9.9] foo\nUFS< ACK 1 0\n";
        let frames = r.push(stream.as_bytes());
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].kind, "READY");
        assert_eq!(frames[1].kind, "ACK");
    }

    #[test]
    fn reader_buffers_partial_line_across_pushes() {
        let mut r = FrameReader::new();
        assert!(r.push(b"UFS< AC").is_empty());
        assert!(r.push(b"K 4").is_empty());
        let frames = r.push(b" 2\n");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].kind, "ACK");
        assert_eq!(frames[0].args, vec!["4", "2"]);
    }

    #[test]
    fn reader_handles_crlf() {
        let mut r = FrameReader::new();
        let frames = r.push(b"UFS< DONE 1 ok abcd\r\n");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].args, vec!["1", "ok", "abcd"]);
    }
}
