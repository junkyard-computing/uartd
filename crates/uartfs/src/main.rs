// SPDX-License-Identifier: Apache-2.0
//
// uartfs technician CLI: reliable, delta-aware file/flash transport to a phone over the UART
// console owned by uartd. Scriptable (stable output, meaningful exit codes) so an automated
// build -> push -> reflash -> reboot -> verify loop can drive it.
//
// Exit codes: 0 ok · 1 device command returned non-zero (run) · 2 link/daemon error ·
//             3 transfer/verify failure.

use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use clap::{Parser, Subcommand};

use uartfs::client_link::ClientLink;
use uartfs::commands::{self, DEFAULT_DEVICE_DIR};
use uartfs::frame::{Dir, Frame};
use uartfs::transport::{Link, Transport};

#[derive(Parser)]
#[command(
    name = "uartfs",
    about = "Reliable delta-flash transport over a UART console (rides uartd)"
)]
struct Cli {
    /// uartd control socket.
    #[arg(long, global = true, default_value = "/tmp/uartd.sock")]
    socket: PathBuf,
    /// Raw bytes per chunk (base64 on the wire).
    #[arg(long, global = true, default_value_t = 1024)]
    chunk: usize,
    /// Device-side scratch dir (the agent's UARTFS_DIR).
    #[arg(long, global = true, default_value = DEFAULT_DEVICE_DIR)]
    device_dir: String,
    /// Prefix device-side privileged actions with sudo.
    #[arg(long, global = true)]
    sudo: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Handshake with the agent.
    Ping,
    /// Run a command on the device; stream stdout/stderr; propagate its exit code.
    Run {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        command: Vec<String>,
    },
    /// Copy a local file to the device (verified + read-back-checked).
    Push { local: PathBuf, remote: String },
    /// Read a remote file or `partlabel:offset:len` slice into a local file (`-` for stdout).
    Pull { spec: String, local: String },
    /// Flash an image to a partition by label (delivered, dd'd, read-back-verified).
    /// With --base, ships only a zstd delta of (base -> image) and reconstructs on-device.
    Flash {
        image: PathBuf,
        partlabel: String,
        /// Local copy of what's currently on the partition; enables delta flashing.
        #[arg(long)]
        base: Option<PathBuf>,
        #[arg(long)]
        dry_run: bool,
        /// Treat <partlabel> as a full device/file path instead of by-partlabel.
        #[arg(long)]
        raw_target: bool,
    },
    /// Install a kernel module, then depmod (default) or insmod it.
    InstallModule {
        local: PathBuf,
        #[arg(long)]
        insmod: bool,
    },
    /// Install + launch the phone-side agent over the bare console.
    Bootstrap,
    /// Tell the agent to exit (return the console to the shell).
    Quit,
}

fn now_id() -> u32 {
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(1);
    (n % 90000) + 1
}

fn transport(cli: &Cli) -> Transport<ClientLink> {
    Transport::new(ClientLink::new(cli.socket.clone()))
}

fn main() {
    let cli = Cli::parse();
    std::process::exit(run(&cli));
}

fn run(cli: &Cli) -> i32 {
    match &cli.cmd {
        Cmd::Ping => {
            let mut t = transport(cli);
            match t.ping() {
                Ok(v) => {
                    println!("agent ready (v{v})");
                    0
                }
                Err(e) => fail_link(e),
            }
        }
        Cmd::Run { command } => {
            let mut t = transport(cli);
            let cmd = command.join(" ");
            match commands::run(&mut t, &cmd) {
                Ok(r) => {
                    let _ = std::io::stdout().write_all(&r.stdout);
                    let _ = std::io::stderr().write_all(&r.stderr);
                    r.code
                }
                Err(e) => fail_link(e),
            }
        }
        Cmd::Push { local, remote } => {
            let data = match std::fs::read(local) {
                Ok(d) => d,
                Err(e) => return fail_io(local, e),
            };
            let mut t = transport(cli);
            match commands::push(
                &mut t,
                &data,
                remote,
                cli.sudo,
                cli.chunk,
                now_id(),
                &cli.device_dir,
            ) {
                Ok(sha) => {
                    eprintln!(
                        "pushed {} bytes to {remote} (sha256 {sha}) — verified",
                        data.len()
                    );
                    0
                }
                Err(e) => fail_transfer(e),
            }
        }
        Cmd::Pull { spec, local } => {
            let mut t = transport(cli);
            match commands::pull(&mut t, spec, cli.sudo) {
                Ok(bytes) => {
                    if local == "-" {
                        let _ = std::io::stdout().write_all(&bytes);
                    } else if let Err(e) = std::fs::write(local, &bytes) {
                        return fail_io(&PathBuf::from(local), e);
                    }
                    eprintln!("pulled {} bytes from {spec}", bytes.len());
                    0
                }
                Err(e) => fail_transfer(e),
            }
        }
        Cmd::Flash {
            image,
            partlabel,
            base,
            dry_run,
            raw_target,
        } => {
            let data = match std::fs::read(image) {
                Ok(d) => d,
                Err(e) => return fail_io(image, e),
            };
            let target = if *raw_target {
                partlabel.clone()
            } else {
                format!("/dev/disk/by-partlabel/{partlabel}")
            };
            let mut t = transport(cli);
            // delta path when a base is supplied (and not a dry run)
            if let Some(base_path) = base {
                if *dry_run {
                    eprintln!(
                        "[dry-run] would delta-flash {} bytes to {target} against base {}",
                        data.len(),
                        base_path.display()
                    );
                    return 0;
                }
                return match commands::flash_delta(
                    &mut t,
                    base_path,
                    image,
                    &target,
                    cli.sudo,
                    cli.chunk,
                    now_id(),
                    &cli.device_dir,
                ) {
                    Ok(rep) => {
                        eprintln!(
                            "delta-flashed {} bytes to {} (sha256 {}) — read-back verified",
                            rep.bytes, rep.target, rep.sha256
                        );
                        0
                    }
                    Err(e) => fail_transfer(e),
                };
            }
            match commands::flash(
                &mut t,
                &data,
                &target,
                cli.sudo,
                cli.chunk,
                now_id(),
                &cli.device_dir,
                *dry_run,
            ) {
                Ok(rep) if rep.written => {
                    eprintln!(
                        "flashed {} bytes to {} (sha256 {}) — read-back verified",
                        rep.bytes, rep.target, rep.sha256
                    );
                    0
                }
                Ok(rep) => {
                    eprintln!(
                        "[dry-run] would flash {} bytes to {} (sha256 {})",
                        rep.bytes, rep.target, rep.sha256
                    );
                    0
                }
                Err(e) => fail_transfer(e),
            }
        }
        Cmd::InstallModule { local, insmod } => {
            let data = match std::fs::read(local) {
                Ok(d) => d,
                Err(e) => return fail_io(local, e),
            };
            let name = local
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "module.ko".into());
            let mut t = transport(cli);
            match commands::install_module(
                &mut t,
                &data,
                &name,
                cli.sudo,
                cli.chunk,
                now_id(),
                &cli.device_dir,
                !insmod,
            ) {
                Ok(()) => {
                    eprintln!("installed module {name}");
                    0
                }
                Err(e) => fail_transfer(e),
            }
        }
        Cmd::Bootstrap => bootstrap(cli),
        Cmd::Quit => {
            let mut link = ClientLink::new(cli.socket.clone());
            let f = Frame::new(Dir::ToDevice, "QUIT", vec![]);
            match link.send_line(&f.encode()) {
                Ok(()) => {
                    eprintln!("sent QUIT");
                    0
                }
                Err(e) => {
                    eprintln!("uartfs: {e}");
                    2
                }
            }
        }
    }
}

/// Install the agent script over the bare console (no agent running yet), then launch it.
fn bootstrap(cli: &Cli) -> i32 {
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD as B64;

    let script = include_str!("../agent/uartfs-agent.sh");
    let b64 = B64.encode(script.as_bytes());
    let mut link = ClientLink::new(cli.socket.clone());

    let remote_b64 = "/tmp/uartfs-agent.b64";
    let remote_sh = "/tmp/uartfs-agent.sh";

    let mut send = |line: String| -> i32 {
        match link.send_line(&line) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("uartfs: bootstrap send failed: {e}");
                2
            }
        }
    };

    // These are raw shell commands to the login shell, not frames.
    if send(format!(": > {remote_b64}")) != 0 {
        return 2;
    }
    for chunk in b64.as_bytes().chunks(160) {
        let piece = std::str::from_utf8(chunk).unwrap();
        if send(format!("printf %s '{piece}' >> {remote_b64}")) != 0 {
            return 2;
        }
    }
    if send(format!("base64 -d {remote_b64} > {remote_sh}")) != 0 {
        return 2;
    }
    // launch foreground so it owns the console; returns to the shell on QUIT/EOF
    if send(format!("sh {remote_sh}")) != 0 {
        return 2;
    }

    // confirm it came up
    let mut t = transport(cli);
    match t.ping() {
        Ok(v) => {
            eprintln!("agent bootstrapped and ready (v{v})");
            0
        }
        Err(e) => {
            eprintln!("uartfs: agent did not come up after bootstrap: {e}");
            3
        }
    }
}

fn fail_link(e: uartfs::transport::TransportError) -> i32 {
    eprintln!("uartfs: {e}");
    2
}
fn fail_transfer(e: uartfs::transport::TransportError) -> i32 {
    eprintln!("uartfs: {e}");
    3
}
fn fail_io(path: &std::path::Path, e: std::io::Error) -> i32 {
    eprintln!("uartfs: {}: {e}", path.display());
    2
}
