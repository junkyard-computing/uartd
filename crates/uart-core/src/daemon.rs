// SPDX-License-Identifier: Apache-2.0
//
// The daemon: owns the serial port, captures continuously into the drain buffer + forensic
// log, and serves the CLI over a Unix socket. Concurrency is deliberately boring — std
// threads + channels, no async runtime (one port + one socket doesn't need one):
//
//   * reader thread  — owns the read handle; ingests bytes; on port loss enters a reconnect
//     loop (backoff reopen, logs a marker, resumes) and never panics.
//   * acceptor thread — accepts CLI connections; spawns a short-lived handler thread each.
//   * waiters         — `wait`/`send --expect` subscribe to a broadcast of incoming chunks so
//     a blocking call on one connection never blocks `status`/`read` on another.
//   * writer          — a cloned port handle behind a mutex; sends are paced (flow-control-safe)
//     and the lock is released between writes so reads keep flowing.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serialport::{DataBits, FlowControl, Parity as SParity, SerialPort, StopBits};

use crate::buffer::{DrainBuffer, drop_marker};
use crate::clock::Clock;
use crate::config::{Config, Parity};
use crate::expect::ExpectMatcher;
use crate::lines::{LineFramer, format_log_line};
use crate::login::AutoLogin;
use crate::pacer::{self, PacerConfig};
use crate::proto::{LineJson, Request, Response};

const READ_BUF: usize = 8192;

/// Shared daemon state, reachable from every thread.
pub struct Shared {
    cfg: Config,
    clock: Arc<dyn Clock>,
    started: Instant,
    log_path: PathBuf,

    buf: Mutex<DrainBuffer>,
    log: Mutex<File>,
    framer: Mutex<LineFramer>,
    /// Cloned write handle to the port; `None` while disconnected.
    writer: Mutex<Option<Box<dyn SerialPort>>>,
    /// Live subscribers to the incoming byte stream (for wait/expect).
    subscribers: Mutex<Vec<Sender<Arc<Vec<u8>>>>>,
    login: Mutex<Option<AutoLogin>>,

    connected: AtomicBool,
    buffered: AtomicUsize,
    shutdown: Arc<AtomicBool>,
}

impl Shared {
    fn now(&self) -> (u64, u64) {
        self.clock.now()
    }

    /// Append a daemon-internal note to the forensic log (markers for connect/disconnect).
    fn log_note(&self, note: &str) {
        let (mono, wall) = self.now();
        if let Ok(mut f) = self.log.lock() {
            let _ = writeln!(f, "[w={wall} m={mono}] ==== uartd: {note} ====");
            let _ = f.flush();
        }
    }

    /// Ingest freshly captured bytes: buffer, log whole lines, broadcast, drive auto-login.
    fn ingest(&self, data: &[u8]) {
        let (mono, wall) = self.now();

        {
            let mut b = self.buf.lock().unwrap();
            b.push(mono, wall, data);
            self.buffered.store(b.len(), Ordering::Relaxed);
        }

        // Forensic log: whole lines only (partials wait for their newline).
        if let (Ok(mut fr), Ok(mut f)) = (self.framer.lock(), self.log.lock()) {
            for line in fr.push(mono, wall, data) {
                let _ = writeln!(f, "{}", format_log_line(&line));
            }
            let _ = f.flush();
        }

        // Broadcast to waiters; drop senders whose receiver is gone.
        let arc = Arc::new(data.to_vec());
        if let Ok(mut subs) = self.subscribers.lock() {
            subs.retain(|s| s.send(arc.clone()).is_ok());
        }

        // Auto-login (opt-in): may write credentials back to the port.
        self.drive_auto_login(data);
    }

    fn drive_auto_login(&self, data: &[u8]) {
        let mut guard = self.login.lock().unwrap();
        let Some(al) = guard.as_mut() else { return };
        for out in al.feed(data) {
            // Send credentials line-paced, ignoring transient write failures.
            let _ = self.send_paced(&out, false);
        }
    }

    /// Write `text` to the port, paced to be flow-control-safe. `no_newline` suppresses the
    /// trailing newline. Returns an error string if not connected or the write fails.
    fn send_paced(&self, text: &str, no_newline: bool) -> Result<(), String> {
        let steps = pacer::plan(
            text.as_bytes(),
            &PacerConfig {
                newline_append: !no_newline,
                inter_line: self.cfg.inter_line,
                inter_char: self.cfg.inter_char,
            },
        );
        for step in steps {
            {
                let mut w = self.writer.lock().unwrap();
                let port = w
                    .as_mut()
                    .ok_or_else(|| "not connected to the port".to_string())?;
                port.write_all(&step.bytes).map_err(|e| e.to_string())?;
                port.flush().map_err(|e| e.to_string())?;
            } // release the lock before sleeping so reads keep flowing
            if !step.delay_after.is_zero() {
                thread::sleep(step.delay_after);
            }
        }
        Ok(())
    }
}

/// A running daemon. Drop or call `shutdown` to stop.
pub struct Daemon {
    shared: Arc<Shared>,
    threads: Vec<JoinHandle<()>>,
}

impl Daemon {
    /// Bind the socket, open the log, and spawn the reader + acceptor threads. Returns once
    /// the socket is ready to accept connections.
    pub fn start(cfg: Config, clock: Arc<dyn Clock>) -> std::io::Result<Daemon> {
        fs::create_dir_all(&cfg.log_dir)?;
        let log_path = cfg.log_dir.join("uartd.log");
        let mut logf = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)?;
        let (mono, wall) = clock.now();
        writeln!(
            logf,
            "[w={wall} m={mono}] ==== uartd session start: port={} baud={} ====",
            cfg.port, cfg.baud
        )?;
        logf.flush()?;

        let login = cfg
            .login_user
            .clone()
            .zip(cfg.login_pass.clone())
            .map(|(u, p)| AutoLogin::new(u, p));

        // Bind the control socket (clear any stale one first).
        let _ = fs::remove_file(&cfg.socket_path);
        let listener = UnixListener::bind(&cfg.socket_path)?;
        listener.set_nonblocking(true)?;

        let shared = Arc::new(Shared {
            log_path,
            clock,
            started: Instant::now(),
            buf: Mutex::new(DrainBuffer::new(cfg.buffer_cap)),
            log: Mutex::new(logf),
            framer: Mutex::new(LineFramer::new()),
            writer: Mutex::new(None),
            subscribers: Mutex::new(Vec::new()),
            login: Mutex::new(login),
            connected: AtomicBool::new(false),
            buffered: AtomicUsize::new(0),
            shutdown: Arc::new(AtomicBool::new(false)),
            cfg,
        });

        let mut threads = Vec::new();
        {
            let s = shared.clone();
            threads.push(thread::spawn(move || reader_loop(s)));
        }
        {
            let s = shared.clone();
            threads.push(thread::spawn(move || acceptor_loop(s, listener)));
        }

        Ok(Daemon { shared, threads })
    }

    pub fn log_path(&self) -> &Path {
        &self.shared.log_path
    }

    pub fn socket_path(&self) -> &Path {
        &self.shared.cfg.socket_path
    }

    pub fn is_shutdown(&self) -> bool {
        self.shared.shutdown.load(Ordering::SeqCst)
    }

    /// The flag the daemon watches; wire signal handlers to it.
    pub fn shutdown_flag(&self) -> Arc<AtomicBool> {
        self.shared.shutdown.clone()
    }

    /// Block until something requests shutdown (a `stop` request or the flag being set).
    pub fn wait_for_shutdown(&self) {
        while !self.is_shutdown() {
            thread::sleep(Duration::from_millis(50));
        }
    }

    /// Signal shutdown, join threads, and remove the socket.
    pub fn shutdown(self) {
        self.shared.shutdown.store(true, Ordering::SeqCst);
        for t in self.threads {
            let _ = t.join();
        }
        let _ = fs::remove_file(&self.shared.cfg.socket_path);
        if let Ok(mut f) = self.shared.log.lock() {
            let _ = f.flush();
        }
    }
}

fn map_data_bits(n: u8) -> DataBits {
    match n {
        5 => DataBits::Five,
        6 => DataBits::Six,
        7 => DataBits::Seven,
        _ => DataBits::Eight,
    }
}
fn map_parity(p: Parity) -> SParity {
    match p {
        Parity::N => SParity::None,
        Parity::E => SParity::Even,
        Parity::O => SParity::Odd,
    }
}
fn map_stop_bits(n: u8) -> StopBits {
    match n {
        2 => StopBits::Two,
        _ => StopBits::One,
    }
}

fn open_port(cfg: &Config) -> serialport::Result<Box<dyn SerialPort>> {
    serialport::new(&cfg.port, cfg.baud)
        .data_bits(map_data_bits(cfg.data_bits))
        .parity(map_parity(cfg.parity))
        .stop_bits(map_stop_bits(cfg.stop_bits))
        .flow_control(FlowControl::None)
        .timeout(Duration::from_millis(100))
        .open()
}

/// Owns the port: connect, capture until the port drops, then reconnect — forever.
fn reader_loop(shared: Arc<Shared>) {
    let mut first = true;
    while !shared.shutdown.load(Ordering::SeqCst) {
        match open_port(&shared.cfg) {
            Ok(port) => {
                // Set up the write handle (a clone of the same fd).
                match port.try_clone() {
                    Ok(wr) => *shared.writer.lock().unwrap() = Some(wr),
                    Err(_) => {
                        // Without a write handle we can still capture; log and continue.
                        *shared.writer.lock().unwrap() = None;
                    }
                }
                // Re-arm auto-login: a freshly (re)connected device will show login: again.
                if let Some(al) = shared.login.lock().unwrap().as_mut() {
                    al.rearm();
                }
                shared.connected.store(true, Ordering::SeqCst);
                shared.log_note(if first { "connected" } else { "reconnected" });
                first = false;

                capture(&shared, port);

                // Fell out of capture => port lost.
                shared.connected.store(false, Ordering::SeqCst);
                *shared.writer.lock().unwrap() = None;
                if !shared.shutdown.load(Ordering::SeqCst) {
                    shared.log_note("disconnected");
                }
            }
            Err(_) => {
                // Port not present yet (or vanished). Keep retrying.
            }
        }
        // Back off before retrying (also throttles the connect spin while absent).
        sleep_interruptible(&shared, shared.cfg.reconnect_backoff);
    }
}

/// Read from an open port until it errors/EOFs or shutdown is requested.
fn capture(shared: &Arc<Shared>, mut port: Box<dyn SerialPort>) {
    let mut buf = [0u8; READ_BUF];
    loop {
        if shared.shutdown.load(Ordering::SeqCst) {
            return;
        }
        match port.read(&mut buf) {
            Ok(0) => return, // EOF: port gone
            Ok(n) => shared.ingest(&buf[..n]),
            Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut => continue,
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return, // any other error: treat as disconnect
        }
    }
}

fn sleep_interruptible(shared: &Arc<Shared>, dur: Duration) {
    let step = Duration::from_millis(50);
    let mut left = dur;
    while left > Duration::ZERO {
        if shared.shutdown.load(Ordering::SeqCst) {
            return;
        }
        let s = step.min(left);
        thread::sleep(s);
        left = left.saturating_sub(s);
    }
}

fn acceptor_loop(shared: Arc<Shared>, listener: UnixListener) {
    while !shared.shutdown.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _addr)) => {
                let s = shared.clone();
                thread::spawn(move || handle_conn(s, stream));
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(20));
            }
            Err(_) => thread::sleep(Duration::from_millis(20)),
        }
    }
}

fn handle_conn(shared: Arc<Shared>, stream: UnixStream) {
    use std::io::{BufRead, BufReader};
    let read_stream = match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    };
    let mut reader = BufReader::new(read_stream);
    let mut line = String::new();
    if reader.read_line(&mut line).unwrap_or(0) == 0 {
        return;
    }

    let resp = match crate::proto::from_line::<Request>(&line) {
        Ok(req) => dispatch(&shared, req),
        Err(e) => Response::Error {
            message: format!("malformed request: {e}"),
        },
    };

    let mut w = stream;
    let _ = w.write_all(crate::proto::to_line(&resp).as_bytes());
    let _ = w.flush();
}

fn dispatch(shared: &Arc<Shared>, req: Request) -> Response {
    match req {
        Request::Read => {
            let (dropped, chunks) = shared.buf.lock().unwrap().drain_chunks();
            shared.buffered.store(0, Ordering::Relaxed);
            build_read(dropped, &chunks)
        }
        Request::Peek => {
            let (dropped, chunks) = shared.buf.lock().unwrap().peek_chunks();
            build_read(dropped, &chunks)
        }
        Request::Send {
            text,
            no_newline,
            expect,
            timeout_ms,
        } => match expect {
            None => match shared.send_paced(&text, no_newline) {
                Ok(()) => Response::Ok,
                Err(message) => Response::Error { message },
            },
            Some(pattern) => do_expect(
                shared,
                &pattern,
                timeout_ms.unwrap_or(5000),
                Some((text, no_newline)),
            ),
        },
        Request::Wait {
            pattern,
            timeout_ms,
        } => do_expect(shared, &pattern, timeout_ms, None),
        Request::Status => Response::Status {
            running: true,
            port: shared.cfg.port.clone(),
            baud: shared.cfg.baud,
            connected: shared.connected.load(Ordering::SeqCst),
            buffer_bytes: shared.buffered.load(Ordering::Relaxed),
            uptime_s: shared.started.elapsed().as_secs(),
            log_path: shared.log_path.display().to_string(),
        },
        Request::Log => Response::Log {
            path: shared.log_path.display().to_string(),
        },
        Request::Stop => {
            shared.shutdown.store(true, Ordering::SeqCst);
            Response::Ok
        }
    }
}

/// Subscribe to the stream, optionally send, then block until `pattern` matches or timeout.
/// The timeout clock starts after any send completes (so pacing never eats the budget).
fn do_expect(
    shared: &Arc<Shared>,
    pattern: &str,
    timeout_ms: u64,
    send_first: Option<(String, bool)>,
) -> Response {
    let mut matcher = match ExpectMatcher::new(pattern) {
        Ok(m) => m,
        Err(e) => {
            return Response::Error {
                message: format!("bad regex: {e}"),
            };
        }
    };

    // Register interest BEFORE sending, so a fast response is not missed.
    let (tx, rx) = channel::<Arc<Vec<u8>>>();
    shared.subscribers.lock().unwrap().push(tx);

    if let Some((text, no_newline)) = send_first
        && let Err(message) = shared.send_paced(&text, no_newline)
    {
        return Response::Error { message };
    }

    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Response::Match {
                matched: false,
                matched_text: None,
                context: matcher.buffer().to_string(),
                timed_out: true,
            };
        }
        match rx.recv_timeout(remaining) {
            Ok(chunk) => {
                if let Some(m) = matcher.feed(&chunk) {
                    return Response::Match {
                        matched: true,
                        matched_text: Some(m.matched),
                        context: m.context,
                        timed_out: false,
                    };
                }
            }
            Err(_) => {
                return Response::Match {
                    matched: false,
                    matched_text: None,
                    context: matcher.buffer().to_string(),
                    timed_out: true,
                };
            }
        }
    }
    // rx drops here on return; the reader prunes the dead sender on its next broadcast.
}

/// Build a `read`/`peek` response: plain `text` (with any drop marker) plus a framed
/// per-line view for `--json`.
fn build_read(dropped: u64, chunks: &[crate::buffer::Chunk]) -> Response {
    let mut text = String::new();
    let mut lines: Vec<LineJson> = Vec::new();

    if dropped > 0 {
        let marker = drop_marker(dropped);
        text.push_str(&marker);
        lines.push(LineJson {
            mono_ns: 0,
            wall_ms: 0,
            text: marker.trim_end().to_string(),
        });
    }

    let mut fr = LineFramer::new();
    for c in chunks {
        text.push_str(&String::from_utf8_lossy(&c.bytes));
        for l in fr.push(c.mono_ns, c.wall_ms, &c.bytes) {
            lines.push(l.into());
        }
    }
    if let Some(l) = fr.flush() {
        lines.push(l.into());
    }

    Response::Read {
        dropped,
        text,
        lines,
    }
}
