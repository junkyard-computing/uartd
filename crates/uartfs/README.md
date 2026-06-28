# uartfs

A reliable, delta-aware **file/flash transport over a lossy UART console** — for iterating on a
mainline kernel on a Pixel (felix/gs201) that has *no working network*, where the serial line
is the only persistent channel and fastboot would mean a human swapping the USB-C cable every
cycle.

It rides the console owned by [`uartd`](../../README.md): the host speaks a framed, ACK'd,
sha256-verified protocol to a small dependency-light agent on the phone. Per-iteration payloads
are **deltas** (a new `vendor_boot` is ~99% identical to the last — only the dtb changes), so a
reflash moves *KB, not MB*.

## Why it's a separate tool from uartd (but the same repo)

uartd owns the port and is the single console transport. uartfs is a **consumer** of that
channel — but unlike `benchctl` (which only uses uartd's stable `uart` CLI), uartfs couples to
the wire framing and co-evolves with it, so it lives here as a workspace crate sharing
`uart-core`'s socket client.

## How it works

- **Framing.** Every message is one ASCII line with a direction sentinel (`UFS>` host→device,
  `UFS<` device→host). The reader resyncs to the last sentinel on a line (stripping printk that
  shares it) and ignores everything that isn't a frame, so frames survive console noise, shell
  echo, and dropped characters. Distinct sentinels mean each side ignores the echo of its own
  traffic.
- **Reliable delivery.** A blob is split into base64 chunks, each tagged with a sha256 prefix.
  Transfer is **stop-and-wait ARQ**: every chunk is resent until ACK'd (or NAK'd → resend), and
  nothing is "delivered" until the device returns a `DONE` whose **full sha256 matches** what was
  sent. Never trust a byte you didn't checksum.
- **Apply = exec.** Once a verified blob lands in a device temp file, applying it (dd, insmod,
  decompress, patch) is an ordinary verified `exec`. `exec` also powers `pull` (the command's
  stdout *is* the bytes) and `run` (probe the device, get stdout/stderr/exit-code back).
- **Delta.** `flash --base <current>` ships only a `zstd --patch-from` patch of (base → new) and
  reconstructs on-device against the live partition content, after checking the device's base
  sha matches. Refuses if it doesn't (reconstruction would be garbage).
- **Safety.** Flashes are read-back-verified: after `dd`, the written region's sha256 must equal
  the image's, or it's reported as failed — it never claims success on a corrupt write.

## Phone-side agent

[`agent/uartfs-agent.sh`](agent/uartfs-agent.sh) — a POSIX-shell receiver needing only
`base64`, `sha256sum`, `dd`, `wc`, `tr`, `cat`. It is **bootstrappable over the bare console**:
`uartfs bootstrap` pastes it in (base64) and launches it, no prior agent required. Heavier tools
(`zstd` for delta) are detected; install/push them if missing.

## Commands

```sh
uartfs --socket /tmp/uartd.sock bootstrap        # install + launch the agent over the console
uartfs ping                                       # handshake
uartfs run dmesg | grep -i edgetpu                # run a probe, get stdout/stderr/exit-code
uartfs push ./initramfs /tmp/initramfs            # verified file copy + read-back check
uartfs pull /tmp/log ./log                        # read a file back (or partlabel:off:len)
uartfs flash boot.img boot_a                      # full flash: deliver, dd, read-back verify
uartfs flash vendor_boot.img vendor_boot_a \
        --base ./prev-vendor_boot.img             # DELTA flash: ship only the patch
uartfs install-module felix_drv.ko --insmod       # push to /lib/modules/$(uname -r)/extra
uartfs quit                                        # return the console to the shell
```

Global: `--socket` (uartd socket), `--chunk` (bytes/chunk), `--sudo` (privileged device
actions), `--device-dir` (agent scratch dir). Exit codes: `0` ok · `1` device command non-zero
(`run`) · `2` link/daemon error · `3` transfer/verify failure.

## The autonomous loop it enables

```
build new boot/vendor_boot  ->  uartfs flash --base <prev>  (only the delta crosses the wire)
                            ->  reconstruct + dd + read-back verify on-device
                            ->  uartfs run reboot
                            ->  uartfs run <probe>           (read the result back)
```
Zero fastboot, zero cable swaps, low-KB per iteration.

## Validated on hardware

On a Pixel Fold (felix) over `/dev/ttyUSB0` @ 115200 8N1: bootstrap → ping → `run` (with
exit-code propagation) → `push`/`pull` (byte-identical roundtrip) → **delta-flash** (a 200-byte
change in a 30 KB file crossed as a 31-byte patch, reconstructed + read-back-verified on-device).

## Testing

```sh
cargo test -p uartfs
```
The protocol/transport/delta logic is unit-tested; the full host↔agent path (handshake, blob
transfer, exec, push, flash, delta) runs against the **real shell agent over a pty** — no
hardware needed. (Delta tests require `zstd` on `PATH`.)

Licensed Apache-2.0.
