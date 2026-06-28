// SPDX-License-Identifier: Apache-2.0
//
// Test harness: run the real phone-side shell agent as a subprocess attached to a pty, and
// give the host transport a Link over the pty master. This exercises the actual agent script
// (base64/sha256sum/dd shell logic) end-to-end without any hardware.

use std::ffi::CStr;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::io::{AsRawFd, FromRawFd};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use uartfs::transport::Link;

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn open_pty() -> (File, String) {
    unsafe {
        let master = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
        assert!(master >= 0);
        assert_eq!(libc::grantpt(master), 0);
        assert_eq!(libc::unlockpt(master), 0);
        let name = libc::ptsname(master);
        assert!(!name.is_null());
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
    (std::process::id() as u64).wrapping_mul(1_000_003) ^ t ^ (n << 40)
}

/// A running agent: the spawned shell process plus the unique dir it stores blobs under.
pub struct Agent {
    pub master: File,
    pub dir: PathBuf,
    child: Child,
}

impl Drop for Agent {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Spawn the agent script attached to a fresh pty; returns the Agent (master side).
pub fn spawn_agent() -> Agent {
    let dir = std::env::temp_dir().join(format!("uartfs-test-{}", unique()));
    spawn_agent_in(dir)
}

/// Spawn the agent against a specific scratch dir (used to model a device reboot: kill the
/// agent, then respawn it over the SAME dir so persisted chunks survive for resume).
pub fn spawn_agent_in(dir: PathBuf) -> Agent {
    let (master, slave_path) = open_pty();
    let slave = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&slave_path)
        .expect("open pty slave");

    let agent = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("agent/uartfs-agent.sh");

    let child = Command::new("sh")
        .arg(&agent)
        .env("UARTFS_DIR", &dir)
        .stdin(slave.try_clone().unwrap())
        .stdout(slave.try_clone().unwrap())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn agent");

    Agent { master, dir, child }
}

impl Agent {
    /// Kill the running agent process but KEEP its scratch dir (models a device reboot).
    pub fn kill_keep_dir(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A Link over the pty master.
pub struct PtyLink {
    master: File,
}

impl PtyLink {
    pub fn new(master: File) -> Self {
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
        let mut buf = [0u8; 4096];
        loop {
            match self.master.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => out.extend_from_slice(&buf[..n]),
                Err(_) => break, // would-block: nothing more right now
            }
        }
        Ok(out)
    }
}
