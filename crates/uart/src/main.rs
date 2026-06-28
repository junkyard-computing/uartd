// SPDX-License-Identifier: Apache-2.0
//
// uart CLI: the thin per-turn client an agent invokes. Stable, scriptable output and
// meaningful exit codes:
//   0  success / pattern matched
//   1  timeout (expect/wait found nothing)
//   2  daemon not running / connection error
//   3  daemon returned an error
//
// `--json` emits the raw protocol response (the locked schema) for reliable parsing.

use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use clap::{Parser, Subcommand};

use uart_core::client::send_request;
use uart_core::config::DEFAULT_SOCKET;
use uart_core::proto::{Request, Response};

#[derive(Parser)]
#[command(name = "uart", about = "Client for the uartd serial console daemon")]
struct Cli {
    /// Control socket path (default /tmp/uartd.sock).
    #[arg(long, global = true)]
    socket: Option<PathBuf>,
    /// Emit structured JSON instead of plain text.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Return + clear everything captured since the last read.
    Read,
    /// Like read, but do not clear the buffer.
    Peek,
    /// Send input to the device. Appends a newline unless --no-newline.
    Send {
        text: String,
        #[arg(long)]
        no_newline: bool,
        /// Block until this regex matches the reply; exit non-zero on timeout.
        #[arg(long)]
        expect: Option<String>,
        /// Timeout in seconds for --expect (default 5).
        #[arg(long)]
        timeout: Option<f64>,
    },
    /// Block until a regex appears in the stream (no send).
    Wait {
        pattern: String,
        /// Timeout in seconds (default 30).
        #[arg(long)]
        timeout: Option<f64>,
    },
    /// Daemon health + port state.
    Status,
    /// Print the path to the forensic log file.
    Log,
    /// Launch the daemon detached. Extra args are forwarded to uartd.
    Start {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Ask the daemon to shut down.
    Stop,
}

fn socket_path(cli: &Cli) -> PathBuf {
    cli.socket
        .clone()
        .unwrap_or_else(|| PathBuf::from(DEFAULT_SOCKET))
}

fn print_json<T: serde::Serialize>(v: &T) {
    println!("{}", serde_json::to_string(v).unwrap());
}

fn main() {
    let cli = Cli::parse();
    let code = run(&cli);
    std::process::exit(code);
}

fn run(cli: &Cli) -> i32 {
    let sock = socket_path(cli);

    if let Cmd::Start { args } = &cli.cmd {
        return start_daemon(cli, &sock, args);
    }

    let request = match &cli.cmd {
        Cmd::Read => Request::Read,
        Cmd::Peek => Request::Peek,
        Cmd::Send {
            text,
            no_newline,
            expect,
            timeout,
        } => Request::Send {
            text: text.clone(),
            no_newline: *no_newline,
            expect: expect.clone(),
            timeout_ms: Some(secs_to_ms(timeout.unwrap_or(5.0))),
        },
        Cmd::Wait { pattern, timeout } => Request::Wait {
            pattern: pattern.clone(),
            timeout_ms: secs_to_ms(timeout.unwrap_or(30.0)),
        },
        Cmd::Status => Request::Status,
        Cmd::Log => Request::Log,
        Cmd::Stop => Request::Stop,
        Cmd::Start { .. } => unreachable!(),
    };

    let resp = match send_request(&sock, &request) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("uart: {e}");
            return 2;
        }
    };

    render(cli, &resp)
}

fn secs_to_ms(s: f64) -> u64 {
    (s * 1000.0).max(0.0) as u64
}

fn render(cli: &Cli, resp: &Response) -> i32 {
    match resp {
        Response::Read { text, .. } => {
            if cli.json {
                print_json(resp);
            } else {
                print!("{text}");
            }
            0
        }
        Response::Match {
            context, timed_out, ..
        } => {
            if cli.json {
                print_json(resp);
            } else {
                print!("{context}");
            }
            if *timed_out { 1 } else { 0 }
        }
        Response::Status { connected, .. } => {
            if cli.json {
                print_json(resp);
            } else if let Response::Status {
                port,
                baud,
                buffer_bytes,
                uptime_s,
                log_path,
                ..
            } = resp
            {
                println!(
                    "running  port={port} baud={baud} connected={connected} buffer={buffer_bytes}B uptime={uptime_s}s log={log_path}"
                );
            }
            0
        }
        Response::Log { path } => {
            if cli.json {
                print_json(resp);
            } else {
                println!("{path}");
            }
            0
        }
        Response::Ok => {
            if cli.json {
                print_json(resp);
            }
            0
        }
        Response::Error { message } => {
            if cli.json {
                print_json(resp);
            } else {
                eprintln!("uart: {message}");
            }
            3
        }
    }
}

/// Spawn `uartd` detached (new session), then poll until it is up.
fn start_daemon(cli: &Cli, sock: &std::path::Path, extra: &[String]) -> i32 {
    // already running?
    if send_request(sock, &Request::Status).is_ok() {
        eprintln!("uart: daemon already running at {}", sock.display());
        return 0;
    }

    let uartd = match std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("uartd")))
    {
        Some(p) if p.exists() => p,
        _ => PathBuf::from("uartd"), // fall back to PATH
    };

    let mut cmd = Command::new(uartd);
    if let Some(s) = &cli.socket {
        cmd.arg("--socket").arg(s);
    }
    cmd.args(extra);
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // detach into its own session so it survives the CLI exiting
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }

    match cmd.spawn() {
        Ok(_) => {}
        Err(e) => {
            eprintln!("uart: failed to launch uartd: {e}");
            return 2;
        }
    }

    // poll until the socket answers
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if send_request(sock, &Request::Status).is_ok() {
            eprintln!("uart: daemon started at {}", sock.display());
            return 0;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    eprintln!("uart: daemon did not come up within 5s");
    2
}
