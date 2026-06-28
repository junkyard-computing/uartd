// SPDX-License-Identifier: Apache-2.0
//
// uartd daemon entry point. Resolves config (defaults < file < env < flags), opens the port,
// and serves the CLI until SIGINT/SIGTERM or a `stop` request. Runs in the foreground; use
// `uart start` to launch it detached.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use clap::Parser;

use uart_core::clock::SystemClock;
use uart_core::config::{Parity, PartialConfig, resolve};
use uart_core::daemon::Daemon;

#[derive(Parser)]
#[command(
    name = "uartd",
    about = "Buffered UART console daemon for AI-driven serial control"
)]
struct Args {
    /// Serial port path (e.g. /dev/ttyUSB0). Required unless set in config/env.
    #[arg(long)]
    port: Option<String>,
    /// Baud rate (default 115200).
    #[arg(long)]
    baud: Option<u32>,
    #[arg(long)]
    data_bits: Option<u8>,
    /// Parity: n, e, or o (default n).
    #[arg(long)]
    parity: Option<String>,
    #[arg(long)]
    stop_bits: Option<u8>,
    /// Config file (TOML). Also honored via UARTD_CONFIG.
    #[arg(long)]
    config: Option<PathBuf>,
    /// Control socket path (default /tmp/uartd.sock).
    #[arg(long)]
    socket: Option<PathBuf>,
    /// Directory for the forensic log (default /tmp/uartd).
    #[arg(long)]
    log_dir: Option<PathBuf>,
    /// Max bytes held in the drain buffer before oldest is dropped.
    #[arg(long)]
    buffer_cap: Option<usize>,
    /// Inter-line pacing delay in ms (default 20).
    #[arg(long)]
    inter_line_ms: Option<u64>,
    /// Inter-character pacing delay in ms (default 0).
    #[arg(long)]
    inter_char_ms: Option<u64>,
    /// Reconnect backoff in ms (default 500).
    #[arg(long)]
    reconnect_ms: Option<u64>,
    /// Auto-login username (opt-in; both user and pass required).
    #[arg(long)]
    login_user: Option<String>,
    /// Auto-login password.
    #[arg(long)]
    login_pass: Option<String>,
}

static TERM: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_sig: libc::c_int) {
    TERM.store(true, Ordering::SeqCst);
}

fn flags_from(args: &Args) -> Result<PartialConfig, String> {
    let parity = match args.parity.as_deref() {
        None => None,
        Some(s) => Some(match s.to_ascii_uppercase().as_str() {
            "N" | "NONE" => Parity::N,
            "E" | "EVEN" => Parity::E,
            "O" | "ODD" => Parity::O,
            other => return Err(format!("invalid parity {other:?} (use n, e, or o)")),
        }),
    };
    Ok(PartialConfig {
        port: args.port.clone(),
        baud: args.baud,
        data_bits: args.data_bits,
        parity,
        stop_bits: args.stop_bits,
        socket_path: args.socket.clone(),
        log_dir: args.log_dir.clone(),
        buffer_cap: args.buffer_cap,
        inter_line_ms: args.inter_line_ms,
        inter_char_ms: args.inter_char_ms,
        reconnect_ms: args.reconnect_ms,
        login_user: args.login_user.clone(),
        login_pass: args.login_pass.clone(),
    })
}

fn run() -> Result<(), String> {
    let args = Args::parse();

    let file = match args
        .config
        .clone()
        .or_else(|| std::env::var_os("UARTD_CONFIG").map(PathBuf::from))
    {
        Some(path) => {
            let body = std::fs::read_to_string(&path)
                .map_err(|e| format!("reading config {}: {e}", path.display()))?;
            PartialConfig::from_toml_str(&body).map_err(|e| e.to_string())?
        }
        None => PartialConfig::default(),
    };
    let env = PartialConfig::from_env(|k| std::env::var(k).ok());
    let flags = flags_from(&args)?;

    let cfg = resolve(file, env, flags).map_err(|e| e.to_string())?;

    let daemon = Daemon::start(cfg, Arc::new(SystemClock::new()))
        .map_err(|e| format!("starting daemon: {e}"))?;

    eprintln!(
        "uartd: listening on {}, logging to {}",
        daemon.socket_path().display(),
        daemon.log_path().display()
    );

    // Install signal handlers, then wait for stop (signal or `uart stop`).
    unsafe {
        libc::signal(libc::SIGINT, on_signal as *const () as usize);
        libc::signal(libc::SIGTERM, on_signal as *const () as usize);
    }
    while !daemon.is_shutdown() && !TERM.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(50));
    }

    eprintln!("uartd: shutting down");
    daemon.shutdown();
    Ok(())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("uartd: {e}");
        std::process::exit(1);
    }
}
