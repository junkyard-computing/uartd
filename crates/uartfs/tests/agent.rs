// SPDX-License-Identifier: Apache-2.0
//
// End-to-end tests of the host transport against the REAL phone-side shell agent over a pty.

mod common;

use std::time::Duration;

use common::{PtyLink, spawn_agent, spawn_agent_in};
use uartfs::chunk::prepare;
use uartfs::commands;
use uartfs::hash::sha256_hex;
use uartfs::msg::Msg;
use uartfs::transport::{Link, Timeouts, Transport};

fn timeouts() -> Timeouts {
    Timeouts {
        ack: Duration::from_secs(2),
        done: Duration::from_secs(5),
        exec: Duration::from_secs(10),
        ready: Duration::from_secs(5),
        poll: Duration::from_millis(5),
        retries: 6,
    }
}

#[test]
fn agent_handshakes() {
    let agent = spawn_agent();
    let master = agent.master.try_clone().unwrap();
    let mut t = Transport::with_timeouts(PtyLink::new(master), timeouts());
    assert_eq!(t.ping().unwrap(), "1");
}

#[test]
fn agent_receives_and_verifies_blob() {
    let agent = spawn_agent();
    let master = agent.master.try_clone().unwrap();
    let mut t = Transport::with_timeouts(PtyLink::new(master), timeouts());
    t.ping().unwrap();

    let data: Vec<u8> = (0..4096u32)
        .map(|i| (i.wrapping_mul(37) % 256) as u8)
        .collect();
    let sha = t.send_blob(1, &data, 1024).unwrap();
    assert_eq!(sha, sha256_hex(&data));

    // the agent reconstructed the exact bytes on disk
    let out = std::fs::read(agent.dir.join("1/out")).unwrap();
    assert_eq!(out, data);
}

#[test]
fn agent_exec_returns_stdout_and_code() {
    let agent = spawn_agent();
    let master = agent.master.try_clone().unwrap();
    let mut t = Transport::with_timeouts(PtyLink::new(master), timeouts());
    t.ping().unwrap();

    let r = t.exec("printf 'hello %s\\n' world").unwrap();
    assert_eq!(r.code, 0);
    assert_eq!(r.stdout, b"hello world\n");

    let r2 = t.exec("exit 7").unwrap();
    assert_eq!(r2.code, 7);
    assert!(r2.stdout.is_empty());
}

#[test]
fn agent_exec_larger_output_chunked() {
    let agent = spawn_agent();
    let master = agent.master.try_clone().unwrap();
    let mut t = Transport::with_timeouts(PtyLink::new(master), timeouts());
    t.ping().unwrap();

    // ~2 KB of output forces multiple OUT frames; host concatenates + verifies sha
    let r = t
        .exec("for i in $(seq 1 200); do printf 'line-%03d\\n' \"$i\"; done")
        .unwrap();
    assert_eq!(r.code, 0);
    let text = String::from_utf8(r.stdout).unwrap();
    assert!(text.contains("line-001"));
    assert!(text.contains("line-200"));
    assert_eq!(text.lines().count(), 200);
}

// push-then-apply: deliver a blob, then an EXEC moves it into place and reads it back —
// the shape of `uartfs push`.
#[test]
fn push_then_apply_roundtrip() {
    let agent = spawn_agent();
    let master = agent.master.try_clone().unwrap();
    let mut t = Transport::with_timeouts(PtyLink::new(master), timeouts());
    t.ping().unwrap();

    let payload = b"file contents delivered over a lossy uart, verified end to end".to_vec();
    t.send_blob(2, &payload, 16).unwrap();

    let dest = agent.dir.join("delivered.txt");
    let blob = agent.dir.join("2/out");
    let r = t
        .exec(&format!(
            "cp '{}' '{}' && cat '{}'",
            blob.display(),
            dest.display(),
            dest.display()
        ))
        .unwrap();
    assert_eq!(r.code, 0);
    assert_eq!(r.stdout, payload);
    assert_eq!(std::fs::read(&dest).unwrap(), payload);
}

// commands::push — deliver + copy into place + read-back-verify.
#[test]
fn command_push_with_readback_verify() {
    let agent = spawn_agent();
    let master = agent.master.try_clone().unwrap();
    let mut t = Transport::with_timeouts(PtyLink::new(master), timeouts());
    t.ping().unwrap();

    let data: Vec<u8> = (0..3000u32).map(|i| (i % 256) as u8).collect();
    let remote = agent.dir.join("pushed.bin");
    let sha = commands::push(
        &mut t,
        &data,
        remote.to_str().unwrap(),
        false,
        1024,
        11,
        agent.dir.to_str().unwrap(),
    )
    .unwrap();
    assert_eq!(sha, sha256_hex(&data));
    assert_eq!(std::fs::read(&remote).unwrap(), data);
}

// commands::flash to a regular file standing in for a partition; includes read-back verify.
#[test]
fn command_flash_to_file_target_verifies() {
    let agent = spawn_agent();
    let master = agent.master.try_clone().unwrap();
    let mut t = Transport::with_timeouts(PtyLink::new(master), timeouts());
    t.ping().unwrap();

    let image: Vec<u8> = (0..5000u32)
        .map(|i| (i.wrapping_mul(7) % 256) as u8)
        .collect();
    let target = agent.dir.join("fake_partition.img");
    // pre-fill the target with different bytes so the write is meaningful
    std::fs::write(&target, vec![0xAAu8; 6000]).unwrap();

    let report = commands::flash(
        &mut t,
        &image,
        target.to_str().unwrap(),
        false,
        1024,
        12,
        agent.dir.to_str().unwrap(),
        false,
    )
    .unwrap();
    assert!(report.written);
    assert_eq!(report.sha256, sha256_hex(&image));
    // the written region matches the image exactly
    let on_disk = std::fs::read(&target).unwrap();
    assert_eq!(&on_disk[..image.len()], &image[..]);
}

#[test]
fn command_flash_dry_run_does_not_write() {
    let agent = spawn_agent();
    let master = agent.master.try_clone().unwrap();
    let mut t = Transport::with_timeouts(PtyLink::new(master), timeouts());
    t.ping().unwrap();

    let image = b"would-be flashed".to_vec();
    let report = commands::flash(
        &mut t,
        &image,
        "/dev/null-nonexistent",
        false,
        64,
        13,
        agent.dir.to_str().unwrap(),
        true,
    )
    .unwrap();
    assert!(!report.written);
    assert_eq!(report.sha256, sha256_hex(&image));
}

// commands::flash_delta — ship only a zstd patch, reconstruct against the on-device base.
#[test]
fn command_flash_delta_reconstructs_and_verifies() {
    let agent = spawn_agent();
    let master = agent.master.try_clone().unwrap();
    let mut t = Transport::with_timeouts(PtyLink::new(master), timeouts());
    t.ping().unwrap();

    // base ~ new (a few bytes differ), like a dtb tweak between iterations
    let mut base: Vec<u8> = (0..40_000u32)
        .map(|i| (i.wrapping_mul(13) % 256) as u8)
        .collect();
    let base_path = agent.dir.join("base.img");
    std::fs::write(&base_path, &base).unwrap();
    for x in base.iter_mut().skip(20_000).take(200) {
        *x ^= 0xFF;
    }
    let new = base; // now mutated
    let new_path = agent.dir.join("new.img");
    std::fs::write(&new_path, &new).unwrap();

    // the "partition" starts out holding exactly the base bytes
    let target = agent.dir.join("partition.img");
    std::fs::write(&target, std::fs::read(&base_path).unwrap()).unwrap();

    let rep = commands::flash_delta(
        &mut t,
        &base_path,
        &new_path,
        target.to_str().unwrap(),
        false,
        1024,
        21,
        agent.dir.to_str().unwrap(),
    )
    .unwrap();
    assert!(rep.written);
    assert_eq!(rep.sha256, sha256_hex(&new));
    // the partition now holds the new image
    let on_disk = std::fs::read(&target).unwrap();
    assert_eq!(&on_disk[..new.len()], &new[..]);
}

#[test]
fn command_flash_delta_refuses_on_base_mismatch() {
    let agent = spawn_agent();
    let master = agent.master.try_clone().unwrap();
    let mut t = Transport::with_timeouts(PtyLink::new(master), timeouts());
    t.ping().unwrap();

    let base: Vec<u8> = (0..2000u32).map(|i| (i % 256) as u8).collect();
    let base_path = agent.dir.join("b.img");
    std::fs::write(&base_path, &base).unwrap();
    let new_path = agent.dir.join("n.img");
    std::fs::write(&new_path, vec![1u8; 2000]).unwrap();

    // target holds DIFFERENT bytes than base -> must refuse
    let target = agent.dir.join("p.img");
    std::fs::write(&target, vec![0x55u8; 2000]).unwrap();

    let err = commands::flash_delta(
        &mut t,
        &base_path,
        &new_path,
        target.to_str().unwrap(),
        false,
        1024,
        22,
        agent.dir.to_str().unwrap(),
    );
    assert!(err.is_err(), "should refuse when device base mismatches");
}

// Resume across a (simulated) device reboot against the REAL shell agent: deliver a prefix of
// chunks, kill the agent keeping its scratch dir, respawn over the same dir, then send_blob the
// full blob with the same xid. The agent must NOT have wiped the prefix on the first OPEN, must
// report it via HAVE on STAT, and the host must resume from there — not restart at chunk 0.
#[test]
fn agent_resumes_after_reboot_keeps_prefix() {
    let dir = std::env::temp_dir().join(format!(
        "uartfs-resume-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let xid = 77u32;
    let data: Vec<u8> = (0..4000u32)
        .map(|i| (i.wrapping_mul(53) % 256) as u8)
        .collect();
    let blob = prepare(&data, 256);
    assert!(blob.nchunks() > 4, "need several chunks for a meaningful prefix");

    // --- first agent: hand-deliver OPEN + the first 3 chunks, then "reboot" ---
    {
        let mut agent = spawn_agent_in(dir.clone());
        let master = agent.master.try_clone().unwrap();
        let mut t = Transport::with_timeouts(PtyLink::new(master), timeouts());
        t.ping().unwrap();

        let mut link = PtyLink::new(agent.master.try_clone().unwrap());
        link.send_line(
            &Msg::Open {
                xid,
                nchunks: blob.nchunks(),
                chunk_size: blob.chunk_size,
                sha256: blob.sha256.clone(),
            }
            .to_frame()
            .encode(),
        )
        .unwrap();
        for c in blob.chunks.iter().take(3) {
            link.send_line(
                &Msg::Data {
                    xid,
                    seq: c.seq,
                    b64: c.b64.clone(),
                    sum: c.sum.clone(),
                }
                .to_frame()
                .encode(),
            )
            .unwrap();
            // wait for the ACK so we know the chunk was persisted before we kill the agent
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            loop {
                let got = t.stat(xid).unwrap_or(0);
                if got > c.seq || std::time::Instant::now() >= deadline {
                    assert!(got > c.seq, "chunk {} not persisted before reboot", c.seq);
                    break;
                }
            }
        }
        // model a device reboot: agent dies, scratch dir survives
        agent.kill_keep_dir();
        std::mem::forget(agent); // don't run Drop (which would delete the dir)
    }

    // --- second agent over the SAME dir: resume ---
    let mut agent2 = spawn_agent_in(dir.clone());
    let master = agent2.master.try_clone().unwrap();
    let mut t = Transport::with_timeouts(PtyLink::new(master), timeouts());
    t.ping().unwrap();
    // the prefix must have survived the reboot
    assert_eq!(t.stat(xid).unwrap(), 3, "agent lost the resume prefix");

    let sha = t.send_blob(xid, &data, 256).unwrap();
    assert_eq!(sha, sha256_hex(&data));
    let out = std::fs::read(dir.join(format!("{xid}/out"))).unwrap();
    assert_eq!(out, data);
    // agent2's Drop cleans the dir
}

// commands::pull a file back into memory.
#[test]
fn command_pull_file() {
    let agent = spawn_agent();
    let master = agent.master.try_clone().unwrap();
    let mut t = Transport::with_timeouts(PtyLink::new(master), timeouts());
    t.ping().unwrap();

    let src = agent.dir.join("source.dat");
    let content: Vec<u8> = (0..1500u32).map(|i| (i % 256) as u8).collect();
    std::fs::write(&src, &content).unwrap();

    let got = commands::pull(&mut t, src.to_str().unwrap(), false).unwrap();
    assert_eq!(got, content);
}
