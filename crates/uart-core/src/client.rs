// SPDX-License-Identifier: Apache-2.0
//
// Thin client used by the `uart` CLI (and the test suite) to talk to the daemon: connect to
// the Unix socket, write one request line, read one response line. Blocking requests
// (`wait`, `send --expect`) hold the connection open with no read timeout, so a long wait is
// fine; a missing daemon surfaces as a clear error rather than a hang.

use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;

use crate::proto::{Request, Response, from_line, to_line};

/// Connect, send `req`, and return the daemon's response.
pub fn send_request(socket: &Path, req: &Request) -> io::Result<Response> {
    let stream = UnixStream::connect(socket).map_err(|e| match e.kind() {
        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused => io::Error::new(
            e.kind(),
            format!(
                "uartd is not running (no socket at {}). Start it with `uart start` or `uartd`.",
                socket.display()
            ),
        ),
        _ => e,
    })?;

    let mut writer = stream.try_clone()?;
    writer.write_all(to_line(req).as_bytes())?;
    writer.flush()?;

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    let n = reader.read_line(&mut line)?;
    if n == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "daemon closed the connection without responding",
        ));
    }
    from_line::<Response>(&line).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}
