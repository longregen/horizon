//! Tmux prefix key interception for Horizon.
//!
//! When the tmux backend is active, this module intercepts the tmux prefix key
//! (default: Ctrl-b) and translates the following key into a tmux command
//! instead of sending it to the terminal.
//!
//! This gives users their familiar tmux keybindings while Horizon handles the
//! rendering and multiplexing.

use egui::{Key, Modifiers};

/// State machine for tmux prefix key interception.
pub struct TmuxPrefixState {
    /// Whether the prefix key has been pressed and we're waiting for the
    /// next key to form a tmux command.
    awaiting_suffix: bool,

    /// The prefix key. Default: Ctrl-b.
    prefix_key: Key,

    /// The prefix modifier. Default: Ctrl.
    prefix_modifiers: Modifiers,
}

impl Default for TmuxPrefixState {
    fn default() -> Self {
        Self::new()
    }
}

/// A tmux command produced by the prefix key handler.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TmuxKeyAction {
    /// Create a new window (Ctrl-b c).
    NewWindow,
    /// Go to next window (Ctrl-b n).
    NextWindow,
    /// Go to previous window (Ctrl-b p).
    PreviousWindow,
    /// Detach from tmux (Ctrl-b d).
    Detach,
    /// Rename current window (Ctrl-b ,).
    RenameWindow,
    /// Close current window (Ctrl-b &).
    CloseWindow,
    /// Select window by number (Ctrl-b 0-9).
    SelectWindow(u8),
    /// Show window list (Ctrl-b w).
    ListWindows,
    /// Send the prefix key itself (Ctrl-b Ctrl-b).
    SendPrefix,
    /// Unknown suffix — pass through.
    Unknown(Key),
}

impl TmuxPrefixState {
    /// Create a new prefix state with default Ctrl-b prefix.
    #[must_use]
    pub fn new() -> Self {
        Self {
            awaiting_suffix: false,
            prefix_key: Key::B,
            prefix_modifiers: Modifiers::CTRL,
        }
    }

    /// Whether we are currently awaiting the suffix key after the prefix.
    #[must_use]
    pub fn is_awaiting_suffix(&self) -> bool {
        self.awaiting_suffix
    }

    /// Process a key press event. Returns `Some(action)` if this key completes
    /// a tmux command, `None` if the key should be forwarded normally to the
    /// terminal.
    ///
    /// Returns `Some(action)` in two cases:
    /// 1. The prefix key is pressed → returns `None` but sets internal state
    /// 2. A suffix key is pressed while awaiting → returns the action
    ///
    /// The caller should check the return value: if `None`, forward the key
    /// event to the terminal as usual.
    pub fn on_key_press(&mut self, key: Key, modifiers: Modifiers) -> Option<TmuxKeyAction> {
        if self.awaiting_suffix {
            self.awaiting_suffix = false;
            return Some(self.translate_suffix(key, modifiers));
        }

        if key == self.prefix_key && modifiers == self.prefix_modifiers {
            self.awaiting_suffix = true;
            // Consume the prefix key — don't forward to terminal.
            return Some(TmuxKeyAction::SendPrefix); // Sentinel; caller should not forward.
        }

        // Not a prefix key — forward normally.
        None
    }

    /// Cancel the prefix state (e.g. on Escape or timeout).
    pub fn cancel(&mut self) {
        self.awaiting_suffix = false;
    }

    fn translate_suffix(&self, key: Key, modifiers: Modifiers) -> TmuxKeyAction {
        // Ctrl-b Ctrl-b → send literal Ctrl-b to the terminal.
        if key == self.prefix_key && modifiers == self.prefix_modifiers {
            return TmuxKeyAction::SendPrefix;
        }

        match key {
            Key::C if modifiers.is_none() => TmuxKeyAction::NewWindow,
            Key::N if modifiers.is_none() => TmuxKeyAction::NextWindow,
            Key::P if modifiers.is_none() => TmuxKeyAction::PreviousWindow,
            Key::D if modifiers.is_none() => TmuxKeyAction::Detach,
            Key::W if modifiers.is_none() => TmuxKeyAction::ListWindows,
            Key::Num0 if modifiers.is_none() => TmuxKeyAction::SelectWindow(0),
            Key::Num1 if modifiers.is_none() => TmuxKeyAction::SelectWindow(1),
            Key::Num2 if modifiers.is_none() => TmuxKeyAction::SelectWindow(2),
            Key::Num3 if modifiers.is_none() => TmuxKeyAction::SelectWindow(3),
            Key::Num4 if modifiers.is_none() => TmuxKeyAction::SelectWindow(4),
            Key::Num5 if modifiers.is_none() => TmuxKeyAction::SelectWindow(5),
            Key::Num6 if modifiers.is_none() => TmuxKeyAction::SelectWindow(6),
            Key::Num7 if modifiers.is_none() => TmuxKeyAction::SelectWindow(7),
            Key::Num8 if modifiers.is_none() => TmuxKeyAction::SelectWindow(8),
            Key::Num9 if modifiers.is_none() => TmuxKeyAction::SelectWindow(9),
            other => TmuxKeyAction::Unknown(other),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_then_c_creates_new_window() {
        let mut state = TmuxPrefixState::new();

        // Press Ctrl-b.
        let result = state.on_key_press(Key::B, Modifiers::CTRL);
        assert!(result.is_some()); // Prefix consumed.
        assert!(state.is_awaiting_suffix());

        // Press 'c'.
        let result = state.on_key_press(Key::C, Modifiers::NONE);
        assert_eq!(result, Some(TmuxKeyAction::NewWindow));
        assert!(!state.is_awaiting_suffix());
    }

    #[test]
    fn non_prefix_key_passes_through() {
        let mut state = TmuxPrefixState::new();

        let result = state.on_key_press(Key::A, Modifiers::NONE);
        assert!(result.is_none());
        assert!(!state.is_awaiting_suffix());
    }

    #[test]
    fn double_prefix_sends_prefix() {
        let mut state = TmuxPrefixState::new();

        state.on_key_press(Key::B, Modifiers::CTRL);
        let result = state.on_key_press(Key::B, Modifiers::CTRL);
        assert_eq!(result, Some(TmuxKeyAction::SendPrefix));
    }

    #[test]
    fn cancel_clears_awaiting_state() {
        let mut state = TmuxPrefixState::new();

        state.on_key_press(Key::B, Modifiers::CTRL);
        assert!(state.is_awaiting_suffix());

        state.cancel();
        assert!(!state.is_awaiting_suffix());
    }

    #[test]
    fn number_keys_select_windows() {
        let mut state = TmuxPrefixState::new();

        state.on_key_press(Key::B, Modifiers::CTRL);
        let result = state.on_key_press(Key::Num3, Modifiers::NONE);
        assert_eq!(result, Some(TmuxKeyAction::SelectWindow(3)));
    }
}
