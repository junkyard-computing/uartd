// SPDX-License-Identifier: Apache-2.0
//
// The technician operations, built from two transport primitives — `send_blob` (reliable,
// verified delivery into a device temp file) and `exec` (run a verified command). Everything
// here is `Transport<L>`-generic, so it is integration-tested against the real shell agent
// over a pty (see tests/agent.rs) as well as usable over the live uartd link.

use crate::delta::Codec;
use crate::transport::{ExecResult, Link, Transport, TransportError};

type Result<T> = std::result::Result<T, TransportError>;

/// The agent's default base directory (its `UARTFS_DIR`).
pub const DEFAULT_DEVICE_DIR: &str = "/tmp/uartfs";

/// Device-side path where the agent reconstructs transfer `xid`, under base dir `device_dir`.
pub fn blob_path(device_dir: &str, xid: u32) -> String {
    format!("{device_dir}/{xid}/out")
}

/// Single-quote a string for safe interpolation into a `sh -c` command.
pub fn shq(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn sudo_prefix(sudo: bool) -> &'static str {
    if sudo { "sudo " } else { "" }
}

fn require_zero(r: ExecResult, what: &str) -> Result<ExecResult> {
    if r.code != 0 {
        let err = String::from_utf8_lossy(&r.stderr).trim().to_string();
        return Err(TransportError::Protocol(format!(
            "{what} failed on device (exit {}): {err}",
            r.code
        )));
    }
    Ok(r)
}

/// Run a command on the device and return its result.
pub fn run<L: Link>(t: &mut Transport<L>, command: &str) -> Result<ExecResult> {
    t.exec(command)
}

/// Push a local file to `remote` on the device: compress, deliver (verified), decompress +
/// sha-gate on-device, copy into place, then read-back-verify the destination's sha256 against
/// the original (uncompressed) data. Compression cuts the bytes that cross the slow line.
#[allow(clippy::too_many_arguments)]
pub fn push<L: Link>(
    t: &mut Transport<L>,
    data: &[u8],
    remote: &str,
    sudo: bool,
    chunk: usize,
    xid: u32,
    device_dir: &str,
) -> Result<String> {
    let (sha, raw) = deliver_compressed(t, data, sudo, chunk, xid, device_dir)?;
    let sp = sudo_prefix(sudo);
    let cp = format!(
        "{sp}sh -c {}",
        shq(&format!(
            "mkdir -p \"$(dirname {dst})\" && cp {src} {dst}",
            dst = shq(remote),
            src = shq(&raw)
        ))
    );
    require_zero(t.exec(&cp)?, "copy into place")?;
    verify_remote_sha(t, remote, &sha, sudo)?;
    Ok(sha)
}

/// Pull a remote file or partition slice into memory. `spec` is either a path, or
/// `partlabel:offset:len` to read a byte range of a partition.
pub fn pull<L: Link>(t: &mut Transport<L>, spec: &str, sudo: bool) -> Result<Vec<u8>> {
    let sp = sudo_prefix(sudo);
    let cmd = match parse_part_slice(spec) {
        Some((label, off, len)) => format!(
            "{sp}dd if={dev} bs=1 skip={off} count={len} status=none",
            dev = shq(&format!("/dev/disk/by-partlabel/{label}")),
        ),
        None => format!("{sp}cat {}", shq(spec)),
    };
    let r = require_zero(t.exec(&cmd)?, "pull")?;
    Ok(r.stdout)
}

/// Plan / report for a flash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlashReport {
    pub sha256: String,
    pub bytes: usize,
    pub target: String,
    pub written: bool,
}

/// Flash `image` to a device block target (a full `/dev/...` path). Delivers the verified
/// image, dd's it, then read-back-verifies the written region. Refuses to claim success on a
/// hash mismatch. `dry_run` reports the plan without writing.
#[allow(clippy::too_many_arguments)]
pub fn flash<L: Link>(
    t: &mut Transport<L>,
    image: &[u8],
    target: &str,
    sudo: bool,
    chunk: usize,
    xid: u32,
    device_dir: &str,
    dry_run: bool,
) -> Result<FlashReport> {
    let sha = crate::hash::sha256_hex(image);
    if dry_run {
        return Ok(FlashReport {
            sha256: sha,
            bytes: image.len(),
            target: target.to_string(),
            written: false,
        });
    }
    // compress + deliver + decompress + sha-gate (the verified raw file is what we dd)
    let (sha, raw) = deliver_compressed(t, image, sudo, chunk, xid, device_dir)?;
    let len = image.len();
    let sp = sudo_prefix(sudo);

    // Write, capturing dd's stderr so we can confirm it actually moved `len` bytes (a short
    // write — full target, EIO partway — otherwise reads as success here). Then read back
    // EXACTLY `len` bytes with dd (works on a block device, unlike head -c heuristics) and
    // compare the sha against the verified image.
    let write = write_and_readback_cmd(sp, &raw, target, len);
    let r = require_zero(t.exec(&write)?, "dd to target")?;
    let (wrote, rbsha) = parse_write_readback(&r.stdout)?;
    if wrote != len as u64 {
        return Err(TransportError::Verify(format!(
            "dd wrote {wrote} bytes, expected {len} — short write, target may be corrupted"
        )));
    }
    if rbsha != sha {
        return Err(TransportError::Verify(format!(
            "read-back sha {rbsha} != image sha {sha} — target may be corrupted"
        )));
    }
    Ok(FlashReport {
        sha256: sha,
        bytes: len,
        target: target.to_string(),
        written: true,
    })
}

/// Install a kernel module: deliver it, copy under `/lib/modules/<uname-r>/extra`, then either
/// `depmod` or `insmod` it.
#[allow(clippy::too_many_arguments)]
pub fn install_module<L: Link>(
    t: &mut Transport<L>,
    ko: &[u8],
    filename: &str,
    sudo: bool,
    chunk: usize,
    xid: u32,
    device_dir: &str,
    depmod: bool,
) -> Result<()> {
    let (sha, raw) = deliver_compressed(t, ko, sudo, chunk, xid, device_dir)?;
    let sp = sudo_prefix(sudo);

    // `name` is quoted with shq like every other interpolated value (it was previously
    // interpolated raw into the double-quoted "$d/{name}", a shell-injection / breakage hazard
    // for any module filename with a space or metacharacter).
    let install = format!(
        "{sp}sh -c {}",
        shq(&format!(
            "d=/lib/modules/$(uname -r)/extra; n={name}; mkdir -p \"$d\" && cp {src} \"$d/$n\" && printf '%s\\n' \"$d/$n\"",
            src = shq(&raw),
            name = shq(filename),
        ))
    );
    let r = require_zero(t.exec(&install)?, "install module")?;
    let path = String::from_utf8_lossy(&r.stdout).trim().to_string();

    // read-back verify the installed file
    verify_remote_sha(t, &path, &sha, sudo)?;

    if depmod {
        require_zero(t.exec(&format!("{sp}depmod"))?, "depmod")?;
    } else {
        require_zero(t.exec(&format!("{sp}insmod {}", shq(&path)))?, "insmod")?;
    }
    Ok(())
}

/// Check that the device has `zstd` available.
pub fn device_has_zstd<L: Link>(t: &mut Transport<L>) -> Result<bool> {
    let r = t.exec("command -v zstd >/dev/null 2>&1 && echo yes || echo no")?;
    Ok(String::from_utf8_lossy(&r.stdout).trim() == "yes")
}

/// Pick the best whole-payload codec the device supports: zstd if present, else gzip (always).
pub fn choose_codec<L: Link>(t: &mut Transport<L>) -> Result<Codec> {
    if device_has_zstd(t)? {
        Ok(Codec::Zstd)
    } else {
        Ok(Codec::Gzip)
    }
}

/// Deliver `data` compressed: pick a codec, ship the compressed bytes into the device temp
/// file, then decompress on-device to `<blob>.raw` and verify its sha256 == the RAW data's sha
/// (the sha-gate is on the decompressed image, never the compressed wire bytes). Returns
/// (raw_sha, raw_path) — the verified, decompressed file the caller then copies or dd's.
fn deliver_compressed<L: Link>(
    t: &mut Transport<L>,
    data: &[u8],
    sudo: bool,
    chunk: usize,
    xid: u32,
    device_dir: &str,
) -> Result<(String, String)> {
    let raw_sha = crate::hash::sha256_hex(data);
    let codec = choose_codec(t)?;
    let packed = crate::delta::compress(data, codec).map_err(TransportError::Io)?;

    // ship the COMPRESSED bytes (this is the wire savings)
    t.send_blob(xid, &packed, chunk)?;
    let blob = blob_path(device_dir, xid);
    let raw = format!("{blob}.raw");
    let sp = sudo_prefix(sudo);

    // decompress on-device, then sha-gate the DECOMPRESSED image before any use
    let recon = format!(
        "{sp}sh -c {}",
        shq(&format!(
            "{decomp} && sha256sum {raw} | cut -c1-64",
            decomp = codec.device_decompress_cmd(&blob, &raw),
            raw = shq(&raw),
        ))
    );
    let r = require_zero(t.exec(&recon)?, "decompress")?;
    let got = String::from_utf8_lossy(&r.stdout).trim().to_string();
    if got != raw_sha {
        return Err(TransportError::Verify(format!(
            "decompressed sha {got} != expected {raw_sha} — refusing to use payload"
        )));
    }
    Ok((raw_sha, raw))
}

/// Delta-flash: ship only a zstd patch of (base -> new) and reconstruct on-device against the
/// current partition content (which must equal `base`). Verifies the device base matches,
/// reconstructs, sha-checks, dd's, and read-back-verifies. Returns the flash report.
#[allow(clippy::too_many_arguments)]
pub fn flash_delta<L: Link>(
    t: &mut Transport<L>,
    base_path: &std::path::Path,
    new_path: &std::path::Path,
    target: &str,
    sudo: bool,
    chunk: usize,
    xid: u32,
    device_dir: &str,
) -> Result<FlashReport> {
    let base = std::fs::read(base_path).map_err(TransportError::Io)?;
    let new = std::fs::read(new_path).map_err(TransportError::Io)?;
    let base_sha = crate::hash::sha256_hex(&base);
    let new_sha = crate::hash::sha256_hex(&new);
    let base_len = base.len();
    let new_len = new.len();
    let sp = sudo_prefix(sudo);

    if !device_has_zstd(t)? {
        return Err(TransportError::Protocol(
            "zstd not found on device (push it, or use a full flash)".into(),
        ));
    }

    // The device's current base must match what we diffed against, else reconstruction is garbage.
    let check = format!(
        "{sp}sh -c {}",
        shq(&format!(
            "head -c {base_len} {dst} | sha256sum | cut -c1-64",
            dst = shq(target)
        ))
    );
    let r = require_zero(t.exec(&check)?, "read device base")?;
    let got_base = String::from_utf8_lossy(&r.stdout).trim().to_string();
    if got_base != base_sha {
        return Err(TransportError::Verify(format!(
            "device base sha {got_base} != expected {base_sha}; use a full flash"
        )));
    }

    // ship the patch
    let patch = crate::delta::make_patch(base_path, new_path).map_err(TransportError::Io)?;
    t.send_blob(xid, &patch, chunk)?;
    let patch_path = blob_path(device_dir, xid);
    let base_file = format!("{device_dir}/{xid}.base");
    let new_file = format!("{device_dir}/{xid}.new");

    // reconstruct against an exact-length copy of the device base, then verify
    let recon = format!(
        "{sp}sh -c {}",
        shq(&format!(
            "head -c {base_len} {dst} > {bf} && {zstd} && head -c {new_len} {nf} | sha256sum | cut -c1-64",
            dst = shq(target),
            bf = shq(&base_file),
            nf = shq(&new_file),
            zstd = crate::delta::device_reconstruct_cmd(&base_file, &patch_path, &new_file),
        ))
    );
    let r = require_zero(t.exec(&recon)?, "reconstruct")?;
    let got_new = String::from_utf8_lossy(&r.stdout).trim().to_string();
    if got_new != new_sha {
        return Err(TransportError::Verify(format!(
            "reconstructed sha {got_new} != new sha {new_sha}"
        )));
    }

    // write + read-back verify (robust: verify dd moved new_len bytes, read back exactly that)
    let write = write_and_readback_cmd(sp, &new_file, target, new_len);
    let r = require_zero(t.exec(&write)?, "write + read-back")?;
    let (wrote, rb) = parse_write_readback(&r.stdout)?;
    if wrote != new_len as u64 {
        return Err(TransportError::Verify(format!(
            "dd wrote {wrote} bytes, expected {new_len} — short write, target may be corrupted"
        )));
    }
    if rb != new_sha {
        return Err(TransportError::Verify(format!(
            "read-back sha {rb} != new sha {new_sha} — target may be corrupted"
        )));
    }

    // tidy up scratch files
    let _ = t.exec(&format!("rm -f {} {}", shq(&base_file), shq(&new_file)));

    Ok(FlashReport {
        sha256: new_sha,
        bytes: new_len,
        target: target.to_string(),
        written: true,
    })
}

/// Build the device-side write+read-back command. dd's stderr (which records "N bytes copied")
/// is parsed so a short write is caught; the read-back uses dd with an exact byte count so it
/// is correct on a block device (not `head -c`). Emits two stdout lines: `WROTE <n>` and the
/// read-back sha256.
fn write_and_readback_cmd(sp: &str, src: &str, dst: &str, len: usize) -> String {
    // `dd ... 2>&1` captures the "N bytes copied" line; we grep the byte count out of it.
    // `iflag=count_bytes` lets us read back EXACTLY `len` bytes regardless of block alignment
    // (coreutils dd on the Debian rootfs supports it; works on block devices unlike head -c).
    format!(
        "{sp}sh -c {}",
        shq(&format!(
            "w=$(dd if={src} of={dst} bs=1M conv=notrunc 2>&1) && sync && \
             n=$(printf '%s\\n' \"$w\" | grep -o '[0-9]\\+ bytes' | head -n1 | grep -o '[0-9]\\+') && \
             printf 'WROTE %s\\n' \"${{n:-0}}\" && \
             dd if={dst} bs=1M count={len} iflag=count_bytes 2>/dev/null | sha256sum | cut -c1-64",
            src = shq(src),
            dst = shq(dst),
            len = len,
        ))
    )
}

/// Parse the two-line output of `write_and_readback_cmd`: `WROTE <n>\n<sha>`.
fn parse_write_readback(stdout: &[u8]) -> Result<(u64, String)> {
    let text = String::from_utf8_lossy(stdout);
    let mut wrote: Option<u64> = None;
    let mut sha: Option<String> = None;
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("WROTE ") {
            wrote = rest.trim().parse().ok();
        } else if line.len() == 64 && line.bytes().all(|b| b.is_ascii_hexdigit()) {
            sha = Some(line.to_string());
        }
    }
    match (wrote, sha) {
        (Some(w), Some(s)) => Ok((w, s)),
        _ => Err(TransportError::Verify(format!(
            "could not parse write/read-back output: {text:?}"
        ))),
    }
}

fn verify_remote_sha<L: Link>(
    t: &mut Transport<L>,
    remote: &str,
    expected: &str,
    sudo: bool,
) -> Result<()> {
    let sp = sudo_prefix(sudo);
    let cmd = format!(
        "{sp}sh -c {}",
        shq(&format!("sha256sum {} | cut -c1-64", shq(remote)))
    );
    let r = require_zero(t.exec(&cmd)?, "read-back sha256")?;
    let got = String::from_utf8_lossy(&r.stdout).trim().to_string();
    if got != expected {
        return Err(TransportError::Verify(format!(
            "remote {remote} sha {got} != expected {expected}"
        )));
    }
    Ok(())
}

/// Parse `label:offset:len` into (label, offset, len). Returns None for a plain path.
fn parse_part_slice(spec: &str) -> Option<(String, u64, u64)> {
    let parts: Vec<&str> = spec.split(':').collect();
    if parts.len() == 3 {
        let off = parts[1].parse().ok()?;
        let len = parts[2].parse().ok()?;
        Some((parts[0].to_string(), off, len))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shquote_escapes_quotes() {
        assert_eq!(shq("plain"), "'plain'");
        assert_eq!(shq("a'b"), "'a'\\''b'");
    }

    #[test]
    fn blob_path_matches_agent_convention() {
        assert_eq!(blob_path(DEFAULT_DEVICE_DIR, 7), "/tmp/uartfs/7/out");
    }

    #[test]
    fn parse_write_readback_happy() {
        let out = b"WROTE 5000\nabc123def4567890abc123def4567890abc123def4567890abc123def4567890\n";
        let (w, sha) = parse_write_readback(out).unwrap();
        assert_eq!(w, 5000);
        assert_eq!(
            sha,
            "abc123def4567890abc123def4567890abc123def4567890abc123def4567890"
        );
    }

    #[test]
    fn parse_write_readback_short_write_detected() {
        // dd reported fewer bytes than expected: the flash code compares this against `len`
        // and bails. Here we just confirm we parse the (smaller) count out.
        let out = b"WROTE 100\n0000000000000000000000000000000000000000000000000000000000000000\n";
        let (w, _sha) = parse_write_readback(out).unwrap();
        assert_eq!(w, 100);
    }

    #[test]
    fn parse_write_readback_missing_fields_errors() {
        assert!(parse_write_readback(b"WROTE 5\n").is_err()); // no sha line
        assert!(parse_write_readback(b"garbage\n").is_err());
    }

    #[test]
    fn write_readback_cmd_uses_dd_not_head_for_readback() {
        let cmd = write_and_readback_cmd("", "/tmp/x.raw", "/dev/block/by-name/boot_a", 4096);
        // read-back must use dd with an exact byte count (correct on block devices),
        // never `head -c`.
        assert!(cmd.contains("iflag=count_bytes"));
        assert!(cmd.contains("count=4096"));
        assert!(!cmd.contains("head -c"));
        // and it must verify dd's own byte count
        assert!(cmd.contains("WROTE"));
    }

    #[test]
    fn gzip_codec_roundtrips() {
        // the device decompress command for gzip must reproduce the original bytes
        let data: Vec<u8> = (0..2000u32).map(|i| (i % 256) as u8).collect();
        let packed = crate::delta::compress(&data, Codec::Gzip).unwrap();
        assert!(packed.len() < data.len(), "should actually compress");
        let dir = std::env::temp_dir().join(format!("uartfs-gzip-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("p.gz");
        let dst = dir.join("p.raw");
        std::fs::write(&src, &packed).unwrap();
        let cmd =
            Codec::Gzip.device_decompress_cmd(src.to_str().unwrap(), dst.to_str().unwrap());
        let st = std::process::Command::new("sh")
            .arg("-c")
            .arg(&cmd)
            .status()
            .unwrap();
        assert!(st.success());
        assert_eq!(std::fs::read(&dst).unwrap(), data);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn part_slice_parsing() {
        assert_eq!(
            parse_part_slice("vendor_boot_a:0:1024"),
            Some(("vendor_boot_a".to_string(), 0, 1024))
        );
        assert_eq!(parse_part_slice("/etc/hostname"), None);
        assert_eq!(parse_part_slice("a:b:c"), None);
    }
}
