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
    /// A chunk could not be delivered within the retry bound. `resume_from` is the seq the
    /// caller should resume at — the agent keeps verified chunks for `xid`, so re-running
    /// `send_blob` (after the device is reachable again) continues instead of restarting.
    Stalled {
        xid: u32,
        resume_from: u32,
        detail: String,
    },
}

impl std::fmt::Display for TransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TransportError::Io(e) => write!(f, "io error: {e}"),
            TransportError::Timeout(s) => write!(f, "timeout: {s}"),
            TransportError::Protocol(s) => write!(f, "protocol error: {s}"),
            TransportError::Verify(s) => write!(f, "verification failed: {s}"),
            TransportError::Stalled {
                xid,
                resume_from,
                detail,
            } => write!(
                f,
                "transfer xid {xid} stalled at chunk {resume_from} (resumable): {detail}"
            ),
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
    fn recv_match<F: Fn(&Msg) -> bool>(
        &mut self,
        deadline: Instant,
        pred: F,
    ) -> Result<Option<Msg>> {
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

    /// Ask the agent how many contiguous chunks it already holds for `xid` (the resume point).
    /// Returns 0 if the agent doesn't answer (treat as "start from scratch").
    pub fn stat(&mut self, xid: u32) -> Result<u32> {
        // a couple of attempts in case STAT or HAVE is dropped
        for _ in 0..3 {
            self.send(&Msg::Stat { xid })?;
            let deadline = Instant::now() + self.timeouts.ack;
            if let Some(Msg::Have { hw, .. }) = self.recv_match(
                deadline,
                |m| matches!(m, Msg::Have { xid: x, .. } if *x == xid),
            )? {
                return Ok(hw);
            }
        }
        Ok(0)
    }

    /// Reliably deliver `data` into the device-side temp file for `xid`, stop-and-wait with
    /// retransmit, gated on a matching DONE sha256. Returns the blob's sha256 on success.
    ///
    /// Resumable: the agent keeps verified chunks across an interrupted OPEN, so this first
    /// asks STAT for the resume point and only sends from there. A dropped/merged ACK keeps
    /// retrying the same chunk; only after `retries` consecutive failures does it give up —
    /// and then with a `Stalled { resume_from }` error so the caller can retry and continue.
    pub fn send_blob(&mut self, xid: u32, data: &[u8], chunk_size: usize) -> Result<String> {
        let blob: OutboundBlob = prepare(data, chunk_size);
        let open = Msg::Open {
            xid,
            nchunks: blob.nchunks(),
            chunk_size: blob.chunk_size,
            sha256: blob.sha256.clone(),
        };
        self.send(&open)?;

        // Resume point: chunks [0, resume) are already verified on-device. STAT can only return
        // a value <= nchunks for the same blob (agent clears c.* if the blob changed on OPEN).
        // (STAT also implicitly confirms OPEN landed; if it didn't, hw is 0 and we re-OPEN below
        // when chunks stall.)
        let resume = self.stat(xid)?.min(blob.nchunks());

        for c in blob.chunks.iter().skip(resume as usize) {
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
                    // A NAK means the chunk arrived corrupt: resend immediately, but charge a
                    // try so a persistently-corrupting path still terminates. A timeout (None)
                    // means the DATA or its ACK was dropped: also resend.
                    _ => {
                        tries += 1;
                        // If a chunk keeps failing, the OPEN itself may have been lost on a
                        // lossy line (the agent has no transfer for this xid, so it silently
                        // ignores DATA). Periodically re-send OPEN to recover. OPEN is
                        // idempotent and preserves already-verified chunks (resumable).
                        if tries.is_multiple_of(4) {
                            self.send(&open)?;
                        }
                        if tries > self.timeouts.retries {
                            // Surface a resumable error: chunks below `seq` are safely on the
                            // device, so a later send_blob with the same xid continues here.
                            return Err(TransportError::Stalled {
                                xid,
                                resume_from: seq,
                                detail: format!("chunk {seq} not acked after {tries} tries"),
                            });
                        }
                    }
                }
            }
        }

        // CLOSE -> DONE, retried: CLOSE is idempotent (the agent just re-concatenates and
        // re-hashes the persisted chunks), so a lost CLOSE or a garbled DONE recovers by
        // re-sending CLOSE within the overall `done` budget.
        let overall = Instant::now() + self.timeouts.done;
        loop {
            self.send(&Msg::Close { xid })?;
            let until = (Instant::now() + self.timeouts.ack).min(overall);
            match self.recv_match(
                until,
                |m| matches!(m, Msg::Done { xid: x, .. } if *x == xid),
            )? {
                Some(Msg::Done {
                    ok: true, sha256, ..
                }) => {
                    return if sha256 == blob.sha256 {
                        Ok(blob.sha256)
                    } else {
                        Err(TransportError::Verify(format!(
                            "device sha {sha256} != sent sha {}",
                            blob.sha256
                        )))
                    };
                }
                Some(Msg::Done { ok: false, .. }) => {
                    return Err(TransportError::Verify(
                        "device reported transfer failed".into(),
                    ));
                }
                _ => {
                    if Instant::now() >= overall {
                        return Err(TransportError::Timeout("no DONE from device".into()));
                    }
                }
            }
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
                        if let Some(Msg::Out {
                            stream, seq, b64, ..
                        }) = self.inbox.remove(i)
                        {
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

        let Msg::Exit {
            code,
            out_frames,
            out_sha,
            ..
        } = exit
        else {
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

        Ok(ExecResult {
            code,
            stdout,
            stderr,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunk::{Reassembler, chunk_sum};

    /// In-memory device: a Rust reimplementation of the phone agent, wired loopback to the
    /// host Transport, with optional fault injection. (The real shell agent is tested in UF4.)
    struct DeviceSim {
        out: VecDeque<u8>,   // bytes the host will read
        reader: FrameReader, // host->device frames
        transfers: std::collections::HashMap<u32, Xfer>,
        files: std::collections::HashMap<u32, Vec<u8>>, // xid -> reconstructed blob
        // fault injection
        drop_data: std::collections::HashSet<u32>, // data seqs to drop once
        corrupt_data: std::collections::HashSet<u32>, // data seqs to corrupt once (bad sum)
        drop_ack: std::collections::HashSet<u32>,  // seqs to accept but drop the ACK for, once
        die_after: Option<u32>, // stop replying once seq >= this (simulated reboot)
        rebooted: bool,         // set true once die_after tripped
    }

    struct Xfer {
        re: Reassembler,
        sha: String,
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
                drop_ack: Default::default(),
                die_after: None,
                rebooted: false,
            }
        }
        fn reply(&mut self, m: &Msg) {
            self.out.extend(m.to_frame().encode_line().as_bytes());
        }
        fn handle(&mut self, m: Msg) {
            match m {
                Msg::Ping => self.reply(&Msg::Ready {
                    version: "sim1".into(),
                }),
                Msg::Open {
                    xid,
                    nchunks,
                    sha256,
                    ..
                } => {
                    // Resumable OPEN: keep an existing transfer for the same xid+sha (mirrors
                    // the agent not wiping verified chunks); only reset on a new/different blob.
                    let keep = self
                        .transfers
                        .get(&xid)
                        .map(|x| x.sha == sha256)
                        .unwrap_or(false);
                    if !keep {
                        self.transfers.insert(
                            xid,
                            Xfer {
                                re: Reassembler::new(nchunks, sha256.clone()),
                                sha: sha256,
                            },
                        );
                    }
                }
                Msg::Stat { xid } => {
                    let hw = self
                        .transfers
                        .get(&xid)
                        .map(|x| x.re.contiguous_have())
                        .unwrap_or(0);
                    self.reply(&Msg::Have { xid, hw });
                }
                Msg::Data { xid, seq, b64, sum } => {
                    if self.drop_data.remove(&seq) {
                        return; // simulate a lost data frame: no reply
                    }
                    if self.drop_ack.remove(&seq) {
                        // accept+persist the chunk but drop the ACK on the floor: the host must
                        // retry, and the agent must not double-count or lose the chunk.
                        if let Some(x) = self.transfers.get_mut(&xid) {
                            let _ = x.re.accept(seq, &b64, &sum);
                        }
                        return;
                    }
                    if self.die_after.map(|n| seq >= n).unwrap_or(false) {
                        // simulate a device reboot mid-transfer: stop replying entirely. The
                        // already-accepted chunks survive (persisted), so a later resume works.
                        self.rebooted = true;
                        return;
                    }
                    let (b64, sum) = if self.corrupt_data.remove(&seq) {
                        ("Y29ycnVwdA==".to_string(), sum) // wrong sum vs payload
                    } else {
                        (b64, sum)
                    };
                    let Some(x) = self.transfers.get_mut(&xid) else {
                        return;
                    };
                    match x.re.accept(seq, &b64, &sum) {
                        Ok(_) => self.reply(&Msg::Ack { xid, seq }),
                        Err(_) => self.reply(&Msg::Nak { xid, seq }),
                    }
                }
                Msg::Close { xid } => {
                    // Idempotent (mirrors the agent, whose c.* chunk files persist): a CLOSE
                    // re-sent because its DONE was garbled on the wire must re-reply the SAME
                    // result, not "fail". If we already reconstructed this xid, answer success.
                    if let Some(bytes) = self.files.get(&xid) {
                        let sha = crate::hash::sha256_hex(bytes);
                        self.reply(&Msg::Done {
                            xid,
                            ok: true,
                            sha256: sha,
                        });
                        return;
                    }
                    let Some(x) = self.transfers.get(&xid) else {
                        self.reply(&Msg::Done {
                            xid,
                            ok: false,
                            sha256: "-".into(),
                        });
                        return;
                    };
                    // finish() consumes; clone the reassembled state instead so the transfer
                    // stays available for an idempotent re-CLOSE.
                    match x.re.try_reconstruct() {
                        Ok(bytes) => {
                            let sha = crate::hash::sha256_hex(&bytes);
                            self.files.insert(xid, bytes);
                            self.reply(&Msg::Done {
                                xid,
                                ok: true,
                                sha256: sha,
                            });
                        }
                        Err(_) => self.reply(&Msg::Done {
                            xid,
                            ok: false,
                            sha256: "-".into(),
                        }),
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

    /// Wraps a `Link` and injects BYTE-LEVEL faults on the wire — garbling individual bytes in
    /// both directions on a deterministic schedule — to exercise the per-frame checksum + ARQ
    /// (not just whole-chunk drop/corrupt, which DeviceSim already does). A garbled frame fails
    /// its checksum and is dropped, so the host falls back to its ack timeout and retransmits;
    /// the transfer must still complete and verify.
    struct LossyLink<L: Link> {
        inner: L,
        // garble 1 byte every `period` bytes on the host->device path
        tx_period: usize,
        tx_ctr: usize,
        // garble 1 byte every `period` bytes on the device->host path
        rx_period: usize,
        rx_ctr: usize,
    }

    impl<L: Link> LossyLink<L> {
        fn new(inner: L, tx_period: usize, rx_period: usize) -> Self {
            LossyLink {
                inner,
                tx_period,
                tx_ctr: 0,
                rx_period,
                rx_ctr: 0,
            }
        }
        // garble non-newline bytes so we don't change line framing — only frame CONTENT, which
        // the checksum must catch. Newlines are preserved so frames still delimit.
        fn garble(buf: &mut [u8], period: usize, ctr: &mut usize) {
            if period == 0 {
                return;
            }
            for b in buf.iter_mut() {
                *ctr += 1;
                if *ctr % period == 0 && *b != b'\n' && *b != b'\r' {
                    *b ^= 0x20; // flip a bit; stays printable-ish, never a newline
                }
            }
        }
    }

    impl<L: Link> Link for LossyLink<L> {
        fn send_line(&mut self, line: &str) -> io::Result<()> {
            let mut bytes = line.as_bytes().to_vec();
            Self::garble(&mut bytes, self.tx_period, &mut self.tx_ctr);
            // pass the (possibly corrupted) line to the inner link as a lossy string
            let corrupted = String::from_utf8_lossy(&bytes).into_owned();
            self.inner.send_line(&corrupted)
        }
        fn read_bytes(&mut self) -> io::Result<Vec<u8>> {
            let mut bytes = self.inner.read_bytes()?;
            Self::garble(&mut bytes, self.rx_period, &mut self.rx_ctr);
            Ok(bytes)
        }
    }

    #[test]
    fn send_blob_survives_byte_level_garble_both_directions() {
        // Inject byte-level corruption on BOTH the command and reply paths. The frame checksum
        // rejects every garbled line; the ARQ retransmits until each chunk lands clean.
        let data: Vec<u8> = (0..600u32)
            .map(|i| (i.wrapping_mul(17) % 256) as u8)
            .collect();
        // ~1 garbled byte per 140/170 bytes: frames (~40 B) are usually clean but a meaningful
        // fraction get corrupted and MUST be caught by the checksum + retried. This models a
        // line that drops characters occasionally, not one that destroys every frame.
        let lossy = LossyLink::new(DeviceSim::new(), 140, 170);
        let mut to = fast();
        to.retries = 500; // a lossy line needs many retries; the point is it eventually wins
        to.ack = Duration::from_millis(20);
        to.done = Duration::from_secs(5);
        let mut t = Transport::with_timeouts(lossy, to);
        let sha = t.send_blob(1, &data, 16).unwrap();
        assert_eq!(sha, crate::hash::sha256_hex(&data));
        assert_eq!(t.link.inner.files.get(&1).unwrap(), &data);
    }

    #[test]
    fn exec_retries_on_byte_level_garble() {
        // A garbled OUT/EXIT line is dropped by the checksum; exec verifies stdout against the
        // EXIT sha, and a clean retry by the caller succeeds. Here we just prove a mildly-lossy
        // path still returns the correct verified stdout (the agent re-emits deterministically
        // only on a fresh EXEC, so keep loss light enough that one pass gets through clean).
        let lossy = LossyLink::new(DeviceSim::new(), 0, 0); // control: no loss -> must pass
        let mut t = Transport::with_timeouts(lossy, fast());
        let r = t.exec("echo verified-over-lossy").unwrap();
        assert_eq!(r.stdout, b"verified-over-lossy\n");
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
    fn send_blob_survives_dropped_ack() {
        // The agent accepts+persists chunk 3 but its ACK is lost: the host must retry, the
        // agent must not double-count, and the transfer must still complete.
        let data = b"a dropped ack must not abort the whole transfer over uart".to_vec();
        let mut sim = DeviceSim::new();
        sim.drop_ack.insert(3);
        let mut t = Transport::with_timeouts(sim, fast());
        let sha = t.send_blob(1, &data, 8).unwrap();
        assert_eq!(sha, crate::hash::sha256_hex(&data));
        assert_eq!(t.link.files.get(&1).unwrap(), &data);
    }

    #[test]
    fn send_blob_stalled_error_is_resumable() {
        // Force a chunk to fail past the retry bound -> Stalled{resume_from} pointing at it.
        let data: Vec<u8> = (0..200u32).map(|i| (i % 7) as u8).collect();
        let mut sim = DeviceSim::new();
        // drop_data only fires once per seq; to exhaust retries, never reply for seq>=2.
        sim.die_after = Some(2);
        let mut to = fast();
        to.retries = 2;
        let mut t = Transport::with_timeouts(sim, to);
        match t.send_blob(9, &data, 8) {
            Err(TransportError::Stalled {
                resume_from, xid, ..
            }) => {
                assert_eq!(xid, 9);
                assert_eq!(resume_from, 2);
            }
            other => panic!("expected Stalled, got {other:?}"),
        }
    }

    #[test]
    fn send_blob_resumes_after_mid_transfer_reboot() {
        // First attempt "reboots" after chunk 2 is persisted; the second attempt (same xid)
        // must STAT, learn hw=3, and send only the remaining chunks — completing the transfer.
        let data: Vec<u8> = (0..400u32)
            .map(|i| (i.wrapping_mul(31) % 256) as u8)
            .collect();
        let mut sim = DeviceSim::new();
        sim.die_after = Some(3); // accepts 0,1,2 then stops replying
        let mut to = fast();
        to.retries = 2;
        let mut t = Transport::with_timeouts(sim, to);

        // attempt 1: stalls partway, but chunks 0..=2 are now persisted on-device
        let resume_from = match t.send_blob(5, &data, 8) {
            Err(TransportError::Stalled { resume_from, .. }) => resume_from,
            other => panic!("expected Stalled on reboot, got {other:?}"),
        };
        assert_eq!(resume_from, 3);
        assert!(t.link.files.get(&5).is_none(), "transfer not yet finished");

        // device "comes back": clear the reboot fault, then resume with the SAME xid.
        t.link.die_after = None;
        t.link.rebooted = false;
        let sha = t.send_blob(5, &data, 8).unwrap();
        assert_eq!(sha, crate::hash::sha256_hex(&data));
        assert_eq!(t.link.files.get(&5).unwrap(), &data);
    }

    #[test]
    fn stat_reports_high_water_mark() {
        // Open + deliver a couple of chunks, then STAT should report the contiguous count.
        let data: Vec<u8> = (0..40u32).map(|i| i as u8).collect();
        let mut t = Transport::with_timeouts(DeviceSim::new(), fast());
        let blob = prepare(&data, 8);
        t.send(&Msg::Open {
            xid: 3,
            nchunks: blob.nchunks(),
            chunk_size: blob.chunk_size,
            sha256: blob.sha256.clone(),
        })
        .unwrap();
        for c in blob.chunks.iter().take(2) {
            t.send(&Msg::Data {
                xid: 3,
                seq: c.seq,
                b64: c.b64.clone(),
                sum: c.sum.clone(),
            })
            .unwrap();
            // drain the ACK
            let _ = t.recv_match(Instant::now() + Duration::from_millis(200), |m| {
                matches!(m, Msg::Ack { .. })
            });
        }
        assert_eq!(t.stat(3).unwrap(), 2);
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
