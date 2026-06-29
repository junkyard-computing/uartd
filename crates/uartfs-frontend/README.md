# uartfs-frontend

The on-device, **pty-owning console front-end** (tier 2) — the reliable serial channel for
mainline felix. It owns the serial line and frames + checksums *every* byte on it (login,
commands, output, interactive sessions), so the lossy-UART problem is retired once at the
transport layer instead of being patched per-command.

It speaks the same framed, checksummed uartfs protocol as the host `Transport`/`commands`, so
those drive it unchanged — plus a new **interactive attach** mode the shell agent can't offer.

## Why a compiled binary (not the shell agent)

`forkpty`, raw-pty termios, a poll event loop, and a streaming interactive bridge aren't
realistically pure shell. So the front-end is a compiled `aarch64-musl` binary (cross-built like
the pixel-* tools), pushed and launched by the agentless floor. The shell agent stays for the
coreutils-only reach; the front-end subsumes its `run`/transfer role once installed and adds
attach.

## Two service modes over one framed channel

1. **Exec / transfer** — the uartfs protocol (`PING`/`OPEN`/`DATA`/`CLOSE`/`STAT`/`EXEC`). Receive
   a framed, checksum-verified command or blob; run/store it; frame back stdout/stderr/exit-code
   or `DONE`. Identical contract to the shell agent (so `uartfs push/pull/flash/run` work over it).
2. **Interactive attach** (new) — `ATTACH` forkpty's a login shell on its own pty (set `-echo`
   raw, so input/output are cleanly separated and framed by *us*, not guessed from echo).
   Host keystrokes → `ATTACHIN` → child pty; child output → `ATTACHOUT` → host. `WINSIZE` resizes
   (`TIOCSWINSZ`); `DETACH` (or shell exit → `AEND`) ends it. Both directions checksummed, so a
   `sudo` password prompt / `vim` / `menuconfig` / a live console finally work reliably over UART.
   Drive it from the host with `uartfs attach` (Ctrl-] to detach).

## Lifecycle & the getty fallback

It IS the console service (see [`deploy/`](deploy/)): `uartfs-console@ttySAC0.service` takes the
line (`Conflicts=serial-getty@`), respawns on transient failure, and — crucially — on permanent
failure hands the line back to `serial-getty@` (`OnFailure=`). That getty is the substrate the
**agentless floor** (`uart run`, see `uartd-reliable-run`) needs to reach the device and
reinstall/relaunch the front-end. So:

- front-end up → use it for everything (framed, reliable, interactive).
- front-end down (fresh boot, pre-init, crash) → drop to the floor over the bare getty/shell;
  the floor brings the front-end back.

First install is bootstrapped over the floor: `uart run` pushes the binary + runs
`deploy/install.sh`.

## Raw vs framed: a client-side boundary (no uartd change)

uartd stays a dumb byte mover (`send`/`read`); framing lives in the clients. So the "mode" is a
client choice, not a uartd code path:

- **Boot / pre-front-end:** raw — `uart wait 'login:'`, kernel `printk`, and the agentless
  `uart run` floor all operate on the raw stream.
- **Front-end up:** framed — `uartfs run/push/flash/attach` parse their frames out of the same
  raw stream (resyncing past any stray `printk`).

Both ride the one uartd-owned port; nothing in uartd needs a mode switch.

## Build

Native (for tests): `cargo build -p uartfs-frontend`. Device (static aarch64):
`nix build .#uartfs-frontend-aarch64` → a static musl binary to scp to `/usr/local/bin`.

## Testing

```sh
cargo test -p uartfs-frontend
```
Spawns the real compiled binary on a pty and drives it with the host `Transport`/`commands`
(handshake, exec, blob, push) and with raw frames for **interactive attach** (types into a live
forkpty'd bash and reads its pty output back) — no hardware needed.

Licensed Apache-2.0.
