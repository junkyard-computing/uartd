// SPDX-License-Identifier: Apache-2.0
//
// End-to-end tests of the device-self-verifying `uart run`/`uart login` (tier 1) against a REAL
// shell over a pty — including injected character drops, to prove the device-side checksum
// catches corruption and the host retries. No hardware needed.

use std::ffi::CStr;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::io::{AsRawFd, FromRawFd};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use uart_core::verified::{Console, RunOpts, login, run};

fn open_pty() -> (File, String) {
    unsafe {
        let m = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
        assert!(m >= 0);
        assert_eq!(libc::grantpt(m), 0);
        assert_eq!(libc::unlockpt(m), 0);
        let name = libc::ptsname(m);
        assert!(!name.is_null());
        let path = CStr::from_ptr(name).to_string_lossy().into_owned();
        (File::from_raw_fd(m), path)
    }
}

/// A console over a pty master, optionally dropping one character from the Nth send to simulate
/// a lossy line.
struct PtyConsole {
    master: File,
    drop_at_send: Option<u32>,
    sends: u32,
    _child: Child,
}

impl PtyConsole {
    fn spawn(cmd: &mut Command) -> Self {
        let (master, slave_path) = open_pty();
        let slave = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&slave_path)
            .unwrap();
        let child = cmd
            .stdin(slave.try_clone().unwrap())
            .stdout(slave.try_clone().unwrap())
            .stderr(slave.try_clone().unwrap())
            .spawn()
            .unwrap();
        let fd = master.as_raw_fd();
        unsafe {
            let fl = libc::fcntl(fd, libc::F_GETFL);
            libc::fcntl(fd, libc::F_SETFL, fl | libc::O_NONBLOCK);
        }
        PtyConsole {
            master,
            drop_at_send: None,
            sends: 0,
            _child: child,
        }
    }

    fn bash() -> Self {
        let mut c = Command::new("bash");
        c.arg("--norc").arg("--noprofile").arg("-i");
        Self::spawn(&mut c)
    }

    fn with_drop(mut self, n: u32) -> Self {
        self.drop_at_send = Some(n);
        self
    }
}

impl Drop for PtyConsole {
    fn drop(&mut self) {
        let _ = self._child.kill();
        let _ = self._child.wait();
    }
}

impl Console for PtyConsole {
    fn send(&mut self, text: &str, newline: bool) -> std::io::Result<()> {
        self.sends += 1;
        let mut payload = text.to_string();
        // simulate a dropped character on the targeted send (inside the D= base64 value)
        if Some(self.sends) == self.drop_at_send
            && let Some(pos) = payload.find("D=")
        {
            let cut = pos + 3;
            if cut < payload.len() {
                payload.remove(cut);
            }
        }
        self.master.write_all(payload.as_bytes())?;
        if newline {
            self.master.write_all(b"\n")?;
        }
        self.master.flush()
    }
    fn read(&mut self) -> std::io::Result<Vec<u8>> {
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

fn opts() -> RunOpts {
    RunOpts {
        timeout: Duration::from_secs(8),
        retries: 5,
        poll: Duration::from_millis(10),
    }
}

#[test]
fn run_against_real_bash() {
    let mut c = PtyConsole::bash();
    let r = run(&mut c, "echo hello-from-bash", &opts()).unwrap();
    assert_eq!(r.code, 0);
    // command substitution strips the trailing newline
    assert_eq!(r.stdout, b"hello-from-bash");
}

#[test]
fn run_propagates_exit_code() {
    let mut c = PtyConsole::bash();
    let r = run(&mut c, "exit 7", &opts()).unwrap();
    assert_eq!(r.code, 7);
    assert!(r.stdout.is_empty());
}

#[test]
fn run_multiline_output() {
    let mut c = PtyConsole::bash();
    let r = run(&mut c, "printf 'a\\nb\\nc\\n'", &opts()).unwrap();
    assert_eq!(r.stdout, b"a\nb\nc");
}

#[test]
fn run_recovers_from_dropped_char() {
    // drop a char inside the base64 on the FIRST command send -> device sha mismatch -> retry
    let mut c = PtyConsole::bash().with_drop(2); // send 1 is the Ctrl-U resync; send 2 is the cmd
    let r = run(&mut c, "echo survived", &opts()).unwrap();
    assert_eq!(r.code, 0);
    assert_eq!(r.stdout, b"survived");
}

#[test]
fn run_stress_many_calls() {
    // a few dozen calls in a row, each verified — the property the success criteria asks for
    let mut c = PtyConsole::bash();
    for i in 0..40 {
        let r = run(&mut c, &format!("echo n{i}"), &opts()).unwrap();
        assert_eq!(r.stdout, format!("n{i}").into_bytes());
        assert_eq!(r.code, 0);
    }
}

#[test]
fn login_then_run_against_fake_getty() {
    // a looping fake getty: prompt, read user, no-echo password, exec bash on the right creds
    let script = r#"
        while true; do
            printf 'fold login: '
            IFS= read -r user
            stty -echo 2>/dev/null
            printf 'Password: '
            IFS= read -r pass
            stty echo 2>/dev/null
            printf '\n'
            if [ "$user" = kalm ] && [ "$pass" = 0000 ]; then exec bash --norc --noprofile -i; fi
            printf 'Login incorrect\n'
        done
    "#;
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(script);
    cmd.stderr(Stdio::null());
    let mut c = PtyConsole::spawn(&mut cmd);

    login(&mut c, "kalm", "0000", &opts()).expect("login should succeed");
    // now a verified command works on the logged-in shell
    let r = run(&mut c, "echo logged-in", &opts()).unwrap();
    assert_eq!(r.stdout, b"logged-in");
}
