// SPDX-License-Identifier: Apache-2.0
//
// Host-side binary delta via `zstd --patch-from`. The whole point of uartfs is to move KB, not
// MB, over the slow line: a new boot/vendor_boot image is ~99% identical to what's already on
// the device, so we ship only a patch of (base -> new) and reconstruct on-device against the
// base that already exists there (e.g. the live partition content).
//
// We use zstd because it is a single common tool that both compresses and patches; the device
// reconstructs with `zstd -d --patch-from`. `--long=27` gives a 128 MiB window (enough for the
// 34 MB vendor_boot) and must match on both sides.

use std::io;
use std::path::Path;
use std::process::Command;

/// zstd long-window log shared by patch creation and reconstruction.
pub const LONG: &str = "--long=27";

/// Produce a zstd patch that reconstructs `new` from `base`. Returns the patch bytes.
pub fn make_patch(base: &Path, new: &Path) -> io::Result<Vec<u8>> {
    let out = Command::new("zstd")
        .args([
            "-q",
            "-f",
            LONG,
            &format!("--patch-from={}", base.display()),
            "-19",
            "-c",
            new.to_str()
                .ok_or_else(|| io::Error::other("non-utf8 path"))?,
        ])
        .output()?;
    if !out.status.success() {
        return Err(io::Error::other(format!(
            "zstd patch failed: {}",
            String::from_utf8_lossy(&out.stderr)
        )));
    }
    Ok(out.stdout)
}

/// The device-side reconstruct command: rebuild `new_out` from `base_file` + `patch_file`.
/// (Used to build the shell command the agent runs.)
pub fn device_reconstruct_cmd(base_file: &str, patch_file: &str, new_out: &str) -> String {
    format!("zstd -q -f -d {LONG} --patch-from='{base_file}' '{patch_file}' -o '{new_out}'")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn patch_is_small_and_reconstructs() {
        // base and new differ in only a few bytes -> patch should be far smaller than new
        let dir = std::env::temp_dir().join(format!("uartfs-delta-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let base = dir.join("base.bin");
        let new = dir.join("new.bin");

        let mut b: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
        let mut f = std::fs::File::create(&base).unwrap();
        f.write_all(&b).unwrap();
        // tweak a few hundred bytes in the middle (like a dtb node change)
        for x in b.iter_mut().skip(50_000).take(300) {
            *x = 0xEE;
        }
        std::fs::write(&new, &b).unwrap();

        let patch = make_patch(&base, &new).unwrap();
        assert!(
            patch.len() < b.len() / 10,
            "patch {} not << new {}",
            patch.len(),
            b.len()
        );

        // reconstruct with the device command (run locally here)
        let patch_file = dir.join("patch.zst");
        std::fs::write(&patch_file, &patch).unwrap();
        let out = dir.join("out.bin");
        let cmd = device_reconstruct_cmd(
            base.to_str().unwrap(),
            patch_file.to_str().unwrap(),
            out.to_str().unwrap(),
        );
        let st = Command::new("sh").arg("-c").arg(&cmd).status().unwrap();
        assert!(st.success());
        assert_eq!(std::fs::read(&out).unwrap(), b);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
