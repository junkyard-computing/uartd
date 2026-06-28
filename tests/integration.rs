// SPDX-License-Identifier: Apache-2.0
//
// End-to-end daemon tests over a pty (no real hardware). Each maps to an acceptance criterion
// in prompts/uartd.md.

mod common;

use std::fs;
use std::io::Write;
use std::sync::Arc;
use std::time::Duration;

use common::*;
use uartd::clock::SystemClock;
use uartd::config::Config;
use uartd::daemon::Daemon;
use uartd::proto::{Request, Response};

fn start(cfg: Config) -> Daemon {
    Daemon::start(cfg, Arc::new(SystemClock::new())).expect("daemon start")
}

// AC1 + AC2: status connected; lines captured with timestamps then cleared; log retains them.
#[test]
fn capture_read_clears_log_retains() {
    let (mut master, slave) = open_pty();
    let cfg = test_config(slave);
    let socket = cfg.socket_path.clone();
    let d = start(cfg);

    wait_connected(&socket, Duration::from_secs(3));

    master.write_all(b"hello world\n").unwrap();
    master.flush().unwrap();

    // peek until it shows up (non-destructive), then read drains it
    wait_for_text(&socket, "hello world", Duration::from_secs(3));

    match req(&socket, Request::Read) {
        Response::Read { text, lines, .. } => {
            assert!(text.contains("hello world"), "got {text:?}");
            assert!(
                lines.iter().any(|l| l.text == "hello world" && l.wall_ms > 0),
                "expected a timestamped line, got {lines:?}"
            );
        }
        other => panic!("unexpected: {other:?}"),
    }

    // immediate second read is empty (drain semantics)
    match req(&socket, Request::Read) {
        Response::Read { text, lines, .. } => {
            assert_eq!(text, "");
            assert!(lines.is_empty());
        }
        other => panic!("unexpected: {other:?}"),
    }

    // forensic log still has it
    let log = fs::read_to_string(d.log_path()).unwrap();
    assert!(log.contains("hello world"), "log was: {log}");

    d.shutdown();
}

// AC3: send a command and expect a marker; a 20-line block is not dropped.
#[test]
fn send_expect_roundtrip_and_no_drops_on_block() {
    let (mut master, slave) = open_pty();
    let cfg = test_config(slave);
    let socket = cfg.socket_path.clone();
    let d = start(cfg);
    wait_connected(&socket, Duration::from_secs(3));

    // A responder thread: echoes a prompt after seeing input, so --expect can match.
    let mut resp_dev = master.try_clone().unwrap();
    let responder = std::thread::spawn(move || {
        // read whatever the daemon sends, then reply with output + prompt
        let got = drain_master(&mut resp_dev, Duration::from_millis(300));
        let echoed = String::from_utf8_lossy(&got).into_owned();
        let _ = resp_dev.write_all(format!("{echoed}hello-back\n$ ").as_bytes());
        let _ = resp_dev.flush();
        echoed
    });

    match req(
        &socket,
        Request::Send {
            text: "echo hello".into(),
            no_newline: false,
            expect: Some(r"\$ ".into()),
            timeout_ms: Some(4000),
        },
    ) {
        Response::Match {
            matched,
            ref context,
            timed_out,
            ..
        } => {
            assert!(matched && !timed_out, "expected match");
            assert!(context.contains("hello-back"), "context: {context:?}");
        }
        other => panic!("unexpected: {other:?}"),
    }

    let echoed = responder.join().unwrap();
    assert!(echoed.contains("echo hello"), "daemon should have sent the command");

    // Now a 20-line block: every line must arrive intact at the device (no dropped chars).
    let block: String = (0..20).map(|i| format!("line{i:02}")).collect::<Vec<_>>().join("\n");
    req(
        &socket,
        Request::Send {
            text: block,
            no_newline: false,
            expect: None,
            timeout_ms: None,
        },
    );
    let received = drain_master(&mut master, Duration::from_millis(500));
    let received = String::from_utf8_lossy(&received);
    for i in 0..20 {
        assert!(received.contains(&format!("line{i:02}")), "missing line{i:02} in {received:?}");
    }

    d.shutdown();
}

// AC5 (wait) + timeout behavior.
#[test]
fn wait_matches_then_times_out() {
    let (mut master, slave) = open_pty();
    let cfg = test_config(slave);
    let socket = cfg.socket_path.clone();
    let d = start(cfg);
    wait_connected(&socket, Duration::from_secs(3));

    // wait in a thread; produce the line shortly after
    let sock2 = socket.clone();
    let waiter = std::thread::spawn(move || {
        req(
            &sock2,
            Request::Wait {
                pattern: "login:".into(),
                timeout_ms: 4000,
            },
        )
    });
    std::thread::sleep(Duration::from_millis(200));
    master.write_all(b"raspberrypi login: ").unwrap();
    master.flush().unwrap();

    match waiter.join().unwrap() {
        Response::Match { matched, timed_out, .. } => assert!(matched && !timed_out),
        other => panic!("unexpected: {other:?}"),
    }

    // a pattern that never appears times out
    match req(
        &socket,
        Request::Wait {
            pattern: "NEVER_EVER".into(),
            timeout_ms: 300,
        },
    ) {
        Response::Match { matched, timed_out, .. } => assert!(!matched && timed_out),
        other => panic!("unexpected: {other:?}"),
    }

    d.shutdown();
}

// AC4: port drops and returns; daemon logs a marker, resumes capture, never crashes.
#[test]
fn reconnects_after_port_drop() {
    // Use a symlink as the stable port path; repoint it across pty generations.
    let id = std::process::id();
    let link = std::env::temp_dir().join(format!("uartd-recon-{id}.link"));
    let _ = fs::remove_file(&link);

    let (mut master1, slave1) = open_pty();
    std::os::unix::fs::symlink(&slave1, &link).unwrap();

    let mut cfg = test_config(link.to_string_lossy().into_owned());
    cfg.reconnect_backoff = Duration::from_millis(50);
    let socket = cfg.socket_path.clone();
    let d = start(cfg);
    wait_connected(&socket, Duration::from_secs(3));

    master1.write_all(b"before-drop\n").unwrap();
    master1.flush().unwrap();
    wait_for_text(&socket, "before-drop", Duration::from_secs(3));
    req(&socket, Request::Read);

    // Drop the port: close master1 and remove the symlink target.
    drop(master1);
    let _ = fs::remove_file(&link);

    // daemon should notice disconnection
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        if let Response::Status { connected, .. } = req(&socket, Request::Status)
            && !connected
        {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "never disconnected");
        std::thread::sleep(Duration::from_millis(20));
    }

    // Bring it back on a new pty behind the same symlink.
    let (mut master2, slave2) = open_pty();
    std::os::unix::fs::symlink(&slave2, &link).unwrap();
    wait_connected(&socket, Duration::from_secs(3));

    master2.write_all(b"after-reconnect\n").unwrap();
    master2.flush().unwrap();
    let text = wait_for_text(&socket, "after-reconnect", Duration::from_secs(3));
    assert!(text.contains("after-reconnect"), "got {text:?}");

    let log = fs::read_to_string(d.log_path()).unwrap();
    assert!(log.contains("disconnected"), "log missing disconnect marker: {log}");
    assert!(log.contains("reconnected"), "log missing reconnect marker: {log}");

    d.shutdown();
    let _ = fs::remove_file(&link);
}

// AC5 (auto-login): with creds configured, the daemon logs in automatically and re-arms.
#[test]
fn auto_login_logs_in_and_rearms() {
    let (master, slave) = open_pty();
    let mut cfg = test_config(slave);
    cfg.login_user = Some("root".into());
    cfg.login_pass = Some("toor".into());
    let socket = cfg.socket_path.clone();
    let d = start(cfg);
    wait_connected(&socket, Duration::from_secs(3));

    let mut dev = master;
    dev.write_all(b"felix login: ").unwrap();
    dev.flush().unwrap();

    // daemon should send the username
    let sent = drain_master(&mut dev.try_clone().unwrap(), Duration::from_millis(400));
    let sent = String::from_utf8_lossy(&sent);
    assert!(sent.contains("root"), "expected username sent, got {sent:?}");

    dev.write_all(b"Password: ").unwrap();
    dev.flush().unwrap();
    let sent2 = drain_master(&mut dev.try_clone().unwrap(), Duration::from_millis(400));
    let sent2 = String::from_utf8_lossy(&sent2);
    assert!(sent2.contains("toor"), "expected password sent, got {sent2:?}");

    d.shutdown();
}
