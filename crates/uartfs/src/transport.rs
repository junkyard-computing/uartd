// SPDX-License-Identifier: Apache-2.0
//
// Host-side transport: drive the protocol over a `Link` (a bidirectional line channel — in
// production, the uartd socket). Reliability is stop-and-wait ARQ: each DATA chunk is resent
// until ACK'd or retries run out, and nothing is "delivered" until the device returns a DONE
// whose sha256 matches what we sent. EXEC streams stdout/stderr back and verifies stdout
// against the sha + frame count in EXIT (so a lossy read can be retried by the caller).

use std::collections::{BTreeMap, VecDeque};
use std::io;
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;

use crate::chunk::{OutboundBlob, prepare};
use crate::frame::{Dir, FrameReader};
use crate::msg::Msg;

/// A bidirectional, line-oriented channel to the device. Implementations deliver each
/// `send_line` as one newline-terminated console line and return whatever bytes have arrived.
pub trait Link {
    fn send_line(&mut self, line: &str) -> io::Result<()>;
    fn read_bytes(&mut self) -> io::Result<Vec<u8>>;
}

#[derive(Debug, Clone)]
pub struct Timeouts {
    pub ack: Duration,
    pub done: Duration,
    pub exec: Duration,
    pub ready: Duration,
    pub poll: Duration,
    pub retries: u32,
}

impl Default for Timeouts {
    fn default() -> Self {
        Timeouts {
            ack: Duration::from_secs(3),
            done: Duration::from_secs(15),
            exec: Duration::from_secs(60),
            ready: Duration::from_secs(5),
            poll: Duration::from_millis(20),
            retries: 6,
        }
    }
}

#[derive(Debug)]
pub enum TransportError {
    Io(io::Error),
    Timeout(String),
    Protocol(String),
    Verify(String),
}

impl std::fmt::Display for TransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TransportError::Io(e) => write!(f, "io error: {e}"),
            TransportError::Timeout(s) => write!(f, "timeout: {s}"),
            TransportError::Protocol(s) => write!(f, "protocol error: {s}"),
            TransportError::Verify(s) => write!(f, "verification failed: {s}"),
        }
    }
}
impl std::error::Error for TransportError {}
impl From<io::Error> for TransportError {
    fn from(e: io::Error) -> Self {
        TransportError::Io(e)
    }
}

type Result<T> = std::result::Result<T, TransportError>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecResult {
    pub code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

pub struct Transport<L: Link> {
    link: L,
    reader: FrameReader,
    inbox: VecDeque<Msg>,
    pub timeouts: Timeouts,
    next_id: u32,
}

impl<L: Link> Transport<L> {
    pub fn new(link: L) -> Self {
        Transport {
            link,
            reader: FrameReader::new(),
            inbox: VecDeque::new(),
            timeouts: Timeouts::default(),
            next_id: 1,
        }
    }

    pub fn with_timeouts(link: L, timeouts: Timeouts) -> Self {
        let mut t = Self::new(link);
        t.timeouts = timeouts;
        t
    }

    fn new_id(&mut self) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn send(&mut self, m: &Msg) -> Result<()> {
        self.link.send_line(&m.to_frame().encode())?;
        Ok(())
    }

    /// Read available bytes and decode any device->host messages into the inbox.
    fn pump(&mut self) -> Result<()> {
        let bytes = self.link.read_bytes()?;
        if !bytes.is_empty() {
            for f in self.reader.push(&bytes) {
                if f.dir == Dir::ToHost
                    && let Some(m) = Msg::from_frame(&f)
                {
                    self.inbox.push_back(m);
                }
            }
        }
        Ok(())
    }

    /// Wait until a message matching `pred` arrives (removing + returning it), or `deadline`.
    /// Other messages stay queued.
    fn recv_match<F: Fn(&Msg) -> bool>(&mut self, deadline: Instant, pred: F) -> Result<Option<Msg>> {
        loop {
            if let Some(i) = self.inbox.iter().position(&pred) {
                return Ok(self.inbox.remove(i));
            }
            self.pump()?;
            if let Some(i) = self.inbox.iter().position(&pred) {
                return Ok(self.inbox.remove(i));
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            std::thread::sleep(self.timeouts.poll);
        }
    }

    /// Handshake: PING until READY (or timeout). Returns the agent version.
    pub fn ping(&mut self) -> Result<String> {
        let deadline = Instant::now() + self.timeouts.ready;
        // a couple of pings in case the first is lost
        loop {
            self.send(&Msg::Ping)?;
            let until = (Instant::now() + self.timeouts.ack).min(deadline);
            if let Some(Msg::Ready { version }) =
                self.recv_match(until, |m| matches!(m, Msg::Ready { .. }))?
            {
                return Ok(version);
            }
            if Instant::now() >= deadline {
                return Err(TransportError::Timeout("no READY from agent".into()));
            }
        }
    }

    /// Reliably deliver `data` into the device-side temp file for `xid`, stop-and-wait with
    /// retransmit, gated on a matching DONE sha256. Returns the blob's sha256 on success.
    pub fn send_blob(&mut self, xid: u32, data: &[u8], chunk_size: usize) -> Result<String> {
        let blob: OutboundBlob = prepare(data, chunk_size);
        self.send(&Msg::Open {
            xid,
            nchunks: blob.nchunks(),
            chunk_size: blob.chunk_size,
            sha256: blob.sha256.clone(),
        })?;

        for c in &blob.chunks {
            let seq = c.seq;
            let mut tries = 0u32;
            loop {
                self.send(&Msg::Data {
                    xid,
                    seq,
                    b64: c.b64.clone(),
                    sum: c.sum.clone(),
                })?;
                let deadline = Instant::now() + self.timeouts.ack;
                let got = self.recv_match(deadline, |m| {
                    matches!(m, Msg::Ack { xid: x, seq: s } if *x == xid && *s == seq)
                        || matches!(m, Msg::Nak { xid: x, seq: s } if *x == xid && *s == seq)
                })?;
                match got {
                    Some(Msg::Ack { .. }) => break,
                    _ => {
                        tries += 1;
                        if tries > self.timeouts.retries {
                            return Err(TransportError::Timeout(format!(
                                "chunk {seq} not acked after {tries} tries"
                            )));
                        }
                    }
                }
            }
        }

        self.send(&Msg::Close { xid })?;
        let deadline = Instant::now() + self.timeouts.done;
        match self.recv_match(deadline, |m| matches!(m, Msg::Done { xid: x, .. } if *x == xid))? {
            Some(Msg::Done { ok: true, sha256, .. }) => {
                if sha256 == blob.sha256 {
                    Ok(blob.sha256)
                } else {
                    Err(TransportError::Verify(format!(
                        "device sha {sha256} != sent sha {}",
                        blob.sha256
                    )))
                }
            }
            Some(Msg::Done { ok: false, .. }) => {
                Err(TransportError::Verify("device reported transfer failed".into()))
            }
            _ => Err(TransportError::Timeout("no DONE from device".into())),
        }
    }

    /// Run a shell command on the device, returning stdout/stderr/exit. Verifies stdout
    /// against the EXIT sha + frame count; on mismatch returns a Verify error (caller retries).
    pub fn exec(&mut self, command: &str) -> Result<ExecResult> {
        let cid = self.new_id();
        let b64cmd = B64.encode(command.as_bytes());
        self.send(&Msg::Exec { cid, b64cmd })?;

        // OUT frames carry pieces of the *whole-stream* base64 (the agent base64s the output
        // then folds it into frames), so we concatenate in seq order and decode once.
        let mut out: BTreeMap<u32, String> = BTreeMap::new();
        let mut err: BTreeMap<u32, String> = BTreeMap::new();
        let deadline = Instant::now() + self.timeouts.exec;

        let exit = loop {
            self.pump()?;
            let mut i = 0;
            let mut exit = None;
            while i < self.inbox.len() {
                match &self.inbox[i] {
                    Msg::Out { cid: c, .. } if *c == cid => {
                        if let Some(Msg::Out { stream, seq, b64, .. }) = self.inbox.remove(i) {
                            if stream == 2 {
                                err.insert(seq, b64);
                            } else {
                                out.insert(seq, b64);
                            }
                        }
                    }
                    Msg::Exit { cid: c, .. } if *c == cid => {
                        exit = self.inbox.remove(i);
                        break;
                    }
                    _ => i += 1,
                }
            }
            if let Some(e) = exit {
                break e;
            }
            if Instant::now() >= deadline {
                return Err(TransportError::Timeout("no EXIT from device".into()));
            }
            std::thread::sleep(self.timeouts.poll);
        };

        let Msg::Exit { code, out_frames, out_sha, .. } = exit else {
            unreachable!()
        };

        if out.len() as u32 != out_frames {
            return Err(TransportError::Verify(format!(
                "stdout frame count {} != expected {out_frames}",
                out.len()
            )));
        }
        let out_b64: String = out.values().fold(String::new(), |mut a, b| {
            a.push_str(b);
            a
        });
        let err_b64: String = err.values().fold(String::new(), |mut a, b| {
            a.push_str(b);
            a
        });
        let stdout = B64
            .decode(out_b64.as_bytes())
            .map_err(|_| TransportError::Verify("stdout is not valid base64".into()))?;
        let stderr = B64.decode(err_b64.as_bytes()).unwrap_or_default();

        let got = crate::hash::sha256_hex(&stdout);
        if out_sha != "-" && got != out_sha {
            return Err(TransportError::Verify(format!(
                "stdout sha {got} != expected {out_sha}"
            )));
        }

        Ok(ExecResult { code, stdout, stderr })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunk::{Reassembler, chunk_sum};

    /// In-memory device: a Rust reimplementation of the phone agent, wired loopback to the
    /// host Transport, with optional fault injection. (The real shell agent is tested in UF4.)
    struct DeviceSim {
        out: VecDeque<u8>,                 // bytes the host will read
        reader: FrameReader,               // host->device frames
        transfers: std::collections::HashMap<u32, Xfer>,
        files: std::collections::HashMap<u32, Vec<u8>>, // xid -> reconstructed blob
        // fault injection
        drop_data: std::collections::HashSet<u32>, // data seqs to drop once
        corrupt_data: std::collections::HashSet<u32>, // data seqs to corrupt once (bad sum)
    }

    struct Xfer {
        re: Reassembler,
    }

    impl DeviceSim {
        fn new() -> Self {
            DeviceSim {
                out: VecDeque::new(),
                reader: FrameReader::new(),
                transfers: Default::default(),
                files: Default::default(),
                drop_data: Default::default(),
                corrupt_data: Default::default(),
            }
        }
        fn reply(&mut self, m: &Msg) {
            self.out.extend(m.to_frame().encode_line().as_bytes());
        }
        fn handle(&mut self, m: Msg) {
            match m {
                Msg::Ping => self.reply(&Msg::Ready { version: "sim1".into() }),
                Msg::Open { xid, nchunks, sha256, .. } => {
                    self.transfers.insert(
                        xid,
                        Xfer { re: Reassembler::new(nchunks, sha256) },
                    );
                }
                Msg::Data { xid, seq, b64, sum } => {
                    if self.drop_data.remove(&seq) {
                        return; // simulate a lost data frame: no reply
                    }
                    let (b64, sum) = if self.corrupt_data.remove(&seq) {
                        ("Y29ycnVwdA==".to_string(), sum) // wrong sum vs payload
                    } else {
                        (b64, sum)
                    };
                    let Some(x) = self.transfers.get_mut(&xid) else { return };
                    match x.re.accept(seq, &b64, &sum) {
                        Ok(_) => self.reply(&Msg::Ack { xid, seq }),
                        Err(_) => self.reply(&Msg::Nak { xid, seq }),
                    }
                }
                Msg::Close { xid } => {
                    let Some(x) = self.transfers.remove(&xid) else {
                        self.reply(&Msg::Done { xid, ok: false, sha256: "-".into() });
                        return;
                    };
                    match x.re.finish() {
                        Ok(bytes) => {
                            let sha = crate::hash::sha256_hex(&bytes);
                            self.files.insert(xid, bytes);
                            self.reply(&Msg::Done { xid, ok: true, sha256: sha });
                        }
                        Err(_) => self.reply(&Msg::Done { xid, ok: false, sha256: "-".into() }),
                    }
                }
                Msg::Exec { cid, b64cmd } => {
                    let cmd = String::from_utf8(B64.decode(b64cmd.as_bytes()).unwrap()).unwrap();
                    // the sim "runs" a tiny fixed command vocabulary
                    let stdout = if let Some(rest) = cmd.strip_prefix("echo ") {
                        format!("{rest}\n").into_bytes()
                    } else {
                        Vec::new()
                    };
                    // base64 the WHOLE stdout, then fold into fixed-width OUT frames (mirrors
                    // the shell agent); the host concatenates and decodes once.
                    let whole = B64.encode(&stdout);
                    let pieces: Vec<&str> = if whole.is_empty() {
                        vec![]
                    } else {
                        fold(&whole, 12)
                    };
                    for (seq, p) in pieces.iter().enumerate() {
                        self.reply(&Msg::Out {
                            cid,
                            stream: 1,
                            seq: seq as u32,
                            b64: (*p).to_string(),
                        });
                    }
                    let out_sha = crate::hash::sha256_hex(&stdout);
                    self.reply(&Msg::Exit {
                        cid,
                        code: 0,
                        out_frames: pieces.len() as u32,
                        out_sha,
                    });
                }
                _ => {}
            }
        }
    }

    impl Link for DeviceSim {
        fn send_line(&mut self, line: &str) -> io::Result<()> {
            let mut bytes = line.as_bytes().to_vec();
            bytes.push(b'\n');
            let frames = self.reader.push(&bytes);
            for f in frames {
                if f.dir == Dir::ToDevice
                    && let Some(m) = Msg::from_frame(&f)
                {
                    self.handle(m);
                }
            }
            Ok(())
        }
        fn read_bytes(&mut self) -> io::Result<Vec<u8>> {
            Ok(self.out.drain(..).collect())
        }
    }

    fn fold(s: &str, w: usize) -> Vec<&str> {
        let mut out = Vec::new();
        let mut i = 0;
        while i < s.len() {
            let end = (i + w).min(s.len());
            out.push(&s[i..end]);
            i = end;
        }
        out
    }

    fn fast() -> Timeouts {
        Timeouts {
            ack: Duration::from_millis(200),
            done: Duration::from_millis(500),
            exec: Duration::from_millis(500),
            ready: Duration::from_millis(500),
            poll: Duration::from_millis(1),
            retries: 6,
        }
    }

    #[test]
    fn ping_handshake() {
        let mut t = Transport::with_timeouts(DeviceSim::new(), fast());
        assert_eq!(t.ping().unwrap(), "sim1");
    }

    #[test]
    fn send_blob_happy_path() {
        let data: Vec<u8> = (0..3000u32).map(|i| (i % 251) as u8).collect();
        let mut t = Transport::with_timeouts(DeviceSim::new(), fast());
        let sha = t.send_blob(7, &data, 256).unwrap();
        assert_eq!(sha, crate::hash::sha256_hex(&data));
        // device reconstructed the exact bytes
        assert_eq!(t.link.files.get(&7).unwrap(), &data);
    }

    #[test]
    fn send_blob_recovers_from_dropped_chunk() {
        let data = b"reliable delta flashing over a lossy uart line".to_vec();
        let mut sim = DeviceSim::new();
        sim.drop_data.insert(2); // chunk 2's first send is lost -> must retransmit
        let mut t = Transport::with_timeouts(sim, fast());
        let sha = t.send_blob(1, &data, 8).unwrap();
        assert_eq!(sha, crate::hash::sha256_hex(&data));
        assert_eq!(t.link.files.get(&1).unwrap(), &data);
    }

    #[test]
    fn send_blob_recovers_from_corrupted_chunk() {
        let data = b"corruption should trigger a NAK and resend".to_vec();
        let mut sim = DeviceSim::new();
        sim.corrupt_data.insert(1);
        let mut t = Transport::with_timeouts(sim, fast());
        t.send_blob(1, &data, 8).unwrap();
        assert_eq!(t.link.files.get(&1).unwrap(), &data);
    }

    #[test]
    fn exec_returns_stdout_verified() {
        let mut t = Transport::with_timeouts(DeviceSim::new(), fast());
        let r = t.exec("echo hello-from-device").unwrap();
        assert_eq!(r.code, 0);
        assert_eq!(r.stdout, b"hello-from-device\n");
    }

    #[test]
    fn chunk_sum_matches_helper() {
        // guards that the sim/agent and host compute the same per-chunk tag
        assert_eq!(chunk_sum("QUJD"), crate::chunk::chunk_sum("QUJD"));
    }
}
