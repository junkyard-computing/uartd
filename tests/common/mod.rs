// SPDX-License-Identifier: Apache-2.0
//
// Shared test harness: a pty pair standing in for the USB-serial device, unique socket/log
// paths per test, and small polling helpers. Writing to the returned master simulates the
// device talking; reading the master observes what the daemon sent.

use std::ffi::CStr;
use std::fs::File;
use std::io::Read;
use std::os::unix::io::FromRawFd;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use uartd::client::send_request;
use uartd::config::{Config, Parity};
use uartd::proto::{Request, Response};

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// Open a pty; return (master file, slave device path like /dev/pts/N).
pub fn open_pty() -> (File, String) {
    unsafe {
        let master = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
        assert!(master >= 0, "posix_openpt failed");
        assert_eq!(libc::grantpt(master), 0, "grantpt failed");
        assert_eq!(libc::unlockpt(master), 0, "unlockpt failed");
        let name = libc::ptsname(master);
        assert!(!name.is_null(), "ptsname failed");
        let path = CStr::from_ptr(name).to_string_lossy().into_owned();
        (File::from_raw_fd(master), path)
    }
}

fn unique() -> u64 {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst) as u64;
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64;
    (std::process::id() as u64).wrapping_mul(1_000_000) ^ t ^ (n << 48)
}

/// A config pointing at `port`, with unique socket/log paths and snappy timeouts for tests.
pub fn test_config(port: String) -> Config {
    let id = unique();
    let base = std::env::temp_dir().join(format!("uartd-test-{id}"));
    Config {
        port,
        baud: 115200,
        data_bits: 8,
        parity: Parity::N,
        stop_bits: 1,
        socket_path: PathBuf::from(format!("{}.sock", base.display())),
        log_dir: base,
        buffer_cap: 1 << 20,
        inter_line: Duration::from_millis(1),
        inter_char: Duration::ZERO,
        reconnect_backoff: Duration::from_millis(50),
        login_user: None,
        login_pass: None,
    }
}

pub fn req(socket: &std::path::Path, r: Request) -> Response {
    send_request(socket, &r).expect("request to daemon failed")
}

/// Poll `status` until connected, or panic after `timeout`.
pub fn wait_connected(socket: &std::path::Path, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(Response::Status { connected, .. }) = send_request(socket, &Request::Status) {
            if connected {
                return;
            }
        }
        if Instant::now() > deadline {
            panic!("daemon never reported connected within {timeout:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Poll `peek` (non-destructive) until the buffered text contains `needle`, or panic.
pub fn wait_for_text(socket: &std::path::Path, needle: &str, timeout: Duration) -> String {
    let deadline = Instant::now() + timeout;
    loop {
        if let Response::Read { text, .. } = req(socket, Request::Peek) {
            if text.contains(needle) {
                return text;
            }
        }
        if Instant::now() > deadline {
            panic!("never saw {needle:?} within {timeout:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Read all currently available bytes from the pty master (non-blocking-ish best effort).
pub fn drain_master(master: &mut File, settle: Duration) -> Vec<u8> {
    std::thread::sleep(settle);
    use std::os::unix::io::AsRawFd;
    let fd = master.as_raw_fd();
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
    }
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match master.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(_) => break,
        }
    }
    out
}
