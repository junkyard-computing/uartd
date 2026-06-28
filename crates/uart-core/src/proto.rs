// SPDX-License-Identifier: Apache-2.0
//
// The control protocol between `uart` (CLI) and `uartd` (daemon): newline-delimited JSON over
// the Unix socket — one request object per line, one response object per line. The same
// response types back the `--json` CLI output, so the wire format IS the public structured
// interface other tools (e.g. benchctl) parse. The shape is locked by tests below.

use serde::{Deserialize, Serialize};

use crate::lines::Line;

/// A request from the CLI to the daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    /// Return + clear everything since the last read.
    Read,
    /// Return everything since the last read, without clearing.
    Peek,
    /// Write input to the device (paced). Optionally block until `expect` matches.
    Send {
        text: String,
        #[serde(default)]
        no_newline: bool,
        #[serde(default)]
        expect: Option<String>,
        #[serde(default)]
        timeout_ms: Option<u64>,
    },
    /// Block until `pattern` appears in the stream (no send).
    Wait { pattern: String, timeout_ms: u64 },
    /// Daemon health + port state.
    Status,
    /// Path to the forensic log file.
    Log,
    /// Ask the daemon to shut down.
    Stop,
}

/// A single timestamped line in `--json` output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineJson {
    pub mono_ns: u64,
    pub wall_ms: u64,
    pub text: String,
}

impl From<Line> for LineJson {
    fn from(l: Line) -> Self {
        LineJson {
            mono_ns: l.mono_ns,
            wall_ms: l.wall_ms,
            text: l.text,
        }
    }
}

/// A response from the daemon to the CLI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    /// Result of `read`/`peek`. `text` is the concatenated bytes (with any drop marker);
    /// `lines` is the framed view for `--json`.
    Read {
        dropped: u64,
        text: String,
        lines: Vec<LineJson>,
    },
    /// Result of `wait` / `send --expect`.
    Match {
        matched: bool,
        #[serde(default)]
        matched_text: Option<String>,
        context: String,
        timed_out: bool,
    },
    /// Result of `status`.
    Status {
        running: bool,
        port: String,
        baud: u32,
        connected: bool,
        buffer_bytes: usize,
        uptime_s: u64,
        log_path: String,
    },
    /// Result of `log`.
    Log { path: String },
    /// Generic success (e.g. plain `send`, `stop`).
    Ok,
    /// Something went wrong handling the request.
    Error { message: String },
}

/// Serialize a message as one wire line (JSON + trailing newline, no interior newlines).
pub fn to_line<T: Serialize>(msg: &T) -> String {
    let mut s = serde_json::to_string(msg).expect("protocol types always serialize");
    s.push('\n');
    s
}

/// Parse one wire line into a message.
pub fn from_line<T: for<'de> Deserialize<'de>>(line: &str) -> Result<T, serde_json::Error> {
    serde_json::from_str(line.trim_end_matches(['\n', '\r']))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req_roundtrip(r: Request) {
        let line = to_line(&r);
        assert!(line.ends_with('\n'));
        assert_eq!(line.matches('\n').count(), 1, "no interior newlines");
        let back: Request = from_line(&line).unwrap();
        assert_eq!(r, back);
    }

    fn resp_roundtrip(r: Response) {
        let line = to_line(&r);
        let back: Response = from_line(&line).unwrap();
        assert_eq!(r, back);
    }

    #[test]
    fn all_requests_roundtrip() {
        req_roundtrip(Request::Read);
        req_roundtrip(Request::Peek);
        req_roundtrip(Request::Send {
            text: "echo hi".into(),
            no_newline: false,
            expect: Some(r"\$ ".into()),
            timeout_ms: Some(5000),
        });
        req_roundtrip(Request::Wait {
            pattern: "login:".into(),
            timeout_ms: 30000,
        });
        req_roundtrip(Request::Status);
        req_roundtrip(Request::Log);
        req_roundtrip(Request::Stop);
    }

    #[test]
    fn all_responses_roundtrip() {
        resp_roundtrip(Response::Read {
            dropped: 0,
            text: "hi\n".into(),
            lines: vec![LineJson {
                mono_ns: 1,
                wall_ms: 2,
                text: "hi".into(),
            }],
        });
        resp_roundtrip(Response::Match {
            matched: true,
            matched_text: Some("$ ".into()),
            context: "...$ ".into(),
            timed_out: false,
        });
        resp_roundtrip(Response::Status {
            running: true,
            port: "/dev/ttyUSB0".into(),
            baud: 115200,
            connected: true,
            buffer_bytes: 10,
            uptime_s: 42,
            log_path: "/tmp/x.log".into(),
        });
        resp_roundtrip(Response::Log {
            path: "/tmp/x.log".into(),
        });
        resp_roundtrip(Response::Ok);
        resp_roundtrip(Response::Error {
            message: "boom".into(),
        });
    }

    #[test]
    fn read_request_wire_shape_is_locked() {
        assert_eq!(to_line(&Request::Read), "{\"cmd\":\"read\"}\n");
    }

    #[test]
    fn send_request_wire_shape_is_locked() {
        let r = Request::Send {
            text: "x".into(),
            no_newline: true,
            expect: None,
            timeout_ms: None,
        };
        assert_eq!(
            to_line(&r),
            "{\"cmd\":\"send\",\"text\":\"x\",\"no_newline\":true,\"expect\":null,\"timeout_ms\":null}\n"
        );
    }

    #[test]
    fn send_request_accepts_minimal_form() {
        // defaults fill in the optional fields
        let r: Request = from_line("{\"cmd\":\"send\",\"text\":\"x\"}").unwrap();
        assert_eq!(
            r,
            Request::Send {
                text: "x".into(),
                no_newline: false,
                expect: None,
                timeout_ms: None
            }
        );
    }

    #[test]
    fn status_response_wire_shape_is_locked() {
        let r = Response::Status {
            running: true,
            port: "/dev/ttyUSB0".into(),
            baud: 115200,
            connected: false,
            buffer_bytes: 0,
            uptime_s: 1,
            log_path: "/l".into(),
        };
        assert_eq!(
            to_line(&r),
            "{\"type\":\"status\",\"running\":true,\"port\":\"/dev/ttyUSB0\",\"baud\":115200,\"connected\":false,\"buffer_bytes\":0,\"uptime_s\":1,\"log_path\":\"/l\"}\n"
        );
    }

    #[test]
    fn line_converts_from_framer_line() {
        let l = Line {
            mono_ns: 7,
            wall_ms: 8,
            text: "z".into(),
        };
        let j: LineJson = l.into();
        assert_eq!(
            j,
            LineJson {
                mono_ns: 7,
                wall_ms: 8,
                text: "z".into()
            }
        );
    }
}
