// SPDX-License-Identifier: Apache-2.0
//
// End-to-end tests of the host transport against the REAL phone-side shell agent over a pty.

mod common;

use std::time::Duration;

use common::{PtyLink, spawn_agent};
use uartfs::commands;
use uartfs::hash::sha256_hex;
use uartfs::transport::{Timeouts, Transport};

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

    let data: Vec<u8> = (0..4096u32).map(|i| (i.wrapping_mul(37) % 256) as u8).collect();
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
    let r = t.exec("for i in $(seq 1 200); do printf 'line-%03d\\n' \"$i\"; done").unwrap();
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
        .exec(&format!("cp '{}' '{}' && cat '{}'", blob.display(), dest.display(), dest.display()))
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

    let image: Vec<u8> = (0..5000u32).map(|i| (i.wrapping_mul(7) % 256) as u8).collect();
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
