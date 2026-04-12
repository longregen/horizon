//! A terminal backed by tmux instead of a direct PTY.
//!
//! [`TmuxTerminal`] wraps an `alacritty_terminal::Term` for VT100 parsing but
//! receives its output bytes from the [`TmuxBackend`](super::TmuxBackend) and
//! routes input back through tmux `send-keys`.  This provides the same
//! rendering interface as [`Terminal`](crate::Terminal) while letting tmux own
//! the actual shell session.

use std::sync::mpsc;

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::{self, RenderableContent, TermDamage, TermMode, viewport_to_point};
use alacritty_terminal::vte::ansi;

use super::control::PaneId;
// Re-use the URL/path detection helpers from the terminal module.
use crate::terminal::support::{find_file_path_at_column, find_url_at_column};

use std::sync::Arc;

/// Options for creating a tmux-backed terminal.
pub struct TmuxTerminalOptions {
    pub pane_id: PaneId,
    pub rows: u16,
    pub cols: u16,
    pub scrollback_limit: usize,
    pub kitty_keyboard: bool,
}

/// Sender for input destined for the tmux pane.
///
/// The board routes these bytes through `TmuxBackend::send_keys`.
pub struct TmuxInputSender {
    tx: mpsc::Sender<Vec<u8>>,
}

impl TmuxInputSender {
    fn send(&self, data: Vec<u8>) {
        let _ = self.tx.send(data);
    }
}

/// Receiver end — polled by the board to forward to tmux.
pub struct TmuxInputReceiver {
    rx: mpsc::Receiver<Vec<u8>>,
}

impl TmuxInputReceiver {
    /// Drain all pending input bytes (non-blocking).
    #[must_use]
    pub fn drain(&self) -> Vec<Vec<u8>> {
        let mut chunks = Vec::new();
        while let Ok(data) = self.rx.try_recv() {
            chunks.push(data);
        }
        chunks
    }
}

/// Create a paired input sender/receiver for a tmux terminal.
#[must_use]
pub fn tmux_input_channel() -> (TmuxInputSender, TmuxInputReceiver) {
    let (tx, rx) = mpsc::channel();
    (TmuxInputSender { tx }, TmuxInputReceiver { rx })
}

// A no-op event listener — we don't need PTY events since tmux handles that.
#[derive(Clone)]
struct NoopEventListener;

impl EventListener for NoopEventListener {
    fn send_event(&self, _event: Event) {}
}

#[derive(Clone, Copy)]
struct TmuxDimensions {
    rows: usize,
    cols: usize,
}

impl TmuxDimensions {
    fn new(rows: u16, cols: u16) -> Self {
        Self {
            rows: usize::from(rows.max(1)),
            cols: usize::from(cols.max(2)),
        }
    }
}

impl Dimensions for TmuxDimensions {
    fn total_lines(&self) -> usize {
        self.rows
    }

    fn screen_lines(&self) -> usize {
        self.rows
    }

    fn columns(&self) -> usize {
        self.cols
    }
}

/// A terminal that receives output from tmux and sends input back through
/// tmux.
///
/// This provides the same rendering interface as [`Terminal`](crate::Terminal):
/// cell grid iteration, scrollback, selection, cursor, etc.  The difference is
/// that I/O goes through the tmux control-mode backend instead of a raw PTY.
pub struct TmuxTerminal {
    term: Arc<FairMutex<alacritty_terminal::term::Term<NoopEventListener>>>,
    parser: ansi::Processor<ansi::StdSyncHandler>,
    pane_id: PaneId,
    input_sender: TmuxInputSender,
    rows: u16,
    cols: u16,
    title: String,
    exited: bool,
}

impl TmuxTerminal {
    /// Create a new tmux-backed terminal.
    #[must_use]
    pub fn new(options: &TmuxTerminalOptions, input_sender: TmuxInputSender) -> Self {
        let rows = options.rows.max(1);
        let cols = options.cols.max(2);
        let dimensions = TmuxDimensions::new(rows, cols);
        let config = term::Config {
            scrolling_history: options.scrollback_limit.max(1),
            kitty_keyboard: options.kitty_keyboard,
            ..term::Config::default()
        };
        let term = Arc::new(FairMutex::new(alacritty_terminal::term::Term::new(
            config,
            &dimensions,
            NoopEventListener,
        )));

        Self {
            term,
            parser: ansi::Processor::default(),
            pane_id: options.pane_id,
            input_sender,
            rows,
            cols,
            title: String::new(),
            exited: false,
        }
    }

    /// Feed raw output bytes from tmux `%output` events into the VT100 parser.
    ///
    /// Called by the board after polling [`TmuxBackend`](super::TmuxBackend).
    /// Returns `true` if any bytes were processed.
    pub fn feed_output(&mut self, data: &[u8]) -> bool {
        if data.is_empty() {
            return false;
        }
        let mut term = self.term.lock();
        self.parser.advance(&mut *term, data);
        true
    }

    /// The tmux pane ID this terminal is bound to.
    #[must_use]
    pub fn pane_id(&self) -> PaneId {
        self.pane_id
    }

    /// Send input bytes (keystrokes) destined for the tmux pane.
    ///
    /// The bytes are buffered and forwarded to tmux by the board's event loop.
    pub fn write_input(&self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        self.input_sender.send(bytes.to_vec());
    }

    /// Resize the virtual terminal grid.
    pub fn resize(&mut self, rows: u16, cols: u16) {
        let rows = rows.max(1);
        let cols = cols.max(2);
        if rows == self.rows && cols == self.cols {
            return;
        }
        self.rows = rows;
        self.cols = cols;
        self.term.lock().resize(TmuxDimensions::new(rows, cols));
        // Note: the actual tmux pane resize is sent by the board through
        // TmuxBackend::resize_pane.
    }

    #[must_use]
    pub fn cols(&self) -> u16 {
        self.cols
    }

    #[must_use]
    pub fn rows(&self) -> u16 {
        self.rows
    }

    /// Set the panel title (updated from tmux window rename events).
    pub fn set_title(&mut self, title: String) {
        self.title = title;
    }

    #[must_use]
    pub fn title(&self) -> &str {
        &self.title
    }

    /// Mark this terminal as exited (the tmux window was closed).
    pub fn mark_exited(&mut self) {
        self.exited = true;
    }

    #[must_use]
    pub fn child_exited(&self) -> bool {
        self.exited
    }

    #[must_use]
    pub fn mode(&self) -> TermMode {
        *self.term.lock().mode()
    }

    pub fn set_focused(&mut self, focused: bool) {
        let mut term = self.term.lock();
        if term.is_focused == focused {
            return;
        }
        term.is_focused = focused;

        if term.mode().contains(TermMode::FOCUS_IN_OUT) {
            drop(term);
            let sequence = if focused { b"\x1b[I" } else { b"\x1b[O" };
            self.write_input(sequence);
        }
    }

    // -- Rendering ----------------------------------------------------------

    pub fn with_renderable_content<R>(&self, render: impl FnOnce(RenderableContent<'_>) -> R) -> R {
        let term = self.term.lock();
        render(term.renderable_content())
    }

    pub fn with_damage<R>(&self, update: impl FnOnce(TermDamage<'_>) -> R) -> R {
        let mut term = self.term.lock();
        update(term.damage())
    }

    pub fn reset_damage(&self) {
        self.term.lock().reset_damage();
    }

    // -- Scrollback ---------------------------------------------------------

    #[must_use]
    pub fn scrollback(&self) -> usize {
        self.term.lock().grid().display_offset()
    }

    pub fn set_scrollback(&mut self, scrollback: usize) {
        let current = self.scrollback();
        if current == scrollback {
            return;
        }
        let current_i = isize::try_from(current).unwrap_or(isize::MAX);
        let target_i = isize::try_from(scrollback).unwrap_or(isize::MAX);
        let delta = target_i.saturating_sub(current_i);
        let delta = delta.clamp(i32::MIN as isize, i32::MAX as isize);
        #[allow(clippy::cast_possible_truncation)]
        let delta = delta as i32;
        self.term.lock().scroll_display(Scroll::Delta(delta));
    }

    pub fn scroll_scrollback_by(&mut self, delta: i32) {
        if delta == 0 {
            return;
        }
        let current = self.scrollback();
        let target = if delta.is_positive() {
            current.saturating_add(usize::try_from(delta).unwrap_or(usize::MAX))
        } else {
            current.saturating_sub(usize::try_from(delta.unsigned_abs()).unwrap_or(usize::MAX))
        };
        self.set_scrollback(target);
    }

    #[must_use]
    pub fn scrollback_limit(&self) -> usize {
        // Not easily accessible from Term — use the grid dimensions.
        let term = self.term.lock();
        let grid = term.grid();
        grid.total_lines().saturating_sub(grid.screen_lines())
    }

    #[must_use]
    pub fn history_size(&self) -> usize {
        let term = self.term.lock();
        let grid = term.grid();
        grid.total_lines().saturating_sub(grid.screen_lines())
    }

    // -- Selection ----------------------------------------------------------

    pub fn start_selection(&self, sel_type: SelectionType, row: usize, col: usize, side: Side) {
        let mut term = self.term.lock();
        let display_offset = term.grid().display_offset();
        let point = viewport_to_point(display_offset, Point::new(row, Column(col)));
        term.selection = Some(Selection::new(sel_type, point, side));
    }

    pub fn update_selection(&self, row: usize, col: usize, side: Side) {
        let mut term = self.term.lock();
        let display_offset = term.grid().display_offset();
        let point = viewport_to_point(display_offset, Point::new(row, Column(col)));
        if let Some(selection) = term.selection.as_mut() {
            selection.update(point, side);
            selection.include_all();
        }
    }

    pub fn clear_selection(&self) {
        self.term.lock().selection = None;
    }

    #[must_use]
    pub fn has_selection(&self) -> bool {
        self.term.lock().selection.is_some()
    }

    #[must_use]
    pub fn selection_to_string(&self) -> Option<String> {
        self.term.lock().selection_to_string()
    }

    // -- Content inspection -------------------------------------------------

    #[must_use]
    pub fn last_lines_text(&self, max_lines: usize) -> String {
        let term = self.term.lock();
        let content = term.renderable_content();
        let cols = usize::from(self.cols);
        let rows = usize::from(self.rows);
        let mut lines: Vec<String> = Vec::with_capacity(max_lines);
        let mut current_line = String::with_capacity(cols);
        let mut current_row: Option<usize> = None;

        for indexed in content.display_iter {
            let Ok(row) = usize::try_from(indexed.point.line.0) else {
                continue;
            };
            if row >= rows {
                continue;
            }
            if current_row != Some(row) {
                if !current_line.is_empty() {
                    lines.push(std::mem::take(&mut current_line));
                }
                current_row = Some(row);
                current_line.clear();
            }
            if indexed.cell.c != ' ' || indexed.cell.zerowidth().is_some() {
                while current_line.len() < indexed.point.column.0 {
                    current_line.push(' ');
                }
                current_line.push(indexed.cell.c);
            }
        }
        if !current_line.is_empty() {
            lines.push(current_line);
        }
        let start = lines.len().saturating_sub(max_lines);
        lines[start..].join("\n")
    }

    #[must_use]
    pub fn clickable_at_point(&self, row: usize, col: usize) -> Option<String> {
        let term = self.term.lock();
        let content = term.renderable_content();
        let cols = usize::from(self.cols);
        let mut line_chars: Vec<char> = vec![' '; cols];

        for indexed in content.display_iter {
            let Ok(r) = usize::try_from(indexed.point.line.0) else {
                continue;
            };
            if r != row {
                continue;
            }
            let c = indexed.point.column.0;
            if c < cols {
                line_chars[c] = indexed.cell.c;
            }
        }

        find_url_at_column(&line_chars, col).or_else(|| find_file_path_at_column(&line_chars, col))
    }
}

impl crate::terminal_emulator::TerminalEmulator for TmuxTerminal {
    fn write_input(&self, bytes: &[u8]) {
        self.write_input(bytes);
    }

    fn mode(&self) -> TermMode {
        self.mode()
    }

    fn cols(&self) -> u16 {
        self.cols()
    }

    fn rows(&self) -> u16 {
        self.rows()
    }

    fn scrollback(&self) -> usize {
        self.scrollback()
    }

    fn set_scrollback(&mut self, offset: usize) {
        self.set_scrollback(offset);
    }

    fn scroll_scrollback_by(&mut self, delta: i32) {
        self.scroll_scrollback_by(delta);
    }

    fn history_size(&self) -> usize {
        self.history_size()
    }

    fn child_exited(&self) -> bool {
        self.child_exited()
    }

    fn has_selection(&self) -> bool {
        self.has_selection()
    }

    fn start_selection(&self, sel_type: SelectionType, row: usize, col: usize, side: Side) {
        self.start_selection(sel_type, row, col, side);
    }

    fn update_selection(&self, row: usize, col: usize, side: Side) {
        self.update_selection(row, col, side);
    }

    fn clear_selection(&self) {
        self.clear_selection();
    }

    fn selection_to_string(&self) -> Option<String> {
        self.selection_to_string()
    }

    fn reset_damage(&self) {
        self.reset_damage();
    }

    fn clickable_at_point(&self, row: usize, col: usize) -> Option<String> {
        self.clickable_at_point(row, col)
    }

    fn last_lines_text(&self, max_lines: usize) -> String {
        self.last_lines_text(max_lines)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feed_output_parses_text() {
        let (sender, _receiver) = tmux_input_channel();
        let mut terminal = TmuxTerminal::new(
            &TmuxTerminalOptions {
                pane_id: PaneId(0),
                rows: 24,
                cols: 80,
                scrollback_limit: 100,
                kitty_keyboard: false,
            },
            sender,
        );

        assert!(terminal.feed_output(b"hello world"));
        let text = terminal.last_lines_text(1);
        assert!(text.contains("hello world"), "got: {text}");
    }

    #[test]
    fn write_input_buffered_for_tmux() {
        let (sender, receiver) = tmux_input_channel();
        let terminal = TmuxTerminal::new(
            &TmuxTerminalOptions {
                pane_id: PaneId(1),
                rows: 24,
                cols: 80,
                scrollback_limit: 100,
                kitty_keyboard: false,
            },
            sender,
        );

        terminal.write_input(b"ls\n");
        let chunks = receiver.drain();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0], b"ls\n");
    }

    #[test]
    fn resize_updates_dimensions() {
        let (sender, _receiver) = tmux_input_channel();
        let mut terminal = TmuxTerminal::new(
            &TmuxTerminalOptions {
                pane_id: PaneId(0),
                rows: 24,
                cols: 80,
                scrollback_limit: 100,
                kitty_keyboard: false,
            },
            sender,
        );

        terminal.resize(40, 120);
        assert_eq!(terminal.rows(), 40);
        assert_eq!(terminal.cols(), 120);
    }
}
