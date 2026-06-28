// SPDX-License-Identifier: Apache-2.0
//
// uartfs: a reliable, delta-aware file/flash transport over a lossy, no-flow-control UART
// console — the channel a mainline-kernel Pixel exposes when it has no working network.
//
// It rides the console owned by `uartd`: the host speaks a framed, ACK'd, sha256-verified
// protocol to a small dependency-light agent on the phone. The library half is the protocol
// + transport (pure and testable against a simulated device); the binary is the technician
// CLI (push/pull/flash/patch/install-module/run/bootstrap).

pub mod chunk;
pub mod client_link;
pub mod commands;
pub mod frame;
pub mod hash;
pub mod msg;
pub mod transport;
