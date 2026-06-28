# uartd

A buffered UART console daemon (`uartd`) + CLI (`uart`) for driving a serial console from an
**AI coding agent** — or any caller that works in discrete request/response turns.

## Why this exists

An agent that drives hardware over a UART (e.g. a Pixel phone running Linux at 115200 8N1 on
`/dev/ttyUSB0`) operates in **discrete turns**: between turns nothing of its own is listening,
so it can't hold the port open and watch a live stream. uartd turns the stream into a
**poll-able resource**: a daemon owns the port and captures continuously, and a thin CLI is
invoked per-turn to read what's new and to send input.

It is the inner-loop transport for autonomous kernel bring-up (see
`../junkyard-boot-img/prompts/`), but it's a general serial-console multiplexer.

## Architecture

- **`uartd`** opens and *exclusively owns* the port, configures it, and captures everything
  into two sinks (below). It serves the CLI over a Unix domain socket and survives across many
  CLI invocations. Concurrency is plain std threads — no async runtime.
- **`uart`** is a thin client: one invocation = one request. Stable, scriptable output and
  meaningful exit codes. The CLI never touches the device directly, so there's a single owner
  of the port (which also solves permissions — the daemon opens it once).

The `uart` command surface (`read`/`send`/`wait`/`log` + `--json`) is the language-agnostic
contract other tools (e.g. a Python `benchctl`) integrate against.

## Two sinks (don't conflate them)

1. **Drain buffer** behind `uart read` — the live "what's new since I last looked" feed.
   Lossy by design: `read` returns everything since the last `read` and clears it. Bounded; on
   overflow the oldest bytes are dropped and the next read is prefixed with
   `[uartd: dropped N bytes]` so loss is never silent.
2. **Forensic log** — an append-only file with per-line timestamps (monotonic + wall-clock)
   that is *never* cleared. `uart log` prints its path; `grep` it for the complete history.

## Install / build

NixOS (recommended):

```sh
nix build              # -> result/bin/uartd, result/bin/uart
nix run .#uartd -- --port /dev/ttyUSB0
nix develop            # dev shell with cargo/rustc/rustfmt/clippy
```

Or with cargo directly:

```sh
cargo build --release  # -> target/release/{uartd,uart}
```

## Usage

```sh
# Start the daemon (foreground), or detached via the CLI:
uartd --port /dev/ttyUSB0 &
uart start --port /dev/ttyUSB0      # launches uartd detached, waits until it's up

uart status                          # daemon/port health
uart read                            # new bytes since last read (and clear)
uart peek                            # same, without clearing
uart send "echo hello"               # paced send, newline appended
uart send "ls /" --expect '\$ ' --timeout 5   # send, block until prompt; exit 1 on timeout
uart wait 'login:' --timeout 30      # block until a regex appears (no send)
uart log                             # path to the forensic log
uart stop                            # shut the daemon down

uart --json read                     # structured output (per-line timestamps) for parsing
```

### Exit codes (`uart`)

| code | meaning |
|---|---|
| 0 | success / pattern matched |
| 1 | timeout (`--expect`/`wait` found nothing) |
| 2 | daemon not running / connection error |
| 3 | daemon returned an error (e.g. not connected, bad regex) |

## Hardware realities handled

- **No flow control.** The target drops bytes if blasted, so `send` is paced — one line at a
  time with an inter-line delay (and an optional inter-character delay). Sending a multi-line
  block never dumps it at once.
- **The port disappears/reappears** (USB re-enumeration, the phone's debug-console dropping,
  constant reboots). The daemon **auto-reconnects**: it keeps retrying the open, resumes
  capture seamlessly, and writes `==== uartd: reconnected ====` / `disconnected` markers to
  the log. It never crashes on a port drop.
- **Optional auto-login.** With `login_user`/`login_pass` configured, the daemon answers
  `login:`/`password:` prompts automatically and re-arms on every reconnect.

## Configuration

Precedence: **defaults < config file < environment < CLI flags.** See
[`uartd.toml.example`](uartd.toml.example). Environment variables mirror the fields
(`UARTD_PORT`, `UARTD_BAUD`, `UARTD_SOCKET`, `UARTD_LOG_DIR`, `UARTD_BUFFER_CAP`,
`UARTD_INTER_LINE_MS`, `UARTD_INTER_CHAR_MS`, `UARTD_RECONNECT_MS`, `UARTD_LOGIN_USER`,
`UARTD_LOGIN_PASS`, `UARTD_CONFIG`). Sensible defaults mean `uartd --port /dev/ttyUSB0` just
works (115200 8N1, socket `/tmp/uartd.sock`, log `/tmp/uartd/uartd.log`).

## Testing

```sh
cargo test
```

Pure logic (drain buffer, line framing, expect matching, send pacing, wire protocol, config)
is unit-tested. End-to-end behavior (capture, send, expect, reconnect, auto-login) runs
against a **pty** — no real hardware needed, so it works in CI.

## Design notes

- **Single read cursor, single consumer.** `read` advances it, `peek` doesn't; built for one
  agent driving the port.
- **`wait`/`--expect` observe a private rolling window** from when the call starts and do
  **not** consume the drain buffer — a subsequent `read` still sees the same bytes. The
  `--expect` timeout clock starts *after* the (paced) send completes.
- **Lossy UTF-8 decoding** — a booting kernel emits non-UTF-8 bytes; the daemon never panics
  on them.

Licensed Apache-2.0.
