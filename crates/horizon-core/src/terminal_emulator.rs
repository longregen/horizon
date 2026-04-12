//! Trait abstracting over terminal emulator backends.
//!
//! Both [`Terminal`](crate::Terminal) (direct PTY via `alacritty_terminal`) and
//! [`TmuxTerminal`](crate::tmux::terminal::TmuxTerminal) implement this trait
//! so the rendering and input layers can work generically without knowing which
//! backend is in use.
//!
//! When `libghostty` replaces `alacritty_terminal` in the future, only a new
//! implementor of this trait needs to be provided — the UI layer is unchanged.

use alacritty_terminal::index::Side;
use alacritty_terminal::selection::SelectionType;
use alacritty_terminal::term::TermMode;

/// Unified interface for terminal emulation backends.
///
/// Covers the operations the UI layer needs: rendering, input, scrollback,
/// selection, and mode queries.
pub trait TerminalEmulator {
    /// Send raw key/input bytes to the underlying shell process.
    fn write_input(&self, bytes: &[u8]);

    /// Current terminal mode flags (alt-screen, mouse reporting, etc.).
    fn mode(&self) -> TermMode;

    /// Number of visible columns.
    fn cols(&self) -> u16;

    /// Number of visible rows.
    fn rows(&self) -> u16;

    /// Current scrollback offset (0 = bottom).
    fn scrollback(&self) -> usize;

    /// Set the scrollback offset.
    fn set_scrollback(&mut self, offset: usize);

    /// Scroll scrollback by a relative delta.
    fn scroll_scrollback_by(&mut self, delta: i32);

    /// Total history lines above the viewport.
    fn history_size(&self) -> usize;

    /// Whether the child process has exited.
    fn child_exited(&self) -> bool;

    /// Whether a text selection is currently active.
    fn has_selection(&self) -> bool;

    /// Start a selection at the given viewport-relative row/col and side.
    fn start_selection(&self, sel_type: SelectionType, row: usize, col: usize, side: Side);

    /// Update the active selection endpoint.
    fn update_selection(&self, row: usize, col: usize, side: Side);

    /// Clear any active selection.
    fn clear_selection(&self);

    /// Extract the selected text, if any.
    fn selection_to_string(&self) -> Option<String>;

    /// Reset damage tracking after a render.
    fn reset_damage(&self);

    /// Return a clickable target (URL or file path) at the given cell.
    fn clickable_at_point(&self, row: usize, col: usize) -> Option<String>;

    /// Extract the last few non-empty lines for pattern matching.
    fn last_lines_text(&self, max_lines: usize) -> String;
}
