// SPDX-License-Identifier: Apache-2.0
//
// Blob chunking and reassembly. A payload is split into fixed-size pieces, each base64-encoded
// and tagged with a sha256 prefix over its base64 text (so the receiver can reject a corrupted
// chunk and ask for a resend). The whole blob carries a full sha256 that gates any use of the
// reconstructed bytes — never trust a byte you didn't checksum.
//
// The `Reassembler` mirrors exactly what the phone-side shell agent does, so host and device
// agree on framing, and the roundtrip is testable here without a device.

use std::collections::BTreeMap;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;

use crate::hash::{sha256_hex, sha256_prefix};

/// Length of the per-chunk sha256 prefix tag (hex chars).
pub const SUM_LEN: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    pub seq: u32,
    pub b64: String,
    pub sum: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboundBlob {
    pub sha256: String,
    pub chunk_size: u32,
    pub chunks: Vec<Chunk>,
}

impl OutboundBlob {
    pub fn nchunks(&self) -> u32 {
        self.chunks.len() as u32
    }
}

/// Per-chunk integrity tag the device recomputes as `printf %s "$b64" | sha256sum`.
pub fn chunk_sum(b64: &str) -> String {
    sha256_prefix(b64.as_bytes(), SUM_LEN)
}

/// Split `data` into base64 chunks of at most `chunk_size` raw bytes each.
pub fn prepare(data: &[u8], chunk_size: usize) -> OutboundBlob {
    let chunk_size = chunk_size.max(1);
    let mut chunks = Vec::new();
    for (i, raw) in data.chunks(chunk_size).enumerate() {
        let b64 = B64.encode(raw);
        let sum = chunk_sum(&b64);
        chunks.push(Chunk {
            seq: i as u32,
            b64,
            sum,
        });
    }
    OutboundBlob {
        sha256: sha256_hex(data),
        chunk_size: chunk_size as u32,
        chunks,
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum ChunkError {
    BadSum { seq: u32 },
    BadBase64 { seq: u32 },
    Incomplete { missing: Vec<u32> },
    Sha256Mismatch { expected: String, got: String },
}

/// Collects chunks, verifying each, and reconstructs the verified blob.
pub struct Reassembler {
    nchunks: u32,
    expected_sha: String,
    got: BTreeMap<u32, Vec<u8>>,
}

impl Reassembler {
    pub fn new(nchunks: u32, expected_sha: impl Into<String>) -> Self {
        Reassembler {
            nchunks,
            expected_sha: expected_sha.into(),
            got: BTreeMap::new(),
        }
    }

    /// Verify and store one chunk. Returns the chunk's seq on success.
    pub fn accept(&mut self, seq: u32, b64: &str, sum: &str) -> Result<u32, ChunkError> {
        if chunk_sum(b64) != sum {
            return Err(ChunkError::BadSum { seq });
        }
        let raw = B64.decode(b64).map_err(|_| ChunkError::BadBase64 { seq })?;
        self.got.insert(seq, raw);
        Ok(seq)
    }

    /// Number of contiguous chunks held from seq 0 — the resume high-water mark the agent
    /// reports via HAVE (host resends from here).
    pub fn contiguous_have(&self) -> u32 {
        let mut hw = 0u32;
        while self.got.contains_key(&hw) {
            hw += 1;
        }
        hw
    }

    pub fn missing(&self) -> Vec<u32> {
        (0..self.nchunks)
            .filter(|s| !self.got.contains_key(s))
            .collect()
    }

    pub fn is_complete(&self) -> bool {
        self.got.len() as u32 == self.nchunks
    }

    /// Concatenate in order and verify the full sha256.
    pub fn finish(self) -> Result<Vec<u8>, ChunkError> {
        let missing = (0..self.nchunks)
            .filter(|s| !self.got.contains_key(s))
            .collect::<Vec<_>>();
        if !missing.is_empty() {
            return Err(ChunkError::Incomplete { missing });
        }
        let mut out = Vec::new();
        for seq in 0..self.nchunks {
            out.extend_from_slice(&self.got[&seq]);
        }
        let got = sha256_hex(&out);
        if got != self.expected_sha {
            return Err(ChunkError::Sha256Mismatch {
                expected: self.expected_sha,
                got,
            });
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reassemble_ok(data: &[u8], chunk_size: usize) -> Vec<u8> {
        let blob = prepare(data, chunk_size);
        let mut r = Reassembler::new(blob.nchunks(), blob.sha256.clone());
        for c in &blob.chunks {
            r.accept(c.seq, &c.b64, &c.sum).unwrap();
        }
        assert!(r.is_complete());
        r.finish().unwrap()
    }

    #[test]
    fn roundtrip_exact_bytes() {
        let data = b"the quick brown fox jumps over the lazy dog, 0123456789";
        assert_eq!(reassemble_ok(data, 8), data);
        assert_eq!(reassemble_ok(data, 1), data);
        assert_eq!(reassemble_ok(data, 1000), data);
    }

    #[test]
    fn roundtrip_binary() {
        let data: Vec<u8> = (0..=255u8).cycle().take(5000).collect();
        assert_eq!(reassemble_ok(&data, 512), data);
    }

    #[test]
    fn empty_blob() {
        let blob = prepare(b"", 16);
        assert_eq!(blob.nchunks(), 0);
        let r = Reassembler::new(0, blob.sha256);
        assert!(r.is_complete());
        assert_eq!(r.finish().unwrap(), b"");
    }

    #[test]
    fn corrupt_chunk_rejected_by_sum() {
        let blob = prepare(b"hello world", 4);
        let mut r = Reassembler::new(blob.nchunks(), blob.sha256);
        let c = &blob.chunks[0];
        // wrong sum
        assert_eq!(
            r.accept(c.seq, &c.b64, "0000000000000000"),
            Err(ChunkError::BadSum { seq: 0 })
        );
    }

    #[test]
    fn corrupt_base64_rejected() {
        // craft a b64 whose sum matches but doesn't decode
        let bad = "!!!!";
        let sum = chunk_sum(bad);
        let mut r = Reassembler::new(1, "x");
        assert_eq!(
            r.accept(0, bad, &sum),
            Err(ChunkError::BadBase64 { seq: 0 })
        );
    }

    #[test]
    fn missing_chunks_reported() {
        let blob = prepare(b"abcdefgh", 2); // 4 chunks
        let mut r = Reassembler::new(blob.nchunks(), blob.sha256);
        r.accept(0, &blob.chunks[0].b64, &blob.chunks[0].sum)
            .unwrap();
        r.accept(2, &blob.chunks[2].b64, &blob.chunks[2].sum)
            .unwrap();
        assert_eq!(r.missing(), vec![1, 3]);
        assert!(!r.is_complete());
        assert_eq!(
            r.finish(),
            Err(ChunkError::Incomplete {
                missing: vec![1, 3]
            })
        );
    }

    #[test]
    fn wrong_expected_sha_caught_at_finish() {
        let blob = prepare(b"payload", 3);
        let mut r = Reassembler::new(blob.nchunks(), "deadbeef".to_string());
        for c in &blob.chunks {
            r.accept(c.seq, &c.b64, &c.sum).unwrap();
        }
        assert!(matches!(r.finish(), Err(ChunkError::Sha256Mismatch { .. })));
    }
}
