// SPDX-License-Identifier: Apache-2.0
//
// sha256 helpers. The phone side computes the same hashes with `sha256sum`, so the wire
// values match byte-for-byte and the host can verify every payload before it is used.

use sha2::{Digest, Sha256};

/// Full lowercase hex sha256 of `data`.
pub fn sha256_hex(data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(data);
    hex(&h.finalize())
}

/// First `n` hex chars of the sha256 — a cheap per-chunk integrity tag.
pub fn sha256_prefix(data: &[u8], n: usize) -> String {
    let full = sha256_hex(data);
    full[..n.min(full.len())].to_string()
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_sha256() {
        // sha256("") and sha256("abc") are standard vectors
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn prefix_len() {
        assert_eq!(sha256_prefix(b"abc", 16).len(), 16);
        assert!(sha256_hex(b"abc").starts_with(&sha256_prefix(b"abc", 16)));
    }
}
