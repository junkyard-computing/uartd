// SPDX-License-Identifier: Apache-2.0
//
// uartfs-frontend — the on-device, pty-owning console front-end (tier 2). It speaks the same
// framed, checksummed uartfs protocol as the shell agent, but compiled (robust, fast) and meant
// to OWN the serial line (replace/wrap serial-getty) so every byte in and out is framed and
// validated — login, commands, output, and (T2b) interactive sessions alike.
//
// This file is the exec + blob server: it reads frames from stdin (the serial line) and writes
// replies to stdout. Because it implements the device side of the uartfs protocol, the existing
// host `Transport`/`commands` drive it unchanged. Interactive attach (forkpty bridge) is added
// in a later milestone.

use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::Command;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;

use uartfs::chunk::Reassembler;
use uartfs::frame::{Dir, FrameReader};
use uartfs::hash::sha256_hex;
use uartfs::msg::Msg;

const OUT_WIDTH: usize = 512; // chars of base64 per OUT frame

struct Xfer {
    re: Reassembler,
}

struct Frontend {
    base: PathBuf,
    transfers: HashMap<u32, Xfer>,
    out: std::io::Stdout,
}

impl Frontend {
    fn new() -> Self {
        let base = std::env::var_os("UARTFS_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/tmp/uartfs"));
        let _ = fs::create_dir_all(&base);
        Frontend {
            base,
            transfers: HashMap::new(),
            out: std::io::stdout(),
        }
    }

    fn reply(&mut self, m: &Msg) {
        let _ = self.out.write_all(m.to_frame().encode_line().as_bytes());
        let _ = self.out.flush();
    }

    fn handle(&mut self, m: Msg) {
        match m {
            Msg::Ping => self.reply(&Msg::Ready {
                version: "fe1".into(),
            }),
            Msg::Open {
                xid,
                nchunks,
                sha256,
                ..
            } => {
                // keep an existing partial transfer for the same xid (resume); else start fresh
                self.transfers
                    .entry(xid)
                    .or_insert_with(|| Xfer {
                        re: Reassembler::new(nchunks, sha256),
                    });
            }
            Msg::Data { xid, seq, b64, sum } => {
                let reply = match self.transfers.get_mut(&xid) {
                    Some(x) => match x.re.accept(seq, &b64, &sum) {
                        Ok(_) => Msg::Ack { xid, seq },
                        Err(_) => Msg::Nak { xid, seq },
                    },
                    None => Msg::Nak { xid, seq },
                };
                self.reply(&reply);
            }
            Msg::Stat { xid } => {
                let hw = self.transfers.get(&xid).map(|x| x.re.contiguous_have()).unwrap_or(0);
                self.reply(&Msg::Have { xid, hw });
            }
            Msg::Close { xid } => {
                let reply = match self.transfers.remove(&xid) {
                    Some(x) => match x.re.finish() {
                        Ok(bytes) => {
                            let dir = self.base.join(xid.to_string());
                            let _ = fs::create_dir_all(&dir);
                            match fs::write(dir.join("out"), &bytes) {
                                Ok(_) => Msg::Done {
                                    xid,
                                    ok: true,
                                    sha256: sha256_hex(&bytes),
                                },
                                Err(_) => Msg::Done {
                                    xid,
                                    ok: false,
                                    sha256: "-".into(),
                                },
                            }
                        }
                        Err(_) => Msg::Done {
                            xid,
                            ok: false,
                            sha256: "-".into(),
                        },
                    },
                    None => Msg::Done {
                        xid,
                        ok: false,
                        sha256: "-".into(),
                    },
                };
                self.reply(&reply);
            }
            Msg::Exec { cid, b64cmd } => self.exec(cid, &b64cmd),
            // device->host messages are never received here
            _ => {}
        }
    }

    fn exec(&mut self, cid: u32, b64cmd: &str) {
        let cmd = B64.decode(b64cmd.as_bytes()).unwrap_or_default();
        let cmd = String::from_utf8_lossy(&cmd).into_owned();
        let output = Command::new("sh").arg("-c").arg(&cmd).output();
        let (code, stdout, stderr) = match output {
            Ok(o) => (
                o.status.code().unwrap_or(-1),
                o.stdout,
                o.stderr,
            ),
            Err(e) => (127, Vec::new(), e.to_string().into_bytes()),
        };
        let out_frames = self.emit_stream(cid, 1, &stdout);
        self.emit_stream(cid, 2, &stderr);
        self.reply(&Msg::Exit {
            cid,
            code,
            out_frames,
            out_sha: sha256_hex(&stdout),
        });
    }

    /// Base64 the whole stream, fold into fixed-width OUT frames; returns the frame count.
    fn emit_stream(&mut self, cid: u32, stream: u8, data: &[u8]) -> u32 {
        let whole = B64.encode(data);
        if whole.is_empty() {
            return 0;
        }
        let mut seq = 0u32;
        let mut i = 0;
        while i < whole.len() {
            let end = (i + OUT_WIDTH).min(whole.len());
            self.reply(&Msg::Out {
                cid,
                stream,
                seq,
                b64: whole[i..end].to_string(),
            });
            seq += 1;
            i = end;
        }
        seq
    }
}

fn main() {
    let mut fe = Frontend::new();
    fe.reply(&Msg::Ready {
        version: "fe1".into(),
    });

    let mut reader = FrameReader::new();
    let mut stdin = std::io::stdin();
    let mut buf = [0u8; 4096];
    loop {
        match stdin.read(&mut buf) {
            Ok(0) => break, // EOF: line closed
            Ok(n) => {
                for f in reader.push(&buf[..n]) {
                    if f.dir == Dir::ToDevice
                        && let Some(m) = Msg::from_frame(&f)
                    {
                        fe.handle(m);
                    }
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
}
