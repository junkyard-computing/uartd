// SPDX-License-Identifier: Apache-2.0
//
// The drain buffer: the lossy "what's new since I last looked" feed behind `uart read`.
//
// Design decisions (see plan.md "Resolved design questions"):
//   * Bounded ring. On overflow we drop the OLDEST data and remember how many bytes were
//     lost, so loss is never silent — the next read/peek is prefixed with a
//     `[uartd: dropped N bytes]` marker. The append-only log is the unbounded forensic record.
//   * Single cursor, single consumer. `drain` returns everything since the last drain and
//     advances; `peek` returns the same without advancing.
//   * Elements carry timestamps (monotonic ns + wall ms) so the `--json` view can report
//     per-chunk timing. Timestamps are passed in by the caller, which keeps this unit pure
//     and deterministically testable with no clock.
//
// Raw bytes (not framed lines) are the unit here on purpose: UART prompts like `login: ` have
// no trailing newline, and expect/wait must be able to see them. Line framing for the
// forensic log is a separate concern (see the lines module).

use std::collections::VecDeque;

/// One captured read from the port, stamped with when it arrived.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    pub mono_ns: u64,
    pub wall_ms: u64,
    pub bytes: Vec<u8>,
}

/// Bounded, drainable capture buffer.
#[derive(Debug)]
pub struct DrainBuffer {
    chunks: VecDeque<Chunk>,
    cap: usize,
    len: usize,
    /// Bytes dropped to overflow since the last drain — reported, then cleared, on drain.
    dropped: u64,
}

/// The marker text prefixed to a read when bytes were lost to overflow.
pub fn drop_marker(n: u64) -> String {
    format!("[uartd: dropped {n} bytes]\n")
}

impl DrainBuffer {
    /// Create a buffer holding at most `cap` bytes of undrained data.
    pub fn new(cap: usize) -> Self {
        DrainBuffer {
            chunks: VecDeque::new(),
            cap,
            len: 0,
            dropped: 0,
        }
    }

    /// Append freshly captured bytes, stamped with their arrival time. Enforces the cap by
    /// dropping whole oldest chunks (the newest chunk is always kept, even if it alone
    /// exceeds the cap).
    pub fn push(&mut self, mono_ns: u64, wall_ms: u64, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        self.len += data.len();
        self.chunks.push_back(Chunk {
            mono_ns,
            wall_ms,
            bytes: data.to_vec(),
        });
        while self.len > self.cap && self.chunks.len() > 1 {
            let old = self.chunks.pop_front().expect("len>0 implies a chunk");
            self.len -= old.bytes.len();
            self.dropped += old.bytes.len() as u64;
        }
    }

    /// True when there is nothing new to report (no chunks and no pending drop marker).
    pub fn is_empty(&self) -> bool {
        self.chunks.is_empty() && self.dropped == 0
    }

    /// Number of undrained bytes currently buffered (excludes any drop marker).
    pub fn len(&self) -> usize {
        self.len
    }

    fn rendered_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.len + 32);
        if self.dropped > 0 {
            out.extend_from_slice(drop_marker(self.dropped).as_bytes());
        }
        for c in &self.chunks {
            out.extend_from_slice(&c.bytes);
        }
        out
    }

    /// Non-destructive view: the drop marker (if any) followed by all buffered bytes.
    pub fn peek_bytes(&self) -> Vec<u8> {
        self.rendered_bytes()
    }

    /// Destructive read: same bytes as `peek_bytes`, then clears the buffer and drop count.
    pub fn drain_bytes(&mut self) -> Vec<u8> {
        let out = self.rendered_bytes();
        self.chunks.clear();
        self.len = 0;
        self.dropped = 0;
        out
    }

    /// Non-destructive structured view: (bytes dropped, chunks) for the `--json` output.
    pub fn peek_chunks(&self) -> (u64, Vec<Chunk>) {
        (self.dropped, self.chunks.iter().cloned().collect())
    }

    /// Destructive structured read: like `peek_chunks`, then clears.
    pub fn drain_chunks(&mut self) -> (u64, Vec<Chunk>) {
        let out = (self.dropped, self.chunks.iter().cloned().collect());
        self.chunks.clear();
        self.len = 0;
        self.dropped = 0;
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drain_returns_then_clears() {
        let mut b = DrainBuffer::new(1024);
        b.push(1, 100, b"hello");
        assert_eq!(b.drain_bytes(), b"hello");
        assert!(b.is_empty());
        assert_eq!(b.drain_bytes(), b"");
    }

    #[test]
    fn peek_is_non_destructive() {
        let mut b = DrainBuffer::new(1024);
        b.push(1, 100, b"world");
        assert_eq!(b.peek_bytes(), b"world");
        assert_eq!(b.peek_bytes(), b"world"); // still there
        assert!(!b.is_empty());
        assert_eq!(b.drain_bytes(), b"world"); // drain after peek still works
        assert!(b.is_empty());
    }

    #[test]
    fn pushes_concatenate_in_order() {
        let mut b = DrainBuffer::new(1024);
        b.push(1, 100, b"foo");
        b.push(2, 200, b"bar");
        b.push(3, 300, b"baz");
        assert_eq!(b.drain_bytes(), b"foobarbaz");
    }

    #[test]
    fn empty_push_is_noop() {
        let mut b = DrainBuffer::new(1024);
        b.push(1, 100, b"");
        assert!(b.is_empty());
    }

    #[test]
    fn overflow_drops_oldest_and_marks() {
        let mut b = DrainBuffer::new(10);
        b.push(1, 100, b"AAAAAA"); // 6
        b.push(2, 200, b"BBBBBBBB"); // +8 = 14 > 10 -> drop the 6 AAAAAA
        let out = b.drain_bytes();
        let expected = format!("{}{}", drop_marker(6), "BBBBBBBB");
        assert_eq!(out, expected.as_bytes());
    }

    #[test]
    fn newest_chunk_kept_even_if_larger_than_cap() {
        let mut b = DrainBuffer::new(4);
        b.push(1, 100, b"AA");
        b.push(2, 200, b"BBBBBBBB"); // alone exceeds cap; kept, AA dropped
        let (dropped, chunks) = b.peek_chunks();
        assert_eq!(dropped, 2);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].bytes, b"BBBBBBBB");
    }

    #[test]
    fn drop_marker_cleared_after_drain() {
        let mut b = DrainBuffer::new(4);
        b.push(1, 100, b"AAAA");
        b.push(2, 200, b"BBBB"); // drops AAAA
        assert!(b.drain_bytes().starts_with(drop_marker(4).as_bytes()));
        assert!(b.is_empty());
        b.push(3, 300, b"CC");
        assert_eq!(b.drain_bytes(), b"CC"); // no stale marker
    }

    #[test]
    fn json_chunks_carry_timestamps() {
        let mut b = DrainBuffer::new(1024);
        b.push(11, 111, b"x");
        b.push(22, 222, b"yz");
        let (dropped, chunks) = b.drain_chunks();
        assert_eq!(dropped, 0);
        assert_eq!(chunks.len(), 2);
        assert_eq!((chunks[0].mono_ns, chunks[0].wall_ms), (11, 111));
        assert_eq!(chunks[1].bytes, b"yz");
        assert!(b.is_empty());
    }
}
