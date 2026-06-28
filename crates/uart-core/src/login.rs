// SPDX-License-Identifier: Apache-2.0
//
// Opt-in auto-login state machine. Watches the incoming stream for a `login:` prompt and
// then a `password:` prompt, emitting the configured credentials to send back so a shell is
// ready by the time the agent polls. It is re-armed on every (re)connect — a rebooting device
// shows `login:` again and again — and also re-triggers if the prompt reappears (e.g. after a
// failed attempt), since `login:` resets it.
//
// Pure: `feed` returns what to send; the daemon does the actual paced write. Prompt detection
// is a substring match (case-insensitive) over a small rolling window, robust to the prompt
// arriving split across reads.

use regex::Regex;

const WINDOW: usize = 256;

pub struct AutoLogin {
    user: String,
    pass: String,
    login_re: Regex,
    pass_re: Regex,
    window: String,
    awaiting_password: bool,
}

impl AutoLogin {
    pub fn new(user: String, pass: String) -> Self {
        AutoLogin {
            user,
            pass,
            login_re: Regex::new(r"(?i)login:").unwrap(),
            pass_re: Regex::new(r"(?i)password:").unwrap(),
            window: String::new(),
            awaiting_password: false,
        }
    }

    /// Reset to waiting for a fresh `login:` prompt (called on reconnect).
    pub fn rearm(&mut self) {
        self.awaiting_password = false;
        self.window.clear();
    }

    /// Feed captured bytes; returns credential strings to send (newline added by the caller).
    pub fn feed(&mut self, data: &[u8]) -> Vec<String> {
        self.window.push_str(&String::from_utf8_lossy(data));
        if self.window.len() > WINDOW {
            let cut = self.window.len() - WINDOW;
            // land on a char boundary
            let mut start = cut;
            while start < self.window.len() && !self.window.is_char_boundary(start) {
                start += 1;
            }
            self.window = self.window[start..].to_string();
        }

        let mut out = Vec::new();
        loop {
            if !self.awaiting_password && self.login_re.is_match(&self.window) {
                out.push(self.user.clone());
                self.awaiting_password = true;
                self.window.clear();
                continue;
            }
            if self.awaiting_password && self.pass_re.is_match(&self.window) {
                out.push(self.pass.clone());
                self.awaiting_password = false;
                self.window.clear();
                continue;
            }
            break;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn al() -> AutoLogin {
        AutoLogin::new("root".into(), "toor".into())
    }

    #[test]
    fn login_then_password_emits_both() {
        let mut a = al();
        assert_eq!(a.feed(b"felix login: "), vec!["root".to_string()]);
        assert_eq!(a.feed(b"Password: "), vec!["toor".to_string()]);
    }

    #[test]
    fn prompt_split_across_chunks() {
        let mut a = al();
        assert!(a.feed(b"raspberry ").is_empty());
        assert_eq!(a.feed(b"log"), Vec::<String>::new());
        assert_eq!(a.feed(b"in: "), vec!["root".to_string()]);
    }

    #[test]
    fn password_only_after_login() {
        let mut a = al();
        // a stray password prompt before any login: is ignored
        assert!(a.feed(b"enter password: ").is_empty());
    }

    #[test]
    fn rearm_allows_relogin() {
        let mut a = al();
        assert_eq!(a.feed(b"login: "), vec!["root".to_string()]);
        assert_eq!(a.feed(b"password: "), vec!["toor".to_string()]);
        // device rebooted; daemon re-armed
        a.rearm();
        assert_eq!(a.feed(b"login: "), vec!["root".to_string()]);
    }

    #[test]
    fn failed_attempt_reprompts_relogin_without_rearm() {
        let mut a = al();
        a.feed(b"login: ");
        a.feed(b"password: ");
        // wrong creds -> device shows login: again; should re-send user
        assert_eq!(a.feed(b"login: "), vec!["root".to_string()]);
    }

    #[test]
    fn case_insensitive() {
        let mut a = al();
        assert_eq!(a.feed(b"Login: "), vec!["root".to_string()]);
    }

    #[test]
    fn no_prompt_no_output() {
        let mut a = al();
        assert!(a.feed(b"just some kernel log line\n").is_empty());
    }
}
