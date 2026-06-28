# uartd — build plan

A buffered UART console daemon + CLI that turns a live serial stream into a **poll-able
request/response resource** for a turn-based AI agent. The agent can't hold a port open
between turns, so `uartd` owns the port and captures continuously; `uart` is the per-turn
client.

Full spec: `../junkyard-boot-img/prompts/uartd.md`. This plan commits to the language,
architecture, the resolved design decisions, and a **TDD milestone sequence**.

---

## Decisions (locked)

- **Language: Rust.** uartd owns a hardware resource and must not crash for hours — it joins
  the repo's compiled-tool family (pixel-bootctl, pixel-ota, finch): edition 2024, `clap`
  derive, minimal deps, a per-tool `flake.nix`. Adapted to a **native host build** (x86
  NixOS), *not* the on-device aarch64-musl cross those tools use — uartd runs host-side.
- **benchctl stays Python** (orchestration glue). The uartd↔benchctl boundary is the `uart`
  CLI contract (`read|send|wait|log` + `--json`); language-agnostic, zero integration cost.
- **Daemon core is std threads + `mpsc`, not tokio.** One serial port + one Unix socket does
  not need an async runtime; keep the dependency list as lean as the sibling tools'.
- **All work is TDD**: every milestone is red→green→refactor. Core logic is pure and unit-
  tested with no hardware; end-to-end tests run over a **pty (openpty)** so CI needs no
  device and no socat.

---

## Resolved design questions

These were open in the spec; defaults chosen for predictable machine-caller behavior:

1. **Drain buffer is bounded.** A ring buffer (default 1 MiB, configurable). On overflow,
   drop oldest and inject a `[uartd: dropped N bytes]` marker into the stream + log, so loss
   is never silent. The append-only log is the unbounded forensic record.
2. **Single read cursor, single consumer assumed.** `read` returns bytes since the cursor and
   advances it; `peek` returns the same without advancing. Documented as single-agent.
3. **`wait`/`--expect` match a rolling window of data arriving *from when the call starts*;
   they do NOT consume the drain buffer** (orthogonal to `read`). `--expect` returns
   everything received between the send completing and the match. Regex matches across line
   boundaries against accumulated text.
4. **`--expect`/`wait` timeout clock starts after the last byte is written** (so send pacing
   never eats into the response budget). Documented.
5. **Auto-login re-arms on every reconnect/reboot** — a rebooting device re-shows `login:`
   constantly, so the watcher is always live when opt-in is configured.
6. **Concurrency:** one handler thread per CLI connection. A blocking `wait`/`--expect` on one
   connection never blocks `status`/`read` on another. The single port writer is serialized
   behind a mutex so paced sends don't interleave.

---

## Architecture

Two binaries from one crate (shared `lib`):

- **`uartd`** — daemon. Threads:
  - *Reader*: exclusively opens + configures the port (termios 8N1/baud), reads bytes, frames
    lines, timestamps (monotonic + wall-clock), appends to the log, pushes to the ring buffer,
    and notifies waiters. On error/EOF it enters the **reconnect loop** (backoff reopen,
    logs a reconnect marker, resumes) — never panics on a port drop.
  - *Socket server*: `accept()` loop on the Unix socket; spawns a handler thread per
    connection.
  - *Auto-login* (opt-in): a waiter watching for `login:`/`Password:` that injects creds.
- **`uart`** — CLI. Thin client: one invocation = one connection = one request. Stable output,
  meaningful exit codes, global `--json`.
- **Wire protocol:** newline-delimited JSON request/response over the Unix socket (`serde`).
  Blocking requests (`wait`, `send --expect`) hold their connection open until match/timeout.

### Crate layout
```
Cargo.toml            # [lib] + [[bin]] uartd + [[bin]] uart
src/lib.rs
src/buffer.rs         # RingBuffer + drain cursor          (pure, unit-tested)
src/lines.rs          # line framing + timestamping         (pure, unit-tested)
src/expect.rs         # incremental regex matcher           (pure, unit-tested)
src/pacer.rs          # split send into paced write chunks  (pure, unit-tested)
src/proto.rs          # serde request/response + --json     (pure, unit-tested)
src/config.rs         # file + env + flag layering          (pure, unit-tested)
src/daemon.rs         # reader/server/reconnect wiring       (pty integration tests)
src/bin/uartd.rs
src/bin/uart.rs
tests/                # end-to-end over openpty
```
Deps: `clap` (derive), `serialport`, `regex`, `serde` + `serde_json`, `anyhow`, `libc`.
Dev-deps: `nix` (openpty for tests).

---

## TDD milestones

Each: write failing test(s) → minimal code → refactor. Pure units first, then integration.

| # | Milestone | Test-first focus | Spec acceptance |
|---|---|---|---|
| 0 | Scaffold | `cargo test` runs; CI green on empty lib | — |
| 1 | `RingBuffer` + cursor | read drains & advances; peek doesn't; bound→drop-oldest emits `dropped N` marker | AC2 |
| 2 | Line framing + timestamps | partial lines buffered; each emitted line carries monotonic+wall ts | AC2 |
| 3 | `ExpectMatcher` | regex matches across chunk boundaries; returns context; reports no-match | AC3/5 |
| 4 | `Pacer` | splits into one-line writes w/ inter-line (+optional inter-char) delay; newline append; `--no-newline` | AC3 |
| 5 | Wire protocol | serde round-trip for every request/response; `--json` shape stable | AC1–5 |
| 6 | Config layering | defaults < file < env < flag; `uartd --port X` works | — |
| 7 | Daemon over pty: capture | start vs pty; `status` connected; master writes → `read` w/ ts, clears; 2nd read empty; log retains | AC1, AC2 |
| 8 | `send` over pty | bytes arrive paced at master; 20-line block → **zero drops** (all lines received) | AC3 |
| 9 | `wait` / `send --expect` | blocks until regex; returns context; **non-zero exit on timeout** | AC3, AC5 |
| 10 | Reconnect | close/reopen pty mid-run → daemon logs marker, resumes, no crash | AC4 |
| 11 | Auto-login | state machine unit-tested; pty integration drives `login:`→user→`Password:`→pass; re-arms after reconnect | AC5 |
| 12 | Packaging | `flake.nix` native pkg (uartd+uart) + dev shell; `nix flake check` runs `cargo test` | — |
| 13 | Docs | README (rationale + usage), sample config | deliverable |

---

## Deliverables (from spec)

Working `uartd` + `uart`; README with rationale + usage; sample config; `flake.nix`
(runnable packages + dev shell); pty-based test suite that runs hardware-free in CI.

---

## Out of scope / deferred

- benchctl (separate Python tool; consumes `uart --json`).
- Multi-consumer / multiple independent read cursors (single-agent assumed).
- Image transfer over UART (spec: console/control only, ~11 KB/s).
