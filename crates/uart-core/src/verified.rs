// SPDX-License-Identifier: Apache-2.0
//
// uart run / uart login — the device-self-verifying, agentless command floor (tier 1).
//
// The lossy serial console drops characters, and terminal echo is NOT a trustworthy receipt
// (passwords echo nothing; an interactive readline shell returns a redisplay, not a mirror; long
// lines wrap and redraw). So we never compare the echo. Instead the command carries its own
// checksum and refuses to run if corrupted, and the reply carries its own checksum so the host
// knows it got the output intact. Every failure mode collapses to "retry"; nothing can be
// mistaken for success, because success requires the random end-nonce AND a matching output sum.
//
// Needs only stock coreutils on the device (printf, base64, sha256sum, sh, cut, test) — no
// installed agent. Its job is to be the always-available floor and to bootstrap the tier-2
// console front-end. The one place echo is legitimately used is a dumb getty login line.

use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use sha2::{Digest, Sha256};

fn sha256_hex(data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(data);
    let d = h.finalize();
    let mut s = String::with_capacity(64);
    for b in d {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// A bidirectional console: send text (optionally with the executing newline) and read whatever
/// bytes have arrived. The daemon's socket is one impl ([`SocketConsole`]); tests use a pty.
pub trait Console {
    /// Send `text`; if `newline`, also send the line terminator that executes it.
    fn send(&mut self, text: &str, newline: bool) -> io::Result<()>;
    /// Return bytes received since the last read (may be empty).
    fn read(&mut self) -> io::Result<Vec<u8>>;

    fn send_line(&mut self, text: &str) -> io::Result<()> {
        self.send(text, true)
    }
}

static NONCE_CTR: AtomicU64 = AtomicU64::new(0);

/// A fresh, unpredictable nonce. Uniqueness is what matters (output is base64-wrapped so it can
/// never contain the sentinel), but we derive it from time+counter+pid and hash it anyway.
pub fn gen_nonce() -> String {
    let ctr = NONCE_CTR.fetch_add(1, Ordering::Relaxed);
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seed = format!("{}-{ctr}-{t}", std::process::id());
    sha256_hex(seed.as_bytes())[..16].to_string()
}

/// Exit code + message the device emits when the delivered command failed its on-device sha
/// check (i.e. it was corrupted in transit). The host treats this specific pair as "resend".
pub const CMD_CORRUPT_RC: i32 = 251;
pub const CMD_CORRUPT_MSG: &[u8] = b"cmd-corrupt";

/// Build the self-verifying one-liner for `cmd`, bracketed by `nonce`. Uses busybox-safe tools
/// (`base64 | tr -d '\n'`, not `base64 -w0`). `hash` is the device hashing command (sha256sum).
pub fn build_command(cmd: &str, nonce: &str) -> String {
    let d = B64.encode(cmd.as_bytes());
    let h = sha256_hex(d.as_bytes());
    format!(
        "D={d}; H={h}; printf '<<S:{nonce}>>\\n'; \
         if [ \"$(printf %s \"$D\"|sha256sum|cut -c1-64)\" = \"$H\" ]; then \
         OUT=$(printf %s \"$D\"|base64 -d|sh 2>&1); rc=$?; else OUT=cmd-corrupt; rc=251; fi; \
         B=$(printf %s \"$OUT\"|base64|tr -d '\\n'); \
         printf '<<E:{nonce}>>:%d:%s:%s\\n' \"$rc\" \"$(printf %s \"$B\"|sha256sum|cut -c1-64)\" \"$B\""
    )
}

/// Outcome of scanning the stream for this call's end-nonce.
#[derive(Debug, PartialEq, Eq)]
pub enum Parsed {
    /// No complete end-nonce line yet — keep reading.
    Pending,
    /// End-nonce seen and the output checksum verified.
    Ok { code: i32, stdout: Vec<u8> },
    /// End-nonce seen but the reply was corrupted in transit (output sha mismatch / bad b64).
    Corrupt,
}

/// Scan accumulated console text for `<<E:nonce>>:rc:sha:B\n` and verify the output checksum.
///
/// The interactive shell ECHOES the command we sent, and that echo literally contains
/// `<<E:nonce>>:%d:%s:%s` (the printf format) — so we must scan *every* occurrence of the
/// marker and skip ones that don't structurally parse (rc not an int, sha not 64 hex). Only a
/// structurally-valid line whose checksum mismatches is a genuinely corrupt reply.
pub fn parse_result(buf: &str, nonce: &str) -> Parsed {
    let marker = format!("<<E:{nonce}>>:");
    let mut from = 0;
    while let Some(rel) = buf[from..].find(&marker) {
        let start = from + rel + marker.len();
        from = start; // continue past this occurrence next iteration
        let rest = &buf[start..];
        let Some(nl) = rest.find('\n') else {
            return Parsed::Pending; // a marker with no newline yet — real line still arriving
        };
        // strip the CR from CRLF (pty onlcr / serial line endings) so it doesn't corrupt `B`
        let line = rest[..nl].trim_end_matches('\r');
        let mut it = line.splitn(3, ':');
        let (Some(rc_s), Some(sha), Some(b)) = (it.next(), it.next(), it.next()) else {
            continue;
        };
        let Ok(code) = rc_s.parse::<i32>() else {
            continue; // the echoed "%d" lands here
        };
        if sha.len() != 64 || !sha.bytes().all(|c| c.is_ascii_hexdigit()) {
            continue; // the echoed "%s"
        }
        // structurally a real result line — now it must verify or it's corrupt
        if sha256_hex(b.as_bytes()) != *sha {
            return Parsed::Corrupt;
        }
        return match B64.decode(b.as_bytes()) {
            Ok(stdout) => Parsed::Ok { code, stdout },
            Err(_) => Parsed::Corrupt,
        };
    }
    Parsed::Pending
}

#[derive(Debug, Clone)]
pub struct RunOpts {
    pub timeout: Duration,
    pub retries: u32,
    pub poll: Duration,
}

impl Default for RunOpts {
    fn default() -> Self {
        RunOpts {
            timeout: Duration::from_secs(10),
            retries: 4,
            poll: Duration::from_millis(30),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunResult {
    pub code: i32,
    pub stdout: Vec<u8>,
}

#[derive(Debug)]
pub enum RunError {
    Io(io::Error),
    /// Exhausted retries without a verified result.
    Failed(String),
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RunError::Io(e) => write!(f, "io: {e}"),
            RunError::Failed(s) => write!(f, "{s}"),
        }
    }
}
impl std::error::Error for RunError {}
impl From<io::Error> for RunError {
    fn from(e: io::Error) -> Self {
        RunError::Io(e)
    }
}

/// Run `cmd` on the device's bare shell, device-verified. Retries on any corruption/timeout.
pub fn run<C: Console>(c: &mut C, cmd: &str, opts: &RunOpts) -> Result<RunResult, RunError> {
    let mut last = String::new();
    for _ in 0..=opts.retries {
        let nonce = gen_nonce();
        // resync: kill any partial input line, then a fresh newline
        c.send_line("\u{15}")?;
        c.send_line(&build_command(cmd, &nonce))?;

        let mut buf = String::new();
        let deadline = Instant::now() + opts.timeout;
        loop {
            let bytes = c.read()?;
            if !bytes.is_empty() {
                buf.push_str(&String::from_utf8_lossy(&bytes));
            }
            match parse_result(&buf, &nonce) {
                // The device's own "command didn't survive" sentinel (sha of the payload
                // mismatched on-device) — a verified reply, but it means: resend.
                Parsed::Ok { code, stdout } if code == CMD_CORRUPT_RC && stdout == CMD_CORRUPT_MSG => {
                    last = "command corrupted in transit (device sha mismatch)".into();
                    break;
                }
                Parsed::Ok { code, stdout } => return Ok(RunResult { code, stdout }),
                Parsed::Corrupt => {
                    last = "reply corrupted in transit".into();
                    break; // retry with a new nonce
                }
                Parsed::Pending => {}
            }
            if Instant::now() >= deadline {
                last = "no verified end-nonce within timeout".into();
                break;
            }
            std::thread::sleep(opts.poll);
        }
    }
    Err(RunError::Failed(format!(
        "uart run failed after {} attempts: {last}",
        opts.retries + 1
    )))
}

#[derive(Debug)]
pub enum LoginError {
    Io(io::Error),
    NoLoginPrompt,
    NotConfirmed,
}
impl std::fmt::Display for LoginError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoginError::Io(e) => write!(f, "io: {e}"),
            LoginError::NoLoginPrompt => write!(f, "no login: prompt detected"),
            LoginError::NotConfirmed => write!(f, "login not confirmed (wrong creds or no shell)"),
        }
    }
}
impl std::error::Error for LoginError {}
impl From<io::Error> for LoginError {
    fn from(e: io::Error) -> Self {
        LoginError::Io(e)
    }
}

/// Read from the console until `needle` (a substring) appears or `timeout`.
fn wait_for<C: Console>(c: &mut C, needle: &str, timeout: Duration) -> io::Result<bool> {
    let deadline = Instant::now() + timeout;
    let mut buf = String::new();
    loop {
        buf.push_str(&String::from_utf8_lossy(&c.read()?));
        if buf.contains(needle) {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        std::thread::sleep(Duration::from_millis(30));
    }
}

/// Accumulate whatever arrives over a fixed window (for a quick "what's on the line?" peek).
fn read_for<C: Console>(c: &mut C, window: Duration) -> io::Result<String> {
    let deadline = Instant::now() + window;
    let mut buf = String::new();
    loop {
        buf.push_str(&String::from_utf8_lossy(&c.read()?));
        if Instant::now() >= deadline {
            return Ok(buf);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Log in over a getty (dumb, cooked-echo line). Idempotent: if a working shell is already
/// present it returns Ok without doing anything. Echo-verifies the username (sound at a dumb
/// line), sends the password blind (echo is off), and confirms by a tier-1 `run` afterwards.
pub fn login<C: Console>(
    c: &mut C,
    user: &str,
    pass: &str,
    opts: &RunOpts,
) -> Result<(), LoginError> {
    let quick = RunOpts {
        timeout: Duration::from_secs(3),
        retries: 0,
        ..opts.clone()
    };

    // Peek at what's already on the line FIRST (a getty prints its prompt unprompted) — don't
    // inject anything into a getty before we know whether there's a shell or a login prompt.
    let peek = read_for(c, Duration::from_millis(800))?;
    if !peek.contains("login:") {
        // no login prompt showing — maybe already a shell
        if let Ok(r) = run(c, "true", &quick) {
            if r.code == 0 {
                return Ok(());
            }
        }
        // nudge a quiet getty and wait for the prompt
        c.send_line("")?;
        if !wait_for(c, "login:", opts.timeout)? {
            return Err(LoginError::NoLoginPrompt);
        }
    }

    // echo-verify the username at the dumb line, then Enter
    c.send(user, false)?;
    let _ = wait_for(c, user, Duration::from_secs(2))?; // best-effort echo confirm
    c.send("", true)?;

    // blind password
    if !wait_for(c, "assword", opts.timeout)? {
        return Err(LoginError::NoLoginPrompt);
    }
    c.send_line(pass)?;

    // confirm via a verified command
    match run(c, "true", opts) {
        Ok(r) if r.code == 0 => Ok(()),
        _ => Err(LoginError::NotConfirmed),
    }
}

/// A [`Console`] over the uartd control socket: each send is a paced `uart send`, each read
/// drains `uart read`. This is how `uart run`/`uart login` reach the device through the daemon.
pub struct SocketConsole {
    socket: std::path::PathBuf,
}

impl SocketConsole {
    pub fn new(socket: impl Into<std::path::PathBuf>) -> Self {
        SocketConsole {
            socket: socket.into(),
        }
    }
}

impl Console for SocketConsole {
    fn send(&mut self, text: &str, newline: bool) -> io::Result<()> {
        let req = crate::proto::Request::Send {
            text: text.to_string(),
            no_newline: !newline,
            expect: None,
            timeout_ms: None,
        };
        match crate::client::send_request(&self.socket, &req)? {
            crate::proto::Response::Ok => Ok(()),
            crate::proto::Response::Error { message } => Err(io::Error::other(message)),
            _ => Ok(()),
        }
    }
    fn read(&mut self) -> io::Result<Vec<u8>> {
        match crate::client::send_request(&self.socket, &crate::proto::Request::Read)? {
            crate::proto::Response::Read { text, .. } => Ok(text.into_bytes()),
            _ => Ok(Vec::new()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A faithful in-memory "device": parses the self-verifying one-liner the host sends,
    /// extracts the base64 command + nonce, and emits a correct end-line — optionally corrupting
    /// the first N replies (to exercise retry). It "runs" the command by echoing it back.
    struct SimConsole {
        out: Vec<u8>,
        corrupt_first: u32,
        seen: u32,
    }
    impl SimConsole {
        fn new(corrupt_first: u32) -> Self {
            SimConsole {
                out: Vec::new(),
                corrupt_first,
                seen: 0,
            }
        }
    }
    impl Console for SimConsole {
        fn send(&mut self, text: &str, _newline: bool) -> io::Result<()> {
            // ignore resync / bare lines
            let Some(npos) = text.find("<<E:") else {
                return Ok(());
            };
            let nonce = text[npos + 4..]
                .split(">>")
                .next()
                .unwrap_or("")
                .to_string();
            let d = text
                .split("D=")
                .nth(1)
                .and_then(|s| s.split(';').next())
                .unwrap_or("");
            let cmd = B64.decode(d.as_bytes()).unwrap_or_default();
            self.seen += 1;
            if self.seen <= self.corrupt_first {
                // device-detected corruption sentinel
                let b = B64.encode(CMD_CORRUPT_MSG);
                let sha = sha256_hex(b.as_bytes());
                self.out.extend(
                    format!("<<E:{nonce}>>:{CMD_CORRUPT_RC}:{sha}:{b}\n").as_bytes(),
                );
            } else {
                // success: stdout = the command bytes back
                let b = B64.encode(&cmd);
                let sha = sha256_hex(b.as_bytes());
                self.out
                    .extend(format!("<<E:{nonce}>>:0:{sha}:{b}\n").as_bytes());
            }
            Ok(())
        }
        fn read(&mut self) -> io::Result<Vec<u8>> {
            Ok(std::mem::take(&mut self.out))
        }
    }

    fn fast() -> RunOpts {
        RunOpts {
            timeout: Duration::from_millis(500),
            retries: 5,
            poll: Duration::from_millis(1),
        }
    }

    #[test]
    fn run_happy_path_against_sim() {
        let mut c = SimConsole::new(0);
        let r = run(&mut c, "echo hello", &fast()).unwrap();
        assert_eq!(r.code, 0);
        assert_eq!(r.stdout, b"echo hello");
    }

    #[test]
    fn run_retries_through_corruption() {
        let mut c = SimConsole::new(3); // first 3 attempts corrupt, 4th succeeds
        let r = run(&mut c, "probe", &fast()).unwrap();
        assert_eq!(r.code, 0);
        assert_eq!(r.stdout, b"probe");
    }

    #[test]
    fn run_fails_loudly_when_always_corrupt() {
        let mut c = SimConsole::new(999);
        let opts = RunOpts {
            retries: 2,
            ..fast()
        };
        assert!(run(&mut c, "x", &opts).is_err());
    }


    #[test]
    fn nonces_are_unique_and_hex16() {
        let a = gen_nonce();
        let b = gen_nonce();
        assert_ne!(a, b);
        assert_eq!(a.len(), 16);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn command_embeds_base64_and_sha_and_nonce() {
        let line = build_command("echo hi", "abcd1234abcd1234");
        let d = B64.encode(b"echo hi");
        assert!(line.contains(&format!("D={d}")));
        assert!(line.contains(&format!("H={}", sha256_hex(d.as_bytes()))));
        assert!(line.contains("<<S:abcd1234abcd1234>>"));
        assert!(line.contains("<<E:abcd1234abcd1234>>"));
        assert!(line.contains("base64 -d|sh"));
        assert!(!line.contains("base64 -w0")); // busybox-safe
    }

    // Build the exact end-line a correct device would emit, and confirm we parse + verify it.
    fn device_end_line(nonce: &str, code: i32, stdout: &[u8]) -> String {
        let b = B64.encode(stdout);
        let sha = sha256_hex(b.as_bytes());
        format!("<<E:{nonce}>>:{code}:{sha}:{b}\n")
    }

    #[test]
    fn parses_verified_result() {
        let n = "deadbeefdeadbeef";
        let buf = format!("noise\n<<S:{n}>>\nhello\n{}", device_end_line(n, 0, b"hello\n"));
        match parse_result(&buf, n) {
            Parsed::Ok { code, stdout } => {
                assert_eq!(code, 0);
                assert_eq!(stdout, b"hello\n");
            }
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[test]
    fn pending_until_newline() {
        let n = "aaaabbbbccccdddd";
        let b = B64.encode(b"x");
        let sha = sha256_hex(b.as_bytes());
        let partial = format!("<<E:{n}>>:0:{sha}:{b}"); // no newline yet
        assert_eq!(parse_result(&partial, n), Parsed::Pending);
    }

    #[test]
    fn detects_corrupt_reply() {
        let n = "1111222233334444";
        // valid structure but wrong sha
        let b = B64.encode(b"data");
        let line = format!("<<E:{n}>>:0:0000000000000000000000000000000000000000000000000000000000000000:{b}\n");
        assert_eq!(parse_result(&line, n), Parsed::Corrupt);
    }

    #[test]
    fn nonzero_exit_code_parsed() {
        let n = "0f0f0f0f0f0f0f0f";
        let buf = device_end_line(n, 42, b"");
        match parse_result(&buf, n) {
            Parsed::Ok { code, stdout } => {
                assert_eq!(code, 42);
                assert!(stdout.is_empty());
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn ignores_other_nonces() {
        let mine = "1234123412341234";
        let other = "9999999999999999";
        let buf = device_end_line(other, 0, b"not mine");
        assert_eq!(parse_result(&buf, mine), Parsed::Pending);
    }
}
