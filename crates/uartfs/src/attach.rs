// SPDX-License-Identifier: Apache-2.0
//
// Host side of interactive attach: put the local terminal in raw mode and bridge it to the
// front-end's forkpty'd shell over the framed channel — host keystrokes -> AttachIn frames,
// AttachOut frames -> local stdout. Both directions ride the reliable, checksummed line, so an
// interactive session (sudo prompt, vim, menuconfig, a live console) finally works over UART.
//
// Detach with Ctrl-] (like telnet). The device I/O is generic over `Link`; the terminal
// handling is host-only libc.

use std::io::{self, Write};
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;

use crate::frame::{Dir, FrameReader};
use crate::msg::Msg;
use crate::transport::Link;

const DETACH_KEY: u8 = 0x1d; // Ctrl-]

/// Restores the terminal's termios on drop.
struct RawGuard {
    fd: i32,
    saved: libc::termios,
}

impl RawGuard {
    fn new(fd: i32) -> Option<Self> {
        unsafe {
            if libc::isatty(fd) != 1 {
                return None;
            }
            let mut saved: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(fd, &mut saved) != 0 {
                return None;
            }
            let mut raw = saved;
            libc::cfmakeraw(&mut raw);
            libc::tcsetattr(fd, libc::TCSANOW, &raw);
            Some(RawGuard { fd, saved })
        }
    }
}

impl Drop for RawGuard {
    fn drop(&mut self) {
        unsafe {
            libc::tcsetattr(self.fd, libc::TCSANOW, &self.saved);
        }
    }
}

fn winsize(fd: i32) -> (u16, u16) {
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_col > 0 {
            (ws.ws_col, ws.ws_row)
        } else {
            (80, 24)
        }
    }
}

fn set_nonblocking(fd: i32) {
    unsafe {
        let fl = libc::fcntl(fd, libc::F_GETFL);
        libc::fcntl(fd, libc::F_SETFL, fl | libc::O_NONBLOCK);
    }
}

/// Run an interactive session over `link` until the remote shell exits or the user hits Ctrl-].
/// Returns the shell's exit code.
pub fn attach<L: Link>(link: &mut L) -> io::Result<i32> {
    let (cols, rows) = winsize(1);
    let _guard = RawGuard::new(0);
    set_nonblocking(0);

    link.send_line(&Msg::Attach { cols, rows }.to_frame().encode())?;

    eprintln!("[uartfs attach — Ctrl-] to detach]\r");

    let mut reader = FrameReader::new();
    let mut sbuf = [0u8; 4096];
    let stdout = io::stdout();

    loop {
        // local keystrokes -> device
        let n = unsafe { libc::read(0, sbuf.as_mut_ptr() as *mut libc::c_void, sbuf.len()) };
        if n > 0 {
            let data = &sbuf[..n as usize];
            if data.contains(&DETACH_KEY) {
                link.send_line(&Msg::Detach.to_frame().encode())?;
                return Ok(0);
            }
            link.send_line(
                &Msg::AttachIn {
                    b64: B64.encode(data),
                }
                .to_frame()
                .encode(),
            )?;
        }

        // device output -> local terminal
        let bytes = link.read_bytes()?;
        for f in reader.push(&bytes) {
            if f.dir != Dir::ToHost {
                continue;
            }
            match Msg::from_frame(&f) {
                Some(Msg::AttachOut { b64 }) => {
                    if let Ok(d) = B64.decode(b64.as_bytes()) {
                        let mut h = stdout.lock();
                        h.write_all(&d)?;
                        h.flush()?;
                    }
                }
                Some(Msg::AttachEnd { code }) => return Ok(code),
                _ => {}
            }
        }

        std::thread::sleep(Duration::from_millis(5));
    }
}
