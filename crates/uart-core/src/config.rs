// SPDX-License-Identifier: Apache-2.0
//
// Daemon configuration with layered overrides: defaults < config file < environment < CLI
// flags. The layering is a pure function (`resolve`) over `PartialConfig` layers, so it is
// testable without touching the real environment or filesystem.

use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;

pub const DEFAULT_BAUD: u32 = 115200;
pub const DEFAULT_SOCKET: &str = "/tmp/uartd.sock";
pub const DEFAULT_LOG_DIR: &str = "/tmp/uartd";
pub const DEFAULT_BUFFER_CAP: usize = 1024 * 1024;
pub const DEFAULT_INTER_LINE_MS: u64 = 20;
pub const DEFAULT_INTER_CHAR_MS: u64 = 0;
pub const DEFAULT_RECONNECT_MS: u64 = 500;

/// Fully resolved configuration the daemon runs from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub port: String,
    pub baud: u32,
    pub data_bits: u8,
    pub parity: Parity,
    pub stop_bits: u8,
    pub socket_path: PathBuf,
    pub log_dir: PathBuf,
    pub buffer_cap: usize,
    pub inter_line: Duration,
    pub inter_char: Duration,
    pub reconnect_backoff: Duration,
    /// Auto-login is opt-in: only attempted when both user and pass are set.
    pub login_user: Option<String>,
    pub login_pass: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Parity {
    N,
    E,
    O,
}

/// A layer of (possibly absent) settings. `None` fields defer to the layer below.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
pub struct PartialConfig {
    pub port: Option<String>,
    pub baud: Option<u32>,
    pub data_bits: Option<u8>,
    pub parity: Option<Parity>,
    pub stop_bits: Option<u8>,
    pub socket_path: Option<PathBuf>,
    pub log_dir: Option<PathBuf>,
    pub buffer_cap: Option<usize>,
    pub inter_line_ms: Option<u64>,
    pub inter_char_ms: Option<u64>,
    pub reconnect_ms: Option<u64>,
    pub login_user: Option<String>,
    pub login_pass: Option<String>,
}

#[derive(Debug)]
pub enum ConfigError {
    MissingPort,
    BadToml(String),
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::MissingPort => write!(
                f,
                "no serial port configured (set --port, UARTD_PORT, or port in the config file)"
            ),
            ConfigError::BadToml(e) => write!(f, "config file parse error: {e}"),
        }
    }
}
impl std::error::Error for ConfigError {}

impl PartialConfig {
    /// Parse a TOML config-file body.
    pub fn from_toml_str(s: &str) -> Result<Self, ConfigError> {
        toml::from_str(s).map_err(|e| ConfigError::BadToml(e.to_string()))
    }

    /// Read settings from the environment via a getter (injected for testability). Keys are
    /// `UARTD_PORT`, `UARTD_BAUD`, `UARTD_SOCKET`, `UARTD_LOG_DIR`, `UARTD_BUFFER_CAP`,
    /// `UARTD_INTER_LINE_MS`, `UARTD_INTER_CHAR_MS`, `UARTD_RECONNECT_MS`,
    /// `UARTD_LOGIN_USER`, `UARTD_LOGIN_PASS`. Unparseable numbers are ignored.
    pub fn from_env<F: Fn(&str) -> Option<String>>(get: F) -> Self {
        PartialConfig {
            port: get("UARTD_PORT"),
            baud: get("UARTD_BAUD").and_then(|v| v.parse().ok()),
            data_bits: get("UARTD_DATA_BITS").and_then(|v| v.parse().ok()),
            parity: get("UARTD_PARITY").and_then(|v| parse_parity(&v)),
            stop_bits: get("UARTD_STOP_BITS").and_then(|v| v.parse().ok()),
            socket_path: get("UARTD_SOCKET").map(PathBuf::from),
            log_dir: get("UARTD_LOG_DIR").map(PathBuf::from),
            buffer_cap: get("UARTD_BUFFER_CAP").and_then(|v| v.parse().ok()),
            inter_line_ms: get("UARTD_INTER_LINE_MS").and_then(|v| v.parse().ok()),
            inter_char_ms: get("UARTD_INTER_CHAR_MS").and_then(|v| v.parse().ok()),
            reconnect_ms: get("UARTD_RECONNECT_MS").and_then(|v| v.parse().ok()),
            login_user: get("UARTD_LOGIN_USER"),
            login_pass: get("UARTD_LOGIN_PASS"),
        }
    }

    /// Overlay `higher` onto `self`: any `Some` in `higher` wins.
    fn overlay(self, higher: PartialConfig) -> PartialConfig {
        PartialConfig {
            port: higher.port.or(self.port),
            baud: higher.baud.or(self.baud),
            data_bits: higher.data_bits.or(self.data_bits),
            parity: higher.parity.or(self.parity),
            stop_bits: higher.stop_bits.or(self.stop_bits),
            socket_path: higher.socket_path.or(self.socket_path),
            log_dir: higher.log_dir.or(self.log_dir),
            buffer_cap: higher.buffer_cap.or(self.buffer_cap),
            inter_line_ms: higher.inter_line_ms.or(self.inter_line_ms),
            inter_char_ms: higher.inter_char_ms.or(self.inter_char_ms),
            reconnect_ms: higher.reconnect_ms.or(self.reconnect_ms),
            login_user: higher.login_user.or(self.login_user),
            login_pass: higher.login_pass.or(self.login_pass),
        }
    }
}

fn parse_parity(s: &str) -> Option<Parity> {
    match s.to_ascii_uppercase().as_str() {
        "N" | "NONE" => Some(Parity::N),
        "E" | "EVEN" => Some(Parity::E),
        "O" | "ODD" => Some(Parity::O),
        _ => None,
    }
}

/// Resolve the final config from the layers, lowest precedence first:
/// defaults < `file` < `env` < `flags`.
pub fn resolve(
    file: PartialConfig,
    env: PartialConfig,
    flags: PartialConfig,
) -> Result<Config, ConfigError> {
    let merged = file.overlay(env).overlay(flags);
    Ok(Config {
        port: merged.port.ok_or(ConfigError::MissingPort)?,
        baud: merged.baud.unwrap_or(DEFAULT_BAUD),
        data_bits: merged.data_bits.unwrap_or(8),
        parity: merged.parity.unwrap_or(Parity::N),
        stop_bits: merged.stop_bits.unwrap_or(1),
        socket_path: merged
            .socket_path
            .unwrap_or_else(|| PathBuf::from(DEFAULT_SOCKET)),
        log_dir: merged
            .log_dir
            .unwrap_or_else(|| PathBuf::from(DEFAULT_LOG_DIR)),
        buffer_cap: merged.buffer_cap.unwrap_or(DEFAULT_BUFFER_CAP),
        inter_line: Duration::from_millis(merged.inter_line_ms.unwrap_or(DEFAULT_INTER_LINE_MS)),
        inter_char: Duration::from_millis(merged.inter_char_ms.unwrap_or(DEFAULT_INTER_CHAR_MS)),
        reconnect_backoff: Duration::from_millis(
            merged.reconnect_ms.unwrap_or(DEFAULT_RECONNECT_MS),
        ),
        login_user: merged.login_user,
        login_pass: merged.login_pass,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn just_port() -> PartialConfig {
        PartialConfig {
            port: Some("/dev/ttyUSB0".into()),
            ..Default::default()
        }
    }

    #[test]
    fn defaults_apply_when_only_port_given() {
        let c = resolve(
            PartialConfig::default(),
            PartialConfig::default(),
            just_port(),
        )
        .unwrap();
        assert_eq!(c.port, "/dev/ttyUSB0");
        assert_eq!(c.baud, 115200);
        assert_eq!(c.data_bits, 8);
        assert_eq!(c.parity, Parity::N);
        assert_eq!(c.stop_bits, 1);
        assert_eq!(c.inter_line, Duration::from_millis(20));
        assert_eq!(c.buffer_cap, DEFAULT_BUFFER_CAP);
        assert!(c.login_user.is_none());
    }

    #[test]
    fn missing_port_is_error() {
        let err = resolve(
            PartialConfig::default(),
            PartialConfig::default(),
            PartialConfig::default(),
        );
        assert!(matches!(err, Err(ConfigError::MissingPort)));
    }

    #[test]
    fn precedence_flag_over_env_over_file_over_default() {
        let file = PartialConfig {
            baud: Some(9600),
            ..just_port()
        };
        let env = PartialConfig {
            baud: Some(57600),
            ..Default::default()
        };
        let flags = PartialConfig {
            baud: Some(38400),
            ..Default::default()
        };
        assert_eq!(
            resolve(file.clone(), env.clone(), flags).unwrap().baud,
            38400
        );
        assert_eq!(
            resolve(file.clone(), env, PartialConfig::default())
                .unwrap()
                .baud,
            57600
        );
        assert_eq!(
            resolve(file, PartialConfig::default(), PartialConfig::default())
                .unwrap()
                .baud,
            9600
        );
    }

    #[test]
    fn parses_toml_file() {
        let toml = r#"
            port = "/dev/ttyUSB1"
            baud = 57600
            parity = "e"
            login_user = "root"
            login_pass = "toor"
            inter_line_ms = 50
        "#;
        let p = PartialConfig::from_toml_str(toml).unwrap();
        let c = resolve(p, PartialConfig::default(), PartialConfig::default()).unwrap();
        assert_eq!(c.port, "/dev/ttyUSB1");
        assert_eq!(c.baud, 57600);
        assert_eq!(c.parity, Parity::E);
        assert_eq!(c.login_user.as_deref(), Some("root"));
        assert_eq!(c.inter_line, Duration::from_millis(50));
    }

    #[test]
    fn reads_from_env_getter() {
        let env = PartialConfig::from_env(|k| match k {
            "UARTD_PORT" => Some("/dev/ttyS0".into()),
            "UARTD_BAUD" => Some("230400".into()),
            "UARTD_LOGIN_USER" => Some("admin".into()),
            "UARTD_BUFFER_CAP" => Some("not_a_number".into()), // ignored
            _ => None,
        });
        let c = resolve(PartialConfig::default(), env, PartialConfig::default()).unwrap();
        assert_eq!(c.port, "/dev/ttyS0");
        assert_eq!(c.baud, 230400);
        assert_eq!(c.login_user.as_deref(), Some("admin"));
        assert_eq!(c.buffer_cap, DEFAULT_BUFFER_CAP); // bad value ignored -> default
    }

    #[test]
    fn auto_login_only_when_both_set() {
        let p = PartialConfig {
            login_user: Some("root".into()),
            login_pass: Some("x".into()),
            ..just_port()
        };
        let c = resolve(p, PartialConfig::default(), PartialConfig::default()).unwrap();
        assert!(c.login_user.is_some() && c.login_pass.is_some());
    }
}
