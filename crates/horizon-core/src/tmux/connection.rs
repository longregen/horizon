//! Manages the subprocess running `tmux -CC` in control mode.
//!
//! [`TmuxConnection`] owns the child process and provides a channel-based
//! interface for reading control-mode events and writing commands.  A
//! background reader thread continuously reads stdout and forwards bytes
//! through an `mpsc` channel, keeping the main thread non-blocking.

use std::io::{BufRead, BufReader, BufWriter, Write};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::thread::JoinHandle;

use crate::error::{Error, Result};

/// A live connection to a tmux server via control mode.
///
/// Dropping this struct sends `detach` to tmux (keeping the server alive) and
/// then waits for the control client process to exit.
pub struct TmuxConnection {
    child: Child,
    writer: BufWriter<std::process::ChildStdin>,
    line_rx: mpsc::Receiver<String>,
    reader_handle: Option<JoinHandle<()>>,
    command_counter: AtomicU64,
    socket_name: Option<String>,
}

/// Options for establishing a tmux connection.
pub struct TmuxConnectOptions {
    /// Custom socket name (`tmux -L <name>`).  Defaults to `"horizon"`.
    pub socket_name: Option<String>,

    /// Session name to create or attach to.
    pub session_name: Option<String>,

    /// Path to the tmux binary.  Defaults to `"tmux"` (found on `$PATH`).
    pub tmux_bin: Option<String>,

    /// Base directory for new sessions.
    pub cwd: Option<String>,
}

impl Default for TmuxConnectOptions {
    fn default() -> Self {
        Self {
            socket_name: Some("horizon".to_string()),
            session_name: None,
            tmux_bin: None,
            cwd: None,
        }
    }
}

impl TmuxConnection {
    /// Start a new tmux control-mode client.
    ///
    /// If a server with the given socket name is already running, this attaches
    /// to it.  Otherwise a new server and default session are created.
    ///
    /// # Errors
    ///
    /// Returns an error if the tmux binary cannot be found or the process
    /// cannot be spawned.
    pub fn connect(options: TmuxConnectOptions) -> Result<Self> {
        let tmux_bin = options.tmux_bin.as_deref().unwrap_or("tmux");

        // Try to attach first — if the server is running this succeeds
        // immediately.  If not, fall back to creating a new session.
        let child_result = Self::try_attach(tmux_bin, &options);

        let mut child = match child_result {
            Ok(child) => child,
            Err(_) => Self::try_new_session(tmux_bin, &options)?,
        };

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| Error::Tmux("failed to capture tmux stdin".to_string()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error::Tmux("failed to capture tmux stdout".to_string()))?;

        // Spawn a reader thread that sends complete lines through a channel.
        // This keeps the main thread non-blocking without requiring unsafe
        // fcntl calls.
        let (line_tx, line_rx) = mpsc::channel();
        let reader_handle = std::thread::Builder::new()
            .name("tmux-reader".to_string())
            .spawn(move || {
                let reader = BufReader::new(stdout);
                for line_result in reader.lines() {
                    match line_result {
                        Ok(line) => {
                            if line_tx.send(line).is_err() {
                                break; // Receiver dropped.
                            }
                        }
                        Err(e) => {
                            tracing::debug!("tmux reader error: {e}");
                            break;
                        }
                    }
                }
                tracing::debug!("tmux reader thread exiting");
            })
            .map_err(|e| Error::Tmux(format!("failed to spawn tmux reader thread: {e}")))?;

        Ok(Self {
            child,
            writer: BufWriter::new(stdin),
            line_rx,
            reader_handle: Some(reader_handle),
            command_counter: AtomicU64::new(1),
            socket_name: options.socket_name,
        })
    }

    /// Send a tmux command through the control channel.
    ///
    /// Returns a command number that will appear in the corresponding
    /// `%begin`/`%end` response block.
    ///
    /// # Errors
    ///
    /// Returns an error if writing to the tmux process fails.
    pub fn send_command(&mut self, command: &str) -> Result<u64> {
        let cmd_num = self.command_counter.fetch_add(1, Ordering::Relaxed);
        writeln!(self.writer, "{command}").map_err(|e| Error::Tmux(format!("failed to send command to tmux: {e}")))?;
        self.writer
            .flush()
            .map_err(|e| Error::Tmux(format!("failed to flush tmux stdin: {e}")))?;
        Ok(cmd_num)
    }

    /// Drain all available lines from the reader thread (non-blocking).
    ///
    /// Returns lines that have been read since the last call.
    pub fn drain_lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        while let Ok(line) = self.line_rx.try_recv() {
            lines.push(line);
        }
        lines
    }

    /// Check if the tmux process is still running.
    #[must_use]
    pub fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// The socket name used for this connection.
    #[must_use]
    pub fn socket_name(&self) -> Option<&str> {
        self.socket_name.as_deref()
    }

    /// Gracefully detach from the tmux server (keeping it alive), then wait
    /// for the control client to exit.
    pub fn detach(&mut self) {
        let _ = self.send_command("detach");
        // Give the process a moment to exit, then clean up.
        let _ = self.child.wait();
    }

    // -----------------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------------

    fn try_attach(tmux_bin: &str, options: &TmuxConnectOptions) -> Result<Child> {
        let mut cmd = Command::new(tmux_bin);
        cmd.arg("-CC");

        if let Some(socket) = &options.socket_name {
            cmd.args(["-L", socket]);
        }

        cmd.arg("attach-session");

        if let Some(session) = &options.session_name {
            cmd.args(["-t", session]);
        }

        cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());

        cmd.spawn()
            .map_err(|e| Error::Tmux(format!("failed to attach to tmux: {e}")))
    }

    fn try_new_session(tmux_bin: &str, options: &TmuxConnectOptions) -> Result<Child> {
        let mut cmd = Command::new(tmux_bin);
        cmd.arg("-CC");

        if let Some(socket) = &options.socket_name {
            cmd.args(["-L", socket]);
        }

        cmd.arg("new-session");

        if let Some(session) = &options.session_name {
            cmd.args(["-s", session]);
        }

        if let Some(cwd) = &options.cwd {
            cmd.args(["-c", cwd]);
        }

        cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());

        cmd.spawn()
            .map_err(|e| Error::Tmux(format!("failed to start tmux: {e}")))
    }
}

impl Drop for TmuxConnection {
    fn drop(&mut self) {
        self.detach();
        // Let the reader thread finish naturally (stdout closes on process exit).
        drop(self.reader_handle.take());
    }
}
