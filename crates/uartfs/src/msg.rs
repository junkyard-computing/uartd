// SPDX-License-Identifier: Apache-2.0
//
// Typed protocol messages over the frame layer.
//
//   host -> device:  PING · OPEN · DATA · CLOSE · EXEC · STAT
//   device -> host:  READY · ACK · NAK · DONE · OUT · EXIT · HAVE
//
// A transfer (OPEN/DATA/CLOSE -> ACK/NAK/DONE) moves a verified blob into a device temp file.
// EXEC runs a shell command and streams stdout/stderr back (OUT) with a verifiable EXIT, which
// also serves as `pull` (the command's stdout *is* the pulled bytes).
//
// Resumability: OPEN does NOT discard chunks already persisted for the same xid. STAT xid asks
// the agent how many contiguous chunks (from seq 0) it already holds; it answers HAVE xid hw.
// Because delivery is in-order stop-and-wait, that high-water mark is exactly the resume point,
// so a transfer interrupted by a device reboot resumes instead of restarting at chunk 0.

use crate::frame::{Dir, Frame};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Msg {
    // host -> device
    Ping,
    Open {
        xid: u32,
        nchunks: u32,
        chunk_size: u32,
        sha256: String,
    },
    Data {
        xid: u32,
        seq: u32,
        b64: String,
        sum: String,
    },
    Close {
        xid: u32,
    },
    /// Ask the agent how many contiguous chunks (from seq 0) it already has for `xid`.
    Stat {
        xid: u32,
    },
    Exec {
        cid: u32,
        b64cmd: String,
    },
    /// Start an interactive session: forkpty a shell sized cols x rows and bridge it.
    Attach {
        cols: u16,
        rows: u16,
    },
    /// Keystrokes (base64) for the attached session's pty stdin.
    AttachIn {
        b64: String,
    },
    /// Terminal resize for the attached session.
    Winsize {
        cols: u16,
        rows: u16,
    },
    /// End the interactive session.
    Detach,
    // device -> host
    Ready {
        version: String,
    },
    Ack {
        xid: u32,
        seq: u32,
    },
    Nak {
        xid: u32,
        seq: u32,
    },
    /// Reply to STAT: `hw` = number of contiguous verified chunks held from seq 0 (the resume
    /// point — host should resend starting at seq `hw`).
    Have {
        xid: u32,
        hw: u32,
    },
    Done {
        xid: u32,
        ok: bool,
        sha256: String,
    },
    Out {
        cid: u32,
        stream: u8, // 1=stdout 2=stderr
        seq: u32,
        b64: String,
    },
    Exit {
        cid: u32,
        code: i32,
        out_frames: u32,
        out_sha: String,
    },
    /// Output (base64) from the attached session's pty.
    AttachOut {
        b64: String,
    },
    /// The attached session's shell exited with `code`.
    AttachEnd {
        code: i32,
    },
}

impl Msg {
    pub fn to_frame(&self) -> Frame {
        match self {
            Msg::Ping => Frame::new(Dir::ToDevice, "PING", vec![]),
            Msg::Open {
                xid,
                nchunks,
                chunk_size,
                sha256,
            } => Frame::new(
                Dir::ToDevice,
                "OPEN",
                vec![s(xid), s(nchunks), s(chunk_size), sha256.clone()],
            ),
            Msg::Data { xid, seq, b64, sum } => Frame::new(
                Dir::ToDevice,
                "DATA",
                vec![s(xid), s(seq), b64.clone(), sum.clone()],
            ),
            Msg::Close { xid } => Frame::new(Dir::ToDevice, "CLOSE", vec![s(xid)]),
            Msg::Stat { xid } => Frame::new(Dir::ToDevice, "STAT", vec![s(xid)]),
            Msg::Exec { cid, b64cmd } => {
                Frame::new(Dir::ToDevice, "EXEC", vec![s(cid), b64cmd.clone()])
            }
            Msg::Attach { cols, rows } => {
                Frame::new(Dir::ToDevice, "ATTACH", vec![s(cols), s(rows)])
            }
            Msg::AttachIn { b64 } => Frame::new(Dir::ToDevice, "ATTACHIN", vec![b64.clone()]),
            Msg::Winsize { cols, rows } => {
                Frame::new(Dir::ToDevice, "WINSIZE", vec![s(cols), s(rows)])
            }
            Msg::Detach => Frame::new(Dir::ToDevice, "DETACH", vec![]),
            Msg::Ready { version } => Frame::new(Dir::ToHost, "READY", vec![version.clone()]),
            Msg::Ack { xid, seq } => Frame::new(Dir::ToHost, "ACK", vec![s(xid), s(seq)]),
            Msg::Nak { xid, seq } => Frame::new(Dir::ToHost, "NAK", vec![s(xid), s(seq)]),
            Msg::Have { xid, hw } => Frame::new(Dir::ToHost, "HAVE", vec![s(xid), s(hw)]),
            Msg::Done { xid, ok, sha256 } => Frame::new(
                Dir::ToHost,
                "DONE",
                vec![s(xid), bool_tok(*ok).into(), sha256.clone()],
            ),
            Msg::Out {
                cid,
                stream,
                seq,
                b64,
            } => Frame::new(
                Dir::ToHost,
                "OUT",
                vec![s(cid), s(stream), s(seq), b64.clone()],
            ),
            Msg::Exit {
                cid,
                code,
                out_frames,
                out_sha,
            } => Frame::new(
                Dir::ToHost,
                "EXIT",
                vec![s(cid), s(code), s(out_frames), out_sha.clone()],
            ),
            Msg::AttachOut { b64 } => Frame::new(Dir::ToHost, "AOUT", vec![b64.clone()]),
            Msg::AttachEnd { code } => Frame::new(Dir::ToHost, "AEND", vec![s(code)]),
        }
    }

    pub fn encode_line(&self) -> String {
        self.to_frame().encode_line()
    }

    pub fn from_frame(f: &Frame) -> Option<Msg> {
        let a = &f.args;
        Some(match f.kind.as_str() {
            "PING" => Msg::Ping,
            "OPEN" => Msg::Open {
                xid: p(a, 0)?,
                nchunks: p(a, 1)?,
                chunk_size: p(a, 2)?,
                sha256: a.get(3)?.clone(),
            },
            "DATA" => Msg::Data {
                xid: p(a, 0)?,
                seq: p(a, 1)?,
                b64: a.get(2)?.clone(),
                sum: a.get(3)?.clone(),
            },
            "CLOSE" => Msg::Close { xid: p(a, 0)? },
            "STAT" => Msg::Stat { xid: p(a, 0)? },
            "EXEC" => Msg::Exec {
                cid: p(a, 0)?,
                b64cmd: a.get(1)?.clone(),
            },
            "ATTACH" => Msg::Attach {
                cols: p(a, 0)?,
                rows: p(a, 1)?,
            },
            "ATTACHIN" => Msg::AttachIn {
                b64: a.first()?.clone(),
            },
            "WINSIZE" => Msg::Winsize {
                cols: p(a, 0)?,
                rows: p(a, 1)?,
            },
            "DETACH" => Msg::Detach,
            "READY" => Msg::Ready {
                version: a.first()?.clone(),
            },
            "ACK" => Msg::Ack {
                xid: p(a, 0)?,
                seq: p(a, 1)?,
            },
            "NAK" => Msg::Nak {
                xid: p(a, 0)?,
                seq: p(a, 1)?,
            },
            "HAVE" => Msg::Have {
                xid: p(a, 0)?,
                hw: p(a, 1)?,
            },
            "DONE" => Msg::Done {
                xid: p(a, 0)?,
                ok: parse_bool(a.get(1)?)?,
                sha256: a.get(2)?.clone(),
            },
            "OUT" => Msg::Out {
                cid: p(a, 0)?,
                stream: p(a, 1)?,
                seq: p(a, 2)?,
                b64: a.get(3)?.clone(),
            },
            "EXIT" => Msg::Exit {
                cid: p(a, 0)?,
                code: p(a, 1)?,
                out_frames: p(a, 2)?,
                out_sha: a.get(3)?.clone(),
            },
            "AOUT" => Msg::AttachOut {
                b64: a.first()?.clone(),
            },
            "AEND" => Msg::AttachEnd { code: p(a, 0)? },
            _ => return None,
        })
    }
}

fn s<T: std::fmt::Display>(v: T) -> String {
    v.to_string()
}
fn p<T: std::str::FromStr>(a: &[String], i: usize) -> Option<T> {
    a.get(i)?.parse().ok()
}
fn bool_tok(b: bool) -> &'static str {
    if b { "ok" } else { "fail" }
}
fn parse_bool(s: &str) -> Option<bool> {
    match s {
        "ok" => Some(true),
        "fail" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::parse_line;

    fn rt(m: Msg) {
        let line = m.encode_line();
        let f = parse_line(&line).expect("frame parses");
        assert_eq!(Msg::from_frame(&f), Some(m));
    }

    #[test]
    fn all_messages_roundtrip() {
        rt(Msg::Ping);
        rt(Msg::Open {
            xid: 1,
            nchunks: 5,
            chunk_size: 1024,
            sha256: "abcd".into(),
        });
        rt(Msg::Data {
            xid: 1,
            seq: 0,
            b64: "QUJD".into(),
            sum: "deadbeef".into(),
        });
        rt(Msg::Close { xid: 1 });
        rt(Msg::Stat { xid: 1 });
        rt(Msg::Have { xid: 1, hw: 3 });
        rt(Msg::Exec {
            cid: 9,
            b64cmd: "ZG1lc2c=".into(),
        });
        rt(Msg::Attach { cols: 80, rows: 24 });
        rt(Msg::AttachIn { b64: "bHMK".into() });
        rt(Msg::Winsize {
            cols: 132,
            rows: 43,
        });
        rt(Msg::Detach);
        rt(Msg::AttachOut {
            b64: "aGVsbG8=".into(),
        });
        rt(Msg::AttachEnd { code: 0 });
        rt(Msg::Ready {
            version: "1".into(),
        });
        rt(Msg::Ack { xid: 1, seq: 0 });
        rt(Msg::Nak { xid: 1, seq: 2 });
        rt(Msg::Done {
            xid: 1,
            ok: true,
            sha256: "abcd".into(),
        });
        rt(Msg::Done {
            xid: 1,
            ok: false,
            sha256: "-".into(),
        });
        rt(Msg::Out {
            cid: 9,
            stream: 1,
            seq: 3,
            b64: "aGk=".into(),
        });
        rt(Msg::Exit {
            cid: 9,
            code: 0,
            out_frames: 4,
            out_sha: "abcd".into(),
        });
    }

    #[test]
    fn negative_exit_code() {
        rt(Msg::Exit {
            cid: 1,
            code: -1,
            out_frames: 0,
            out_sha: "-".into(),
        });
    }

    #[test]
    fn unknown_kind_is_none() {
        let line = Frame::new(Dir::ToDevice, "WAT", vec!["1".into(), "2".into()]).encode();
        let f = parse_line(&line).unwrap();
        assert!(Msg::from_frame(&f).is_none());
    }

    #[test]
    fn short_args_is_none() {
        let line = Frame::new(Dir::ToDevice, "OPEN", vec!["1".into()]).encode();
        let f = parse_line(&line).unwrap();
        assert!(Msg::from_frame(&f).is_none());
    }
}
