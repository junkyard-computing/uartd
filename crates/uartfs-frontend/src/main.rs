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
use std::ffi::CString;
use std::fs;
use std::io::Write;
use std::os::unix::io::RawFd;
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
                self.transfers.entry(xid).or_insert_with(|| Xfer {
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
                let hw = self
                    .transfers
                    .get(&xid)
                    .map(|x| x.re.contiguous_have())
                    .unwrap_or(0);
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
            // Attach is handled in the main loop (it needs the shared frame reader); everything
            // else (device->host messages, stray Attach* mid-exec) is ignored here.
            _ => {}
        }
    }

    /// Interactive session: forkpty a shell and bridge it to the host, framed both ways, until
    /// the shell exits or the host detaches. `reader` is the shared stdin frame decoder.
    fn attach(&mut self, cols: u16, rows: u16, reader: &mut FrameReader) {
        let Some((pid, master)) = fork_shell(cols, rows) else {
            self.reply(&Msg::AttachEnd { code: -1 });
            return;
        };

        let mut fds = [
            libc::pollfd {
                fd: 0,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: master,
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let mut buf = [0u8; 4096];
        let code: i32;

        loop {
            unsafe { libc::poll(fds.as_mut_ptr(), 2, 200) };

            // host -> child: stdin frames
            if fds[0].revents != 0 {
                let n = read_fd(0, &mut buf);
                if n <= 0 {
                    code = -1; // host line closed
                    unsafe { libc::kill(pid, libc::SIGHUP) };
                    break;
                }
                let mut detached = false;
                for f in reader.push(&buf[..n as usize]) {
                    if f.dir == Dir::ToDevice
                        && let Some(m) = Msg::from_frame(&f)
                    {
                        match m {
                            Msg::AttachIn { b64 } => {
                                if let Ok(d) = B64.decode(b64.as_bytes()) {
                                    write_fd_all(master, &d);
                                }
                            }
                            Msg::Winsize { cols, rows } => set_winsize(master, cols, rows),
                            Msg::Detach => detached = true,
                            _ => {} // ignore other frames mid-attach
                        }
                    }
                }
                if detached {
                    unsafe { libc::kill(pid, libc::SIGHUP) };
                    code = 0;
                    break;
                }
            }

            // child -> host: pty output
            if fds[1].revents & (libc::POLLIN | libc::POLLHUP) != 0 {
                let n = read_fd(master, &mut buf);
                if n <= 0 {
                    code = reap(pid);
                    break;
                }
                self.reply(&Msg::AttachOut {
                    b64: B64.encode(&buf[..n as usize]),
                });
            }
        }

        unsafe {
            libc::close(master);
            let mut st = 0;
            libc::waitpid(pid, &mut st, 0);
        }
        self.reply(&Msg::AttachEnd { code });
    }

    fn exec(&mut self, cid: u32, b64cmd: &str) {
        let cmd = B64.decode(b64cmd.as_bytes()).unwrap_or_default();
        let cmd = String::from_utf8_lossy(&cmd).into_owned();
        let output = Command::new("sh").arg("-c").arg(&cmd).output();
        let (code, stdout, stderr) = match output {
            Ok(o) => (o.status.code().unwrap_or(-1), o.stdout, o.stderr),
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

fn read_fd(fd: RawFd, buf: &mut [u8]) -> isize {
    unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) }
}

fn write_fd_all(fd: RawFd, mut data: &[u8]) {
    while !data.is_empty() {
        let n = unsafe { libc::write(fd, data.as_ptr() as *const libc::c_void, data.len()) };
        if n <= 0 {
            break;
        }
        data = &data[n as usize..];
    }
}

fn set_winsize(fd: RawFd, cols: u16, rows: u16) {
    let ws = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    unsafe {
        libc::ioctl(fd, libc::TIOCSWINSZ, &ws);
    }
}

fn reap(pid: libc::pid_t) -> i32 {
    let mut st = 0;
    unsafe { libc::waitpid(pid, &mut st, 0) };
    if libc::WIFEXITED(st) {
        libc::WEXITSTATUS(st)
    } else {
        -1
    }
}

/// forkpty a login shell sized cols x rows; returns (child pid, pty master fd).
fn fork_shell(cols: u16, rows: u16) -> Option<(libc::pid_t, RawFd)> {
    let mut master: libc::c_int = 0;
    let ws = libc::winsize {
        ws_row: rows.max(1),
        ws_col: cols.max(1),
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let pid =
        unsafe { libc::forkpty(&raw mut master, std::ptr::null_mut(), std::ptr::null(), &ws) };
    if pid < 0 {
        return None;
    }
    if pid == 0 {
        // child: the pty slave is already our stdio. Exec an interactive shell (bash, then sh).
        exec_shell();
        unsafe { libc::_exit(127) };
    }
    Some((pid, master))
}

fn exec_shell() {
    let argi = CString::new("-i").unwrap();
    for sh in ["/bin/bash", "/bin/sh"] {
        let prog = CString::new(sh).unwrap();
        let argv = [prog.as_ptr(), argi.as_ptr(), std::ptr::null()];
        unsafe { libc::execv(prog.as_ptr(), argv.as_ptr()) };
        // if execv returns, try the next shell
    }
}

fn main() {
    let mut fe = Frontend::new();
    fe.reply(&Msg::Ready {
        version: "fe1".into(),
    });

    let mut reader = FrameReader::new();
    let mut buf = [0u8; 4096];
    loop {
        let n = read_fd(0, &mut buf);
        if n == 0 {
            break; // EOF: line closed
        }
        if n < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            break;
        }
        for f in reader.push(&buf[..n as usize]) {
            if f.dir == Dir::ToDevice
                && let Some(m) = Msg::from_frame(&f)
            {
                if let Msg::Attach { cols, rows } = m {
                    fe.attach(cols, rows, &mut reader);
                } else {
                    fe.handle(m);
                }
            }
        }
    }
}
