// SPDX-License-Identifier: Apache-2.0
//
// A `Link` over the live uartd console: each frame is sent as a `uart send` (paced,
// flow-control-safe) and replies are drained via `uart read`. This is how uartfs rides the
// port without fighting uartd for it — uartd stays the single owner, uartfs is a consumer of
// its socket. Frames are ASCII, so they pass cleanly through uartd's text channel.

use std::io;
use std::path::PathBuf;

use uart_core::client::send_request;
use uart_core::proto::{Request, Response};

use crate::transport::Link;

pub struct ClientLink {
    socket: PathBuf,
}

impl ClientLink {
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        ClientLink {
            socket: socket.into(),
        }
    }
}

impl Link for ClientLink {
    fn send_line(&mut self, line: &str) -> io::Result<()> {
        let req = Request::Send {
            text: line.to_string(),
            no_newline: false,
            expect: None,
            timeout_ms: None,
        };
        match send_request(&self.socket, &req)? {
            Response::Ok => Ok(()),
            Response::Error { message } => Err(io::Error::other(message)),
            _ => Ok(()),
        }
    }

    fn read_bytes(&mut self) -> io::Result<Vec<u8>> {
        match send_request(&self.socket, &Request::Read)? {
            Response::Read { text, .. } => Ok(text.into_bytes()),
            _ => Ok(Vec::new()),
        }
    }
}
