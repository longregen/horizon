//! Parser for tmux control mode (`-CC`) protocol messages.
//!
//! When tmux runs in control mode it communicates via structured text lines on
//! stdout.  Each notification starts with `%` and carries an event type plus
//! payload.  Command responses are bracketed by `%begin` / `%end` (or
//! `%error`) sentinels.
//!
//! Reference: `tmux(1)` §CONTROL MODE.

use std::collections::VecDeque;

// ---------------------------------------------------------------------------
// Parsed event types
// ---------------------------------------------------------------------------

/// A single event parsed from the tmux control mode stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ControlEvent {
    /// Raw terminal output destined for a specific pane.
    Output { pane_id: PaneId, data: Vec<u8> },

    /// A new window was created.
    WindowAdd { window_id: WindowId },

    /// A window was closed.
    WindowClose { window_id: WindowId },

    /// A window was renamed.
    WindowRenamed { window_id: WindowId, name: String },

    /// The active session changed.
    SessionChanged { session_id: SessionId, name: String },

    /// A session was renamed.
    SessionRenamed { name: String },

    /// A completed command response (success).
    CommandResponse { command_number: u64, lines: Vec<String> },

    /// A failed command response.
    CommandError { command_number: u64, lines: Vec<String> },

    /// The layout of a window changed (e.g. after resize or pane split).
    LayoutChange { window_id: WindowId, layout: String },

    /// A pane mode changed (copy mode entered/exited).
    PaneModeChanged { pane_id: PaneId },

    /// The tmux client is exiting.
    ClientDetached,

    /// The tmux server is exiting.
    Exit { reason: Option<String> },

    /// An unrecognised notification — stored for forward compatibility.
    Unknown { line: String },
}

// ---------------------------------------------------------------------------
// ID wrappers
// ---------------------------------------------------------------------------

/// Tmux pane identifier (e.g. `%0`, `%5`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PaneId(pub u32);

/// Tmux window identifier (e.g. `@0`, `@3`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct WindowId(pub u32);

/// Tmux session identifier (e.g. `$0`, `$1`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SessionId(pub u32);

// ---------------------------------------------------------------------------
// Parser state machine
// ---------------------------------------------------------------------------

/// Incremental parser for the tmux control mode byte stream.
///
/// Feed raw bytes via [`ControlParser::feed`] and drain parsed events from the
/// returned iterator.  The parser buffers incomplete lines internally.
#[derive(Default)]
pub struct ControlParser {
    /// Accumulated bytes that have not yet formed a complete line.
    line_buf: Vec<u8>,

    /// Events ready to be consumed.
    pending: VecDeque<ControlEvent>,

    /// If we are inside a `%begin` block, the command number and accumulated
    /// response lines.
    command_block: Option<CommandBlock>,
}

struct CommandBlock {
    command_number: u64,
    is_error: bool,
    lines: Vec<String>,
}

impl ControlParser {
    /// Create a new parser with empty state.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed raw bytes from the tmux control mode stdout.
    ///
    /// After calling this, use [`ControlParser::poll`] to drain events.
    pub fn feed(&mut self, data: &[u8]) {
        for &byte in data {
            if byte == b'\n' {
                self.process_line();
                self.line_buf.clear();
            } else {
                self.line_buf.push(byte);
            }
        }
    }

    /// Return the next parsed event, if any.
    pub fn poll(&mut self) -> Option<ControlEvent> {
        self.pending.pop_front()
    }

    /// Drain all pending events into a `Vec`.
    pub fn drain(&mut self) -> Vec<ControlEvent> {
        self.pending.drain(..).collect()
    }

    // -----------------------------------------------------------------------
    // Internal line processing
    // -----------------------------------------------------------------------

    fn process_line(&mut self) {
        // Strip trailing \r if present (tmux can send \r\n on some platforms).
        let raw = if self.line_buf.last() == Some(&b'\r') {
            &self.line_buf[..self.line_buf.len() - 1]
        } else {
            &self.line_buf[..]
        };

        let line = String::from_utf8_lossy(raw).into_owned();

        // If we are inside a command response block, check for end/error.
        if let Some(block) = &mut self.command_block {
            if line.starts_with("%end") || line.starts_with("%error") {
                let is_error = line.starts_with("%error") || block.is_error;
                let finished = CommandBlock {
                    command_number: block.command_number,
                    is_error,
                    lines: std::mem::take(&mut block.lines),
                };
                self.command_block = None;

                if finished.is_error {
                    self.pending.push_back(ControlEvent::CommandError {
                        command_number: finished.command_number,
                        lines: finished.lines,
                    });
                } else {
                    self.pending.push_back(ControlEvent::CommandResponse {
                        command_number: finished.command_number,
                        lines: finished.lines,
                    });
                }
            } else {
                block.lines.push(line);
            }
            return;
        }

        // Notification lines start with `%`.
        if !line.starts_with('%') {
            // Non-notification lines outside a command block are ignored
            // (they can be initial greeting text).
            return;
        }

        if let Some(event) = self.parse_notification(&line) {
            self.pending.push_back(event);
        }
    }

    fn parse_notification(&mut self, line: &str) -> Option<ControlEvent> {
        let (keyword, rest) = split_first_word(line);

        match keyword {
            "%output" => parse_output(rest),
            "%window-add" => parse_window_add(rest),
            "%window-close" => parse_window_close(rest),
            "%window-renamed" => parse_window_renamed(rest),
            "%session-changed" => parse_session_changed(rest),
            "%session-renamed" => Some(ControlEvent::SessionRenamed {
                name: rest.trim().to_string(),
            }),
            "%layout-change" => parse_layout_change(rest),
            "%pane-mode-changed" => parse_pane_mode_changed(rest),
            "%client-detached" => Some(ControlEvent::ClientDetached),
            "%exit" => Some(ControlEvent::Exit {
                reason: if rest.is_empty() { None } else { Some(rest.to_string()) },
            }),
            "%begin" => {
                let command_number = parse_begin_command_number(rest);
                self.command_block = Some(CommandBlock {
                    command_number,
                    is_error: false,
                    lines: Vec::new(),
                });
                None
            }
            _ => Some(ControlEvent::Unknown { line: line.to_string() }),
        }
    }
}

// ---------------------------------------------------------------------------
// Notification parsers (free functions — no parser state needed)
// ---------------------------------------------------------------------------

fn parse_output(rest: &str) -> Option<ControlEvent> {
    let (pane_token, data_str) = split_first_word(rest);
    let pane_id = parse_pane_id(pane_token)?;
    let data = unescape_octal(data_str);
    Some(ControlEvent::Output { pane_id, data })
}

fn parse_window_add(rest: &str) -> Option<ControlEvent> {
    let window_id = parse_window_id(rest.trim())?;
    Some(ControlEvent::WindowAdd { window_id })
}

fn parse_window_close(rest: &str) -> Option<ControlEvent> {
    let (token, _) = split_first_word(rest);
    let window_id = parse_window_id(token)?;
    Some(ControlEvent::WindowClose { window_id })
}

fn parse_window_renamed(rest: &str) -> Option<ControlEvent> {
    let (token, name) = split_first_word(rest);
    let window_id = parse_window_id(token)?;
    Some(ControlEvent::WindowRenamed {
        window_id,
        name: name.to_string(),
    })
}

fn parse_session_changed(rest: &str) -> Option<ControlEvent> {
    let (token, name) = split_first_word(rest);
    let session_id = parse_session_id(token)?;
    Some(ControlEvent::SessionChanged {
        session_id,
        name: name.to_string(),
    })
}

fn parse_layout_change(rest: &str) -> Option<ControlEvent> {
    let (token, layout) = split_first_word(rest);
    let window_id = parse_window_id(token)?;
    Some(ControlEvent::LayoutChange {
        window_id,
        layout: layout.to_string(),
    })
}

fn parse_pane_mode_changed(rest: &str) -> Option<ControlEvent> {
    let pane_id = parse_pane_id(rest.trim())?;
    Some(ControlEvent::PaneModeChanged { pane_id })
}

// ---------------------------------------------------------------------------
// Helper functions
// ---------------------------------------------------------------------------

/// Split a string at the first whitespace boundary.
fn split_first_word(s: &str) -> (&str, &str) {
    match s.find(char::is_whitespace) {
        Some(idx) => (&s[..idx], s[idx..].trim_start()),
        None => (s, ""),
    }
}

/// Parse a pane id token like `%0` or `%12`.
fn parse_pane_id(token: &str) -> Option<PaneId> {
    let digits = token.strip_prefix('%')?;
    digits.parse::<u32>().ok().map(PaneId)
}

/// Parse a window id token like `@0` or `@7`.
fn parse_window_id(token: &str) -> Option<WindowId> {
    let digits = token.strip_prefix('@')?;
    digits.parse::<u32>().ok().map(WindowId)
}

/// Parse a session id token like `$0` or `$3`.
fn parse_session_id(token: &str) -> Option<SessionId> {
    let digits = token.strip_prefix('$')?;
    digits.parse::<u32>().ok().map(SessionId)
}

/// Parse the command number from a `%begin` line.
///
/// Format: `<time> <command-number> <flags>`
fn parse_begin_command_number(rest: &str) -> u64 {
    let mut parts = rest.split_whitespace();
    let _time = parts.next();
    parts.next().and_then(|s| s.parse::<u64>().ok()).unwrap_or(0)
}

/// Decode tmux octal-escaped output data.
///
/// tmux control mode escapes bytes outside printable ASCII as `\OOO` (three
/// octal digits).  Backslash itself is escaped as `\\`.
fn unescape_octal(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() {
            if bytes[i + 1] == b'\\' {
                out.push(b'\\');
                i += 2;
            } else if i + 3 < bytes.len()
                && bytes[i + 1].is_ascii_digit()
                && bytes[i + 2].is_ascii_digit()
                && bytes[i + 3].is_ascii_digit()
            {
                let val = u16::from(bytes[i + 1] - b'0') * 64
                    + u16::from(bytes[i + 2] - b'0') * 8
                    + u16::from(bytes[i + 3] - b'0');
                #[allow(clippy::cast_possible_truncation)]
                out.push(val as u8);
                i += 4;
            } else {
                // Not a valid escape — pass through.
                out.push(bytes[i]);
                i += 1;
            }
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    out
}

/// Escape bytes into tmux octal format for sending via `send-keys -H`.
#[must_use]
pub fn escape_for_tmux(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len() * 2);
    for &byte in data {
        if byte == b'\\' {
            out.push_str("\\\\");
        } else if byte.is_ascii_graphic() || byte == b' ' {
            out.push(byte as char);
        } else {
            use std::fmt::Write;
            let _ = write!(out, "\\{byte:03o}");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_output_event() {
        let mut parser = ControlParser::new();
        parser.feed(b"%output %0 hello world\n");
        let event = parser.poll();
        assert_eq!(
            event,
            Some(ControlEvent::Output {
                pane_id: PaneId(0),
                data: b"hello world".to_vec(),
            })
        );
    }

    #[test]
    fn parse_output_with_octal_escape() {
        let mut parser = ControlParser::new();
        parser.feed(b"%output %2 line1\\015\\012line2\n");
        let event = parser.poll();
        assert_eq!(
            event,
            Some(ControlEvent::Output {
                pane_id: PaneId(2),
                data: b"line1\r\nline2".to_vec(),
            })
        );
    }

    #[test]
    fn parse_window_add_event() {
        let mut parser = ControlParser::new();
        parser.feed(b"%window-add @3\n");
        assert_eq!(parser.poll(), Some(ControlEvent::WindowAdd { window_id: WindowId(3) }));
    }

    #[test]
    fn parse_window_close_event() {
        let mut parser = ControlParser::new();
        parser.feed(b"%window-close @5\n");
        assert_eq!(
            parser.poll(),
            Some(ControlEvent::WindowClose { window_id: WindowId(5) })
        );
    }

    #[test]
    fn parse_session_changed() {
        let mut parser = ControlParser::new();
        parser.feed(b"%session-changed $1 my-session\n");
        assert_eq!(
            parser.poll(),
            Some(ControlEvent::SessionChanged {
                session_id: SessionId(1),
                name: "my-session".to_string(),
            })
        );
    }

    #[test]
    fn parse_command_response_block() {
        let mut parser = ControlParser::new();
        parser.feed(b"%begin 1234 42 0\nline one\nline two\n%end 1234 42 0\n");
        assert_eq!(
            parser.poll(),
            Some(ControlEvent::CommandResponse {
                command_number: 42,
                lines: vec!["line one".to_string(), "line two".to_string()],
            })
        );
    }

    #[test]
    fn parse_command_error_block() {
        let mut parser = ControlParser::new();
        parser.feed(b"%begin 1234 7 0\nbad thing\n%error 1234 7 0\n");
        assert_eq!(
            parser.poll(),
            Some(ControlEvent::CommandError {
                command_number: 7,
                lines: vec!["bad thing".to_string()],
            })
        );
    }

    #[test]
    fn parse_exit_with_reason() {
        let mut parser = ControlParser::new();
        parser.feed(b"%exit server exited\n");
        assert_eq!(
            parser.poll(),
            Some(ControlEvent::Exit {
                reason: Some("server exited".to_string()),
            })
        );
    }

    #[test]
    fn parse_exit_without_reason() {
        let mut parser = ControlParser::new();
        parser.feed(b"%exit\n");
        assert_eq!(parser.poll(), Some(ControlEvent::Exit { reason: None }));
    }

    #[test]
    fn incremental_feed_across_line_boundary() {
        let mut parser = ControlParser::new();
        parser.feed(b"%window-add ");
        assert!(parser.poll().is_none());
        parser.feed(b"@9\n");
        assert_eq!(parser.poll(), Some(ControlEvent::WindowAdd { window_id: WindowId(9) }));
    }

    #[test]
    fn unescape_backslash() {
        assert_eq!(unescape_octal("hello\\\\world"), b"hello\\world");
    }

    #[test]
    fn escape_round_trips() {
        let input = b"hello\x1b[31mworld\n";
        let escaped = escape_for_tmux(input);
        let unescaped = unescape_octal(&escaped);
        assert_eq!(unescaped, input);
    }

    #[test]
    fn unknown_notification_preserved() {
        let mut parser = ControlParser::new();
        parser.feed(b"%future-event some data\n");
        assert_eq!(
            parser.poll(),
            Some(ControlEvent::Unknown {
                line: "%future-event some data".to_string(),
            })
        );
    }

    #[test]
    fn window_renamed_parsed() {
        let mut parser = ControlParser::new();
        parser.feed(b"%window-renamed @1 my-editor\n");
        assert_eq!(
            parser.poll(),
            Some(ControlEvent::WindowRenamed {
                window_id: WindowId(1),
                name: "my-editor".to_string(),
            })
        );
    }

    #[test]
    fn client_detached_parsed() {
        let mut parser = ControlParser::new();
        parser.feed(b"%client-detached\n");
        assert_eq!(parser.poll(), Some(ControlEvent::ClientDetached));
    }
}
