//! Tmux control-mode backend for Horizon.
//!
//! Instead of spawning raw PTYs, this module manages terminal sessions through
//! a tmux server running in control mode (`-CC`).  Sessions survive SSH
//! disconnects and Horizon restarts.
//!
//! # Architecture
//!
//! ```text
//! Horizon UI ←→ TmuxBackend ←→ tmux -CC (control mode) ←→ tmux server
//!                    │                                         │
//!                    │  poll_events() → TmuxEvent              │  sessions
//!                    │  send_keys()                             │  windows
//!                    │  create_window()                         │  panes
//! ```
//!
//! The backend is a singleton owned by the [`Board`](crate::Board).  Each
//! frame the board calls [`TmuxBackend::poll_events`] to drain incoming
//! control-mode notifications, then routes `Output` events to the appropriate
//! panel's VT100 parser.

pub mod connection;
pub mod control;
pub mod session;
pub mod terminal;

use std::path::Path;

use self::connection::{TmuxConnectOptions, TmuxConnection};
use self::control::{ControlEvent, ControlParser, PaneId, SessionId, WindowId};
use self::session::TmuxState;
use crate::error::Result;

// Re-exports for convenience.
pub use self::control::{PaneId as TmuxPaneId, SessionId as TmuxSessionId, WindowId as TmuxWindowId};

/// Events emitted by the tmux backend for the UI layer to consume.
#[derive(Clone, Debug)]
pub enum TmuxEvent {
    /// Terminal output for a specific pane.
    Output { pane_id: PaneId, data: Vec<u8> },

    /// A new window was created in tmux.
    WindowAdded { window_id: WindowId },

    /// A window was closed in tmux.
    WindowClosed { window_id: WindowId },

    /// A window was renamed.
    WindowRenamed { window_id: WindowId, name: String },

    /// The active session changed.
    SessionChanged { session_id: SessionId, name: String },

    /// The tmux server is exiting.
    ServerExit,

    /// The client was detached.
    Detached,
}

/// The tmux control-mode backend.
///
/// Owns the connection to the tmux server and the in-memory mirror of tmux
/// state (sessions, windows, panes).
pub struct TmuxBackend {
    connection: TmuxConnection,
    parser: ControlParser,
    state: TmuxState,
}

impl TmuxBackend {
    /// Connect to a tmux server (or start one) in control mode.
    ///
    /// # Errors
    ///
    /// Returns an error if the tmux binary cannot be found or the connection
    /// fails.
    pub fn connect(options: TmuxConnectOptions) -> Result<Self> {
        let connection = TmuxConnection::connect(options)?;
        Ok(Self {
            connection,
            parser: ControlParser::new(),
            state: TmuxState::default(),
        })
    }

    /// Reattach to an existing tmux server by socket name.
    ///
    /// # Errors
    ///
    /// Returns an error if the server is not running or attachment fails.
    pub fn reattach(socket_name: &str) -> Result<Self> {
        Self::connect(TmuxConnectOptions {
            socket_name: Some(socket_name.to_string()),
            ..TmuxConnectOptions::default()
        })
    }

    // -----------------------------------------------------------------------
    // Session management
    // -----------------------------------------------------------------------

    /// Create a new tmux session (maps to a Horizon workspace).
    ///
    /// # Errors
    ///
    /// Returns an error if the tmux command fails.
    pub fn create_session(&mut self, name: &str) -> Result<u64> {
        self.connection.send_command(&format!("new-session -d -s {name}"))
    }

    /// List existing sessions by querying the tmux server.
    ///
    /// # Errors
    ///
    /// Returns an error if the command cannot be sent.
    pub fn list_sessions(&mut self) -> Result<u64> {
        self.connection
            .send_command("list-sessions -F '#{session_id} #{session_name}'")
    }

    // -----------------------------------------------------------------------
    // Window management (maps to Horizon panels)
    // -----------------------------------------------------------------------

    /// Create a new window in the current session.
    ///
    /// # Errors
    ///
    /// Returns an error if the tmux command fails.
    pub fn create_window(&mut self, name: Option<&str>, cwd: Option<&Path>) -> Result<u64> {
        use std::fmt::Write;
        let mut cmd = String::from("new-window");
        if let Some(name) = name {
            let _ = write!(cmd, " -n '{name}'");
        }
        if let Some(cwd) = cwd {
            let _ = write!(cmd, " -c '{}'", cwd.display());
        }
        self.connection.send_command(&cmd)
    }

    /// Create a window running a specific command.
    ///
    /// # Errors
    ///
    /// Returns an error if the tmux command fails.
    pub fn create_window_with_command(&mut self, command: &str, name: Option<&str>, cwd: Option<&Path>) -> Result<u64> {
        use std::fmt::Write;
        let mut cmd = String::from("new-window");
        if let Some(name) = name {
            let _ = write!(cmd, " -n '{name}'");
        }
        if let Some(cwd) = cwd {
            let _ = write!(cmd, " -c '{}'", cwd.display());
        }
        let _ = write!(cmd, " '{command}'");
        self.connection.send_command(&cmd)
    }

    /// Close a tmux window.
    ///
    /// # Errors
    ///
    /// Returns an error if the tmux command fails.
    pub fn kill_window(&mut self, window_id: WindowId) -> Result<u64> {
        self.connection
            .send_command(&format!("kill-window -t @{}", window_id.0))
    }

    /// Rename a tmux window.
    ///
    /// # Errors
    ///
    /// Returns an error if the tmux command fails.
    pub fn rename_window(&mut self, window_id: WindowId, name: &str) -> Result<u64> {
        self.connection
            .send_command(&format!("rename-window -t @{} '{name}'", window_id.0))
    }

    // -----------------------------------------------------------------------
    // Input
    // -----------------------------------------------------------------------

    /// Send raw key bytes to a tmux pane.
    ///
    /// Uses `send-keys -H` (hex mode) for binary safety.
    ///
    /// # Errors
    ///
    /// Returns an error if the tmux command fails.
    pub fn send_keys(&mut self, pane_id: PaneId, data: &[u8]) -> Result<u64> {
        if data.is_empty() {
            return Ok(0);
        }

        // Build hex-encoded key list for `send-keys -H`.
        let hex_keys: Vec<String> = data.iter().map(|b| format!("{b:02x}")).collect();
        let hex_str = hex_keys.join(" ");
        self.connection
            .send_command(&format!("send-keys -t %{} -H {hex_str}", pane_id.0))
    }

    /// Send literal text to a pane (for typing).
    ///
    /// # Errors
    ///
    /// Returns an error if the tmux command fails.
    pub fn send_text(&mut self, pane_id: PaneId, text: &str) -> Result<u64> {
        self.connection.send_command(&format!(
            "send-keys -t %{} -l '{}'",
            pane_id.0,
            text.replace('\'', "'\\''")
        ))
    }

    // -----------------------------------------------------------------------
    // Resize
    // -----------------------------------------------------------------------

    /// Resize a tmux window/pane.
    ///
    /// # Errors
    ///
    /// Returns an error if the tmux command fails.
    pub fn resize_pane(&mut self, pane_id: PaneId, cols: u16, rows: u16) -> Result<u64> {
        self.connection
            .send_command(&format!("resize-pane -t %{} -x {cols} -y {rows}", pane_id.0))
    }

    // -----------------------------------------------------------------------
    // Event polling
    // -----------------------------------------------------------------------

    /// Poll for events from the tmux control mode stream.
    ///
    /// Call this once per frame from the main thread.  Returns a list of
    /// events to process (output routing, window lifecycle, etc.).
    ///
    /// This method is non-blocking.
    pub fn poll_events(&mut self) -> Vec<TmuxEvent> {
        // Drain complete lines from the reader thread.
        for line in self.connection.drain_lines() {
            // Re-add the newline that BufRead::lines() strips, since the
            // control parser expects newline-terminated input.
            let mut bytes = line.into_bytes();
            bytes.push(b'\n');
            self.parser.feed(&bytes);
        }

        // Convert parsed control events to TmuxEvents.
        let mut events = Vec::new();
        while let Some(control_event) = self.parser.poll() {
            if let Some(tmux_event) = self.handle_control_event(control_event) {
                events.push(tmux_event);
            }
        }
        events
    }

    /// Read-only access to the current tmux state mirror.
    #[must_use]
    pub fn state(&self) -> &TmuxState {
        &self.state
    }

    /// Whether the tmux process is still running.
    #[must_use]
    pub fn is_alive(&mut self) -> bool {
        self.connection.is_alive()
    }

    /// Gracefully detach (tmux server keeps running).
    pub fn detach(&mut self) {
        self.connection.detach();
    }

    // -----------------------------------------------------------------------
    // Internal event handling
    // -----------------------------------------------------------------------

    fn handle_control_event(&mut self, event: ControlEvent) -> Option<TmuxEvent> {
        match event {
            ControlEvent::Output { pane_id, data } => Some(TmuxEvent::Output { pane_id, data }),

            ControlEvent::WindowAdd { window_id } => {
                self.state.add_window(window_id);
                tracing::info!("tmux window added: @{}", window_id.0);
                Some(TmuxEvent::WindowAdded { window_id })
            }

            ControlEvent::WindowClose { window_id } => {
                self.state.remove_window(window_id);
                tracing::info!("tmux window closed: @{}", window_id.0);
                Some(TmuxEvent::WindowClosed { window_id })
            }

            ControlEvent::WindowRenamed { window_id, name } => {
                self.state.rename_window(window_id, name.clone());
                Some(TmuxEvent::WindowRenamed { window_id, name })
            }

            ControlEvent::SessionChanged { session_id, name } => {
                self.state.set_active_session(session_id, name.clone());
                tracing::info!("tmux session changed: ${} ({})", session_id.0, name);
                Some(TmuxEvent::SessionChanged { session_id, name })
            }

            ControlEvent::SessionRenamed { name } => {
                self.state.rename_active_session(name);
                None
            }

            ControlEvent::LayoutChange { window_id, layout } => {
                self.state.set_window_layout(window_id, layout);
                None
            }

            ControlEvent::CommandResponse { command_number, lines } => {
                tracing::debug!("tmux command #{command_number} ok: {} lines", lines.len());
                None
            }

            ControlEvent::CommandError { command_number, lines } => {
                tracing::warn!("tmux command #{command_number} error: {}", lines.join("\n"));
                None
            }

            ControlEvent::Exit { reason } => {
                tracing::info!("tmux server exiting: {:?}", reason);
                Some(TmuxEvent::ServerExit)
            }

            ControlEvent::ClientDetached => {
                tracing::info!("tmux client detached");
                Some(TmuxEvent::Detached)
            }

            ControlEvent::PaneModeChanged { .. } | ControlEvent::Unknown { .. } => None,
        }
    }
}
