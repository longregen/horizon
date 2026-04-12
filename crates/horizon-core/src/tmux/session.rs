//! In-memory mirror of the tmux server's session/window/pane hierarchy.
//!
//! Updated by [`TmuxBackend`](super::TmuxBackend) as control-mode events
//! arrive.  The UI layer reads this state to map tmux objects to Horizon
//! workspaces and panels.

use std::collections::HashMap;

use super::control::{PaneId, SessionId, WindowId};

/// Top-level state of every tmux object the backend is aware of.
#[derive(Debug, Default)]
pub struct TmuxState {
    pub sessions: HashMap<SessionId, TmuxSession>,
    pub windows: HashMap<WindowId, TmuxWindow>,
    pub panes: HashMap<PaneId, TmuxPane>,

    /// The session that the control client is currently attached to.
    pub active_session: Option<SessionId>,
}

/// A tmux session — maps to a Horizon workspace.
#[derive(Clone, Debug)]
pub struct TmuxSession {
    pub id: SessionId,
    pub name: String,
    pub windows: Vec<WindowId>,
}

/// A tmux window — maps to a Horizon panel.
#[derive(Clone, Debug)]
pub struct TmuxWindow {
    pub id: WindowId,
    pub session_id: SessionId,
    pub name: String,
    pub panes: Vec<PaneId>,
    pub layout: String,
}

/// A tmux pane — the actual terminal inside a window.
#[derive(Clone, Debug)]
pub struct TmuxPane {
    pub id: PaneId,
    pub window_id: WindowId,
    pub cols: u16,
    pub rows: u16,
}

impl TmuxState {
    /// Register a new session.
    pub fn add_session(&mut self, id: SessionId, name: String) {
        self.sessions.insert(
            id,
            TmuxSession {
                id,
                name,
                windows: Vec::new(),
            },
        );
    }

    /// Register a new window under the current active session.
    pub fn add_window(&mut self, id: WindowId) {
        let session_id = self.active_session.unwrap_or(SessionId(0));
        let window = TmuxWindow {
            id,
            session_id,
            name: String::new(),
            panes: Vec::new(),
            layout: String::new(),
        };
        self.windows.insert(id, window);

        if let Some(session) = self.sessions.get_mut(&session_id)
            && !session.windows.contains(&id)
        {
            session.windows.push(id);
        }
    }

    /// Remove a window and its panes from state.
    pub fn remove_window(&mut self, id: WindowId) {
        if let Some(window) = self.windows.remove(&id) {
            for pane_id in &window.panes {
                self.panes.remove(pane_id);
            }
            if let Some(session) = self.sessions.get_mut(&window.session_id) {
                session.windows.retain(|w| *w != id);
            }
        }
    }

    /// Rename a window.
    pub fn rename_window(&mut self, id: WindowId, name: String) {
        if let Some(window) = self.windows.get_mut(&id) {
            window.name = name;
        }
    }

    /// Update the layout string for a window.
    pub fn set_window_layout(&mut self, id: WindowId, layout: String) {
        if let Some(window) = self.windows.get_mut(&id) {
            window.layout = layout;
        }
    }

    /// Switch the active session.
    pub fn set_active_session(&mut self, id: SessionId, name: String) {
        self.active_session = Some(id);
        if let Some(session) = self.sessions.get_mut(&id) {
            session.name = name;
        } else {
            self.add_session(id, name);
        }
    }

    /// Rename the active session.
    pub fn rename_active_session(&mut self, name: String) {
        if let Some(id) = self.active_session
            && let Some(session) = self.sessions.get_mut(&id)
        {
            session.name = name;
        }
    }

    /// Look up which panes belong to a given window.
    #[must_use]
    pub fn panes_for_window(&self, window_id: WindowId) -> Vec<PaneId> {
        self.windows
            .get(&window_id)
            .map(|w| w.panes.clone())
            .unwrap_or_default()
    }

    /// Look up all window IDs in a session.
    #[must_use]
    pub fn windows_for_session(&self, session_id: SessionId) -> Vec<WindowId> {
        self.sessions
            .get(&session_id)
            .map(|s| s.windows.clone())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_and_remove_window() {
        let mut state = TmuxState::default();
        state.add_session(SessionId(0), "main".to_string());
        state.active_session = Some(SessionId(0));

        state.add_window(WindowId(1));
        assert!(state.windows.contains_key(&WindowId(1)));
        assert_eq!(state.sessions[&SessionId(0)].windows, vec![WindowId(1)]);

        state.remove_window(WindowId(1));
        assert!(!state.windows.contains_key(&WindowId(1)));
        assert!(state.sessions[&SessionId(0)].windows.is_empty());
    }

    #[test]
    fn rename_window_updates_state() {
        let mut state = TmuxState::default();
        state.add_session(SessionId(0), "s0".to_string());
        state.active_session = Some(SessionId(0));
        state.add_window(WindowId(2));

        state.rename_window(WindowId(2), "vim".to_string());
        assert_eq!(state.windows[&WindowId(2)].name, "vim");
    }

    #[test]
    fn session_switch_creates_if_missing() {
        let mut state = TmuxState::default();
        state.set_active_session(SessionId(5), "dev".to_string());

        assert_eq!(state.active_session, Some(SessionId(5)));
        assert_eq!(state.sessions[&SessionId(5)].name, "dev");
    }
}
