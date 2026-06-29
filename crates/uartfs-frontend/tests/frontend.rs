// SPDX-License-Identifier: Apache-2.0
//
// Drive the compiled front-end binary over a pty with the SAME host Transport/commands that
// drive the shell agent — proving the device-side protocol implementation is interchangeable.

use std::ffi::CStr;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::io::{AsRawFd, FromRawFd};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use uartfs::commands;
use uartfs::hash::sha256_hex;
use uartfs::transport::{Link, Timeouts, Transport};

fn open_pty() -> (File, String) {
    unsafe {
        let m = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
        assert!(m >= 0);
        assert_eq!(libc::grantpt(m), 0);
        assert_eq!(libc::unlockpt(m), 0);
        let name = libc::ptsname(m);
        assert!(!name.is_null());
        (File::from_raw_fd(m), CStr::from_ptr(name).to_string_lossy().into_owned())
    }
}

struct Frontend {
    master: File,
    dir: PathBuf,
    child: Child,
}
impl Drop for Frontend {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn spawn_frontend() -> Frontend {
    let (master, slave_path) = open_pty();
    let slave = OpenOptions::new().read(true).write(true).open(&slave_path).unwrap();
    let dir = std::env::temp_dir().join(format!("uartfs-fe-{}", std::process::id()));
    let bin = env!("CARGO_BIN_EXE_uartfs-frontend");
    let child = Command::new(bin)
        .env("UARTFS_DIR", &dir)
        .stdin(slave.try_clone().unwrap())
        .stdout(slave.try_clone().unwrap())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    Frontend { master, dir, child }
}

struct PtyLink {
    master: File,
}
impl PtyLink {
    fn new(master: File) -> Self {
        let fd = master.as_raw_fd();
        unsafe {
            let fl = libc::fcntl(fd, libc::F_GETFL);
            libc::fcntl(fd, libc::F_SETFL, fl | libc::O_NONBLOCK);
        }
        PtyLink { master }
    }
}
impl Link for PtyLink {
    fn send_line(&mut self, line: &str) -> std::io::Result<()> {
        self.master.write_all(line.as_bytes())?;
        self.master.write_all(b"\n")?;
        self.master.flush()
    }
    fn read_bytes(&mut self) -> std::io::Result<Vec<u8>> {
        let mut out = Vec::new();
        let mut b = [0u8; 4096];
        loop {
            match self.master.read(&mut b) {
                Ok(0) => break,
                Ok(n) => out.extend_from_slice(&b[..n]),
                Err(_) => break,
            }
        }
        Ok(out)
    }
}

fn timeouts() -> Timeouts {
    Timeouts {
        ack: Duration::from_secs(2),
        done: Duration::from_secs(5),
        exec: Duration::from_secs(10),
        ready: Duration::from_secs(5),
        poll: Duration::from_millis(5),
        ..Timeouts::default()
    }
}

#[test]
fn frontend_handshakes() {
    let fe = spawn_frontend();
    let mut t = Transport::with_timeouts(PtyLink::new(fe.master.try_clone().unwrap()), timeouts());
    assert_eq!(t.ping().unwrap(), "fe1");
}

#[test]
fn frontend_exec() {
    let fe = spawn_frontend();
    let mut t = Transport::with_timeouts(PtyLink::new(fe.master.try_clone().unwrap()), timeouts());
    t.ping().unwrap();
    let r = t.exec("printf 'hi %s\\n' there").unwrap();
    assert_eq!(r.code, 0);
    assert_eq!(r.stdout, b"hi there\n");
    let r2 = t.exec("exit 9").unwrap();
    assert_eq!(r2.code, 9);
}

#[test]
fn frontend_blob_transfer() {
    let fe = spawn_frontend();
    let mut t = Transport::with_timeouts(PtyLink::new(fe.master.try_clone().unwrap()), timeouts());
    t.ping().unwrap();
    let data: Vec<u8> = (0..4000u32).map(|i| (i % 256) as u8).collect();
    let sha = t.send_blob(3, &data, 1024).unwrap();
    assert_eq!(sha, sha256_hex(&data));
    assert_eq!(std::fs::read(fe.dir.join("3/out")).unwrap(), data);
}

#[test]
fn frontend_push_command() {
    let fe = spawn_frontend();
    let mut t = Transport::with_timeouts(PtyLink::new(fe.master.try_clone().unwrap()), timeouts());
    t.ping().unwrap();
    let data = b"delivered to the compiled front-end".to_vec();
    let remote = fe.dir.join("pushed.bin");
    commands::push(&mut t, &data, remote.to_str().unwrap(), false, 512, 4, fe.dir.to_str().unwrap())
        .unwrap();
    assert_eq!(std::fs::read(&remote).unwrap(), data);
}
