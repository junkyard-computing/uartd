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
//
// Per-frame integrity: every wire line carries a trailing checksum token — the first 8 hex
// chars of sha256 over the frame BODY (`KIND arg1 arg2 ...`, single-space-joined, exactly the
// text that precedes the checksum). The reader recomputes it and REJECTS any line whose
// checksum doesn't match, so a garbled/merged line is treated as not-a-frame (resync) instead
// of reaching `Msg::from_frame` as a valid-but-wrong message. The phone agent computes the
// same token with `printf '%s' "<body>" | sha256sum | cut -c1-8`.

use crate::hash::sha256_prefix;

pub const MAGIC_TO_DEVICE: &str = "UFS>";
pub const MAGIC_TO_HOST: &str = "UFS<";

/// Length (hex chars) of the per-frame checksum token.
pub const CKSUM_LEN: usize = 8;

/// Compute the per-frame checksum over an already-rendered body (`KIND arg1 arg2 ...`).
pub fn frame_cksum(body: &str) -> String {
    sha256_prefix(body.as_bytes(), CKSUM_LEN)
}

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

    /// Render the frame BODY (`KIND arg1 arg2 ...`), without sentinel or checksum. This is the
    /// exact text the checksum is computed over.
    fn body(&self) -> String {
        let mut out = String::from(&self.kind);
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

    /// Render as a wire line (no trailing newline): `SENTINEL BODY CKSUM`. Panics if any token
    /// contains whitespace, which would break tokenisation — callers build tokens from
    /// base64/numbers/hex only.
    pub fn encode(&self) -> String {
        let body = self.body();
        let cksum = frame_cksum(&body);
        let mut out = String::from(self.dir.magic());
        out.push(' ');
        out.push_str(&body);
        out.push(' ');
        out.push_str(&cksum);
        out
    }

    /// Render as a wire line terminated with `\n`.
    pub fn encode_line(&self) -> String {
        let mut s = self.encode();
        s.push('\n');
        s
    }
}

/// Parse one already-delimited line into a frame, resyncing to the last sentinel and verifying
/// the trailing per-frame checksum. Returns `None` for console noise / non-frame lines AND for
/// any line whose checksum doesn't match (corruption is rejected, never mis-parsed).
pub fn parse_line(line: &str) -> Option<Frame> {
    let line = line.trim_end_matches(['\r', '\n']);
    let (dir, start) = last_sentinel(line)?;
    let after = &line[start + MAGIC_TO_DEVICE.len()..];
    // tokens = KIND arg1 .. argN CKSUM
    let mut toks: Vec<&str> = after.split_whitespace().collect();
    // need at least KIND + CKSUM
    if toks.len() < 2 {
        return None;
    }
    let cksum = toks.pop()?; // trailing checksum token
    // recompute over the body (the tokens that remain), single-space-joined
    let body = toks.join(" ");
    if frame_cksum(&body) != cksum {
        return None; // checksum mismatch -> not a (valid) frame; resync
    }
    let kind = toks[0].to_string();
    let args = toks[1..].iter().map(|t| t.to_string()).collect();
    Some(Frame { dir, kind, args })
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

    /// Helper: render `SENTINEL BODY CKSUM` for a hand-written body, with a correct checksum.
    fn wire(magic: &str, body: &str) -> String {
        format!("{magic} {body} {}", frame_cksum(body))
    }

    #[test]
    fn encode_parse_roundtrip() {
        let f = Frame::new(
            Dir::ToDevice,
            "DATA",
            vec!["7".into(), "3".into(), "QUJD".into(), "deadbeef".into()],
        );
        let line = f.encode();
        // sentinel + body + an 8-hex checksum token
        assert!(line.starts_with("UFS> DATA 7 3 QUJD deadbeef "));
        let cksum = line.rsplit(' ').next().unwrap();
        assert_eq!(cksum.len(), CKSUM_LEN);
        assert_eq!(cksum, frame_cksum("DATA 7 3 QUJD deadbeef"));
        assert_eq!(parse_line(&line), Some(f));
    }

    #[test]
    fn reply_direction() {
        let f = Frame::new(Dir::ToHost, "ACK", vec!["1".into(), "0".into()]);
        let line = wire("UFS<", "ACK 1 0");
        assert_eq!(f.encode(), line);
        assert_eq!(parse_line(&line).unwrap().dir, Dir::ToHost);
    }

    #[test]
    fn bad_checksum_is_rejected() {
        // a line that tokenises fine but whose checksum doesn't match the body
        let bad = "UFS< ACK 1 0 00000000";
        assert!(parse_line(bad).is_none());
        // a single-byte corruption of an arg invalidates the (unchanged) checksum
        let good = wire("UFS<", "ACK 1 0");
        let cksum = good.rsplit(' ').next().unwrap().to_string();
        let corrupted = format!("UFS< ACK 1 9 {cksum}"); // 0 -> 9
        assert!(parse_line(&corrupted).is_none());
    }

    #[test]
    fn missing_checksum_token_is_rejected() {
        // a frame body with no checksum token at all is not accepted
        assert!(parse_line("UFS< ACK 1 0").is_none());
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
        let line = format!("[ 3.21] random: crng init done {}", wire("UFS<", "ACK 2 5"));
        let f = parse_line(&line).unwrap();
        assert_eq!(f.kind, "ACK");
        assert_eq!(f.args, vec!["2", "5"]);
    }

    #[test]
    fn reader_extracts_frames_from_noisy_stream() {
        let mut r = FrameReader::new();
        let stream = format!(
            "kalm@fold:~$ \n{}\n[ 9.9] foo\n{}\n",
            wire("UFS<", "READY 1"),
            wire("UFS<", "ACK 1 0"),
        );
        let frames = r.push(stream.as_bytes());
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].kind, "READY");
        assert_eq!(frames[1].kind, "ACK");
    }

    #[test]
    fn reader_buffers_partial_line_across_pushes() {
        let mut r = FrameReader::new();
        let line = wire("UFS<", "ACK 4 2");
        let (a, b) = line.split_at(7);
        assert!(r.push(a.as_bytes()).is_empty());
        let frames = r.push(format!("{b}\n").as_bytes());
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].kind, "ACK");
        assert_eq!(frames[0].args, vec!["4", "2"]);
    }

    #[test]
    fn reader_handles_crlf() {
        let mut r = FrameReader::new();
        let line = format!("{}\r\n", wire("UFS<", "DONE 1 ok abcd"));
        let frames = r.push(line.as_bytes());
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].args, vec!["1", "ok", "abcd"]);
    }

    // ---- byte-level fault injection on the wire ----

    #[test]
    fn reader_rejects_single_byte_garble_in_payload() {
        // Flip one byte of a real frame's payload (not the checksum): every such corruption
        // must be rejected by the checksum, never surface as a frame.
        let good = wire("UFS<", "DATA 1 0 QUJDREVG");
        let bytes = good.as_bytes();
        // try corrupting each payload byte position (skip sentinel + the trailing cksum token)
        let cksum_start = good.rfind(' ').unwrap();
        for i in "UFS< ".len()..cksum_start {
            // pick a different printable byte so the line still tokenises
            let mut b = bytes.to_vec();
            b[i] = if b[i] == b'X' { b'Y' } else { b'X' };
            // only meaningful if we actually changed a non-space byte
            if bytes[i] == b' ' {
                continue;
            }
            let mut r = FrameReader::new();
            let mut line = b;
            line.push(b'\n');
            let frames = r.push(&line);
            assert!(
                frames.is_empty(),
                "corruption at byte {i} was not rejected: {:?}",
                String::from_utf8_lossy(&line)
            );
        }
    }

    #[test]
    fn reader_rejects_garbled_checksum() {
        let good = wire("UFS<", "ACK 9 4");
        let mut b = good.into_bytes();
        let last = b.len() - 1;
        b[last] = if b[last] == b'0' { b'1' } else { b'0' }; // flip a checksum hex digit
        b.push(b'\n');
        let mut r = FrameReader::new();
        assert!(r.push(&b).is_empty());
    }

    #[test]
    fn reader_recovers_after_a_corrupt_line() {
        // a garbled line is dropped, but the following clean frame still parses (resync).
        let mut r = FrameReader::new();
        let bad = "UFS< ACK 1 0 deadbeef\n"; // wrong checksum
        let good = format!("{}\n", wire("UFS<", "ACK 1 1"));
        let frames = r.push(format!("{bad}{good}").as_bytes());
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].args, vec!["1", "1"]);
    }

    #[test]
    fn reader_handles_inserted_newline_splitting_a_frame() {
        // a spurious '\n' inserted mid-frame splits it into two non-frames; both are dropped,
        // and a following clean frame still parses.
        let good = wire("UFS<", "DATA 2 5 QUJD");
        let mid = good.len() / 2;
        let (a, b) = good.split_at(mid);
        let next = wire("UFS<", "ACK 2 5");
        let stream = format!("{a}\n{b}\n{next}\n");
        let mut r = FrameReader::new();
        let frames = r.push(stream.as_bytes());
        assert_eq!(frames.len(), 1, "only the clean trailing frame should parse");
        assert_eq!(frames[0].kind, "ACK");
    }

    #[test]
    fn reader_resyncs_to_last_of_merged_frames() {
        // Two frames merged onto one line (newline between them lost). Resync to the LAST
        // sentinel isolates the trailing frame, whose checksum still matches -> it recovers;
        // the leading frame is discarded. The key safety property is that we never splice the
        // two into a corrupt-but-accepted message.
        let a = wire("UFS<", "ACK 1 0");
        let b = wire("UFS<", "ACK 1 1");
        let merged = format!("{a} {b}\n");
        let mut r = FrameReader::new();
        let frames = r.push(merged.as_bytes());
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].args, vec!["1", "1"]);
    }

    #[test]
    fn reader_drops_merged_frame_when_tail_is_garbled() {
        // If the trailing (resynced) frame is itself corrupted, the whole line is dropped —
        // the leading frame's bytes become body for the failed checksum, so nothing leaks.
        let a = wire("UFS<", "ACK 1 0");
        let b = wire("UFS<", "ACK 1 1");
        let mut merged = format!("{a} {b}").into_bytes();
        let last = merged.len() - 1;
        merged[last] ^= 0x01; // garble the tail checksum
        merged.push(b'\n');
        let mut r = FrameReader::new();
        assert!(r.push(&merged).is_empty());
    }
}
