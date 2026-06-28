// SPDX-License-Identifier: Apache-2.0
//
// The technician operations, built from two transport primitives — `send_blob` (reliable,
// verified delivery into a device temp file) and `exec` (run a verified command). Everything
// here is `Transport<L>`-generic, so it is integration-tested against the real shell agent
// over a pty (see tests/agent.rs) as well as usable over the live uartd link.

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

/// Push a local file to `remote` on the device: deliver (verified), copy into place, then
/// read-back-verify the destination's sha256 against what was sent.
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
    let sha = t.send_blob(xid, data, chunk)?;
    let blob = blob_path(device_dir, xid);
    let sp = sudo_prefix(sudo);
    let cp = format!(
        "{sp}sh -c {}",
        shq(&format!(
            "mkdir -p \"$(dirname {dst})\" && cp {src} {dst}",
            dst = shq(remote),
            src = shq(&blob)
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
    let sha = t.send_blob(xid, image, chunk)?;
    let blob = blob_path(device_dir, xid);
    let sp = sudo_prefix(sudo);
    let write = format!(
        "{sp}sh -c {}",
        shq(&format!(
            "dd if={src} of={dst} bs=1M 2>/dev/null && sync",
            src = shq(&blob),
            dst = shq(target)
        ))
    );
    require_zero(t.exec(&write)?, "dd to target")?;

    // read back exactly the written length and compare
    let readback = format!(
        "{sp}sh -c {}",
        shq(&format!(
            "head -c {len} {dst} | sha256sum | cut -c1-64",
            len = image.len(),
            dst = shq(target)
        ))
    );
    let r = require_zero(t.exec(&readback)?, "read-back")?;
    let got = String::from_utf8_lossy(&r.stdout).trim().to_string();
    if got != sha {
        return Err(TransportError::Verify(format!(
            "read-back sha {got} != image sha {sha} — target may be corrupted"
        )));
    }
    Ok(FlashReport {
        sha256: sha,
        bytes: image.len(),
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
    let sha = t.send_blob(xid, ko, chunk)?;
    let blob = blob_path(device_dir, xid);
    let sp = sudo_prefix(sudo);

    let install = format!(
        "{sp}sh -c {}",
        shq(&format!(
            "d=/lib/modules/$(uname -r)/extra; mkdir -p \"$d\" && cp {src} \"$d/{name}\" && echo \"$d/{name}\"",
            src = shq(&blob),
            name = filename,
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

    // write + read-back verify
    let write = format!(
        "{sp}sh -c {}",
        shq(&format!(
            "dd if={nf} of={dst} bs=1M 2>/dev/null && sync && head -c {new_len} {dst} | sha256sum | cut -c1-64",
            nf = shq(&new_file),
            dst = shq(target),
        ))
    );
    let r = require_zero(t.exec(&write)?, "write + read-back")?;
    let rb = String::from_utf8_lossy(&r.stdout).trim().to_string();
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
    fn part_slice_parsing() {
        assert_eq!(
            parse_part_slice("vendor_boot_a:0:1024"),
            Some(("vendor_boot_a".to_string(), 0, 1024))
        );
        assert_eq!(parse_part_slice("/etc/hostname"), None);
        assert_eq!(parse_part_slice("a:b:c"), None);
    }
}
