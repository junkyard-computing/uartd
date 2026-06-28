// SPDX-License-Identifier: Apache-2.0
//
// Flow-control-safe send pacing. The target UART has no RTS/CTS and silently drops bytes if
// you blast at it, so we never write a multi-line block at once: we emit a plan of small
// writes with a delay after each. The daemon executes the plan (write, flush, sleep); keeping
// the planning pure makes the pacing deterministically testable without sleeping.
//
//   * inter-line delay: applied after every '\n' — the main pacing knob ("one line at a time").
//   * inter-char delay: optional; when > 0, each byte of a line is its own write. Off by
//     default (whole line in one write, paced between lines).

use std::time::Duration;

/// How to pace a send.
#[derive(Debug, Clone)]
pub struct PacerConfig {
    /// Ensure the payload ends with a newline (the Enter key). `--no-newline` sets this false.
    pub newline_append: bool,
    pub inter_line: Duration,
    pub inter_char: Duration,
}

impl Default for PacerConfig {
    fn default() -> Self {
        PacerConfig {
            newline_append: true,
            inter_line: Duration::from_millis(20),
            inter_char: Duration::ZERO,
        }
    }
}

/// One write the daemon performs, then sleeps `delay_after`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteStep {
    pub bytes: Vec<u8>,
    pub delay_after: Duration,
}

/// Build the paced write plan for `input` under `cfg`.
pub fn plan(input: &[u8], cfg: &PacerConfig) -> Vec<WriteStep> {
    // Normalize the payload: optionally guarantee a single trailing newline.
    let mut payload = input.to_vec();
    if cfg.newline_append && payload.last() != Some(&b'\n') {
        payload.push(b'\n');
    }

    let mut steps = Vec::new();
    let mut line: Vec<u8> = Vec::new();
    for &b in &payload {
        if b == b'\n' {
            // flush the body of this line, then the newline paced by inter_line
            emit_body(&mut steps, &line, cfg);
            line.clear();
            steps.push(WriteStep {
                bytes: vec![b'\n'],
                delay_after: cfg.inter_line,
            });
        } else {
            line.push(b);
        }
    }
    // trailing body with no newline (e.g. --no-newline)
    emit_body(&mut steps, &line, cfg);
    steps
}

fn emit_body(steps: &mut Vec<WriteStep>, body: &[u8], cfg: &PacerConfig) {
    if body.is_empty() {
        return;
    }
    if cfg.inter_char > Duration::ZERO {
        for &b in body {
            steps.push(WriteStep {
                bytes: vec![b],
                delay_after: cfg.inter_char,
            });
        }
    } else {
        steps.push(WriteStep {
            bytes: body.to_vec(),
            delay_after: Duration::ZERO,
        });
    }
}

/// The concatenated bytes a plan would write — exactly what reaches the wire.
pub fn payload_of(steps: &[WriteStep]) -> Vec<u8> {
    steps.iter().flat_map(|s| s.bytes.clone()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(append: bool, line_ms: u64, char_ms: u64) -> PacerConfig {
        PacerConfig {
            newline_append: append,
            inter_line: Duration::from_millis(line_ms),
            inter_char: Duration::from_millis(char_ms),
        }
    }

    #[test]
    fn simple_line_gets_newline_appended() {
        let steps = plan(b"echo hi", &cfg(true, 20, 0));
        assert_eq!(payload_of(&steps), b"echo hi\n");
        // body then newline
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[1].bytes, b"\n");
        assert_eq!(steps[1].delay_after, Duration::from_millis(20));
    }

    #[test]
    fn no_newline_suppresses_append() {
        let steps = plan(b"abc", &cfg(false, 20, 0));
        assert_eq!(payload_of(&steps), b"abc");
        assert_eq!(steps.len(), 1);
    }

    #[test]
    fn does_not_double_existing_newline() {
        let steps = plan(b"x\n", &cfg(true, 20, 0));
        assert_eq!(payload_of(&steps), b"x\n");
    }

    #[test]
    fn multiline_paced_per_newline() {
        let steps = plan(b"a\nb\nc", &cfg(true, 15, 0));
        assert_eq!(payload_of(&steps), b"a\nb\nc\n");
        let newline_steps: Vec<_> = steps.iter().filter(|s| s.bytes == b"\n").collect();
        assert_eq!(newline_steps.len(), 3);
        assert!(
            newline_steps
                .iter()
                .all(|s| s.delay_after == Duration::from_millis(15))
        );
    }

    #[test]
    fn inter_char_splits_each_byte() {
        let steps = plan(b"ab", &cfg(false, 20, 5));
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].bytes, b"a");
        assert_eq!(steps[0].delay_after, Duration::from_millis(5));
        assert_eq!(steps[1].bytes, b"b");
    }

    #[test]
    fn inter_char_uses_line_delay_for_newline() {
        let steps = plan(b"ab", &cfg(true, 30, 5));
        // a(5), b(5), \n(30)
        assert_eq!(steps.len(), 3);
        assert_eq!(steps[2].bytes, b"\n");
        assert_eq!(steps[2].delay_after, Duration::from_millis(30));
    }

    #[test]
    fn empty_input_with_append_is_bare_newline() {
        let steps = plan(b"", &cfg(true, 20, 0));
        assert_eq!(payload_of(&steps), b"\n");
    }

    #[test]
    fn empty_input_no_append_is_nothing() {
        let steps = plan(b"", &cfg(false, 20, 0));
        assert!(steps.is_empty());
    }
}
