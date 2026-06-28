// SPDX-License-Identifier: Apache-2.0
//
// uartd: buffered UART console daemon + CLI for AI-driven serial control.
//
// The library half holds the pure, hardware-free logic (ring buffer, line framing, expect
// matching, send pacing, wire protocol, config) so it can be unit-tested without a serial
// port. The daemon module wires these onto a real port + Unix socket.

// Modules are added per TDD milestone (see plan.md).
pub mod buffer;
pub mod clock;
pub mod expect;
pub mod lines;
pub mod pacer;
pub mod proto;
