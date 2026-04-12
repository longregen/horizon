# Tmux + Ghostty Backend — Architecture Plan

## Summary

Replace Horizon's direct PTY management with **tmux control mode** as the
session backend, and replace **alacritty_terminal** with **libghostty** for
VT100 parsing. This turns Horizon into a GPU-accelerated tmux frontend where
sessions survive SSH disconnects and app crashes.

## Concept Mapping

| tmux concept | Horizon concept | Notes |
|---|---|---|
| tmux server | Background daemon | One per user, survives disconnects |
| tmux session | Workspace | Named, color-coded group of terminals |
| tmux window | Panel | A terminal on the canvas |
| tmux pane | (future) Split panels | Not in initial scope |
| Control mode (`-CC`) | Backend protocol | Structured event stream |

## Architecture

```
┌─────────────────────────────────────────────────────┐
│  Horizon UI (egui/wgpu)                             │
│  ┌────────────────────────────────────────────────┐ │
│  │  Canvas / Panels / Minimap / Sidebar           │ │
│  └────────────────────┬───────────────────────────┘ │
│                       │                             │
│  ┌────────────────────▼───────────────────────────┐ │
│  │  libghostty VT100 parser  (per panel)          │ │
│  │  Parses output → cell grid → render            │ │
│  └────────────────────┬───────────────────────────┘ │
│                       │                             │
│  ┌────────────────────▼───────────────────────────┐ │
│  │  TmuxBackend (horizon-core)                    │ │
│  │  - Connects via tmux -CC control mode          │ │
│  │  - Parses %output, %window-add, etc.           │ │
│  │  - Routes output to correct panel              │ │
│  │  - Forwards input keystrokes to tmux           │ │
│  └────────────────────┬───────────────────────────┘ │
│                       │                             │
└───────────────────────┼─────────────────────────────┘
                        │ stdin/stdout pipe
┌───────────────────────▼─────────────────────────────┐
│  tmux server                                        │
│  - Sessions (→ workspaces)                          │
│  - Windows  (→ panels)                              │
│  - Survives SSH disconnect / Horizon crash          │
└─────────────────────────────────────────────────────┘
```

## Phase 1 — tmux Control Mode Backend

### 1.1 New module: `horizon-core/src/tmux/`

```
tmux/
├── mod.rs           # TmuxBackend public API
├── control.rs       # Control mode protocol parser
├── session.rs       # Session/window/pane state tracking
└── connection.rs    # Spawning and managing tmux -CC process
```

**`TmuxBackend`** — singleton per Horizon instance:
- Spawns `tmux -CC new-session` (or `attach-session` if reconnecting)
- Reads stdout for control mode notifications
- Writes stdin for commands (`new-window`, `send-keys`, `resize-window`, etc.)

**Control mode protocol** (tmux sends these on stdout):
```
%begin <time> <flags>       — command output start
%end <time> <flags>         — command output end
%output %<pane-id> <data>   — terminal output from a pane
%window-add @<window-id>    — new window created
%window-close @<window-id>  — window closed
%session-changed $<id> <n>  — session switch
%exit                       — tmux server exiting
```

**Key API surface:**
```rust
pub struct TmuxBackend { /* ... */ }

impl TmuxBackend {
    /// Connect to existing tmux server or start a new one.
    pub fn connect(socket_name: Option<&str>) -> Result<Self>;

    /// Reattach to an existing server after Horizon restart.
    pub fn reattach(socket_name: &str) -> Result<Self>;

    /// Create a new tmux session (→ Horizon workspace).
    pub fn create_session(&mut self, name: &str) -> Result<TmuxSessionId>;

    /// Create a new window in a session (→ Horizon panel).
    pub fn create_window(
        &mut self,
        session: TmuxSessionId,
        command: Option<&str>,
        cwd: Option<&Path>,
    ) -> Result<TmuxWindowId>;

    /// Send keystrokes to a window.
    pub fn send_keys(&mut self, window: TmuxWindowId, keys: &[u8]) -> Result<()>;

    /// Resize a window.
    pub fn resize_window(&mut self, window: TmuxWindowId, cols: u16, rows: u16) -> Result<()>;

    /// Close a window.
    pub fn kill_window(&mut self, window: TmuxWindowId) -> Result<()>;

    /// Poll for events (non-blocking). Called each frame.
    pub fn poll_events(&mut self) -> Vec<TmuxEvent>;
}

pub enum TmuxEvent {
    Output { window: TmuxWindowId, data: Vec<u8> },
    WindowAdded { window: TmuxWindowId },
    WindowClosed { window: TmuxWindowId },
    SessionChanged { session: TmuxSessionId },
    Exited,
}
```

### 1.2 Adapt `Terminal` to accept tmux-routed bytes

Currently `Terminal::spawn()` creates its own PTY. Refactor into:

```rust
pub enum TerminalBackend {
    /// Direct PTY (current behavior, kept as fallback).
    Pty(PtyTerminal),
    /// tmux-backed: receives output bytes from TmuxBackend.
    Tmux(TmuxTerminal),
}
```

`TmuxTerminal` holds:
- The VT100 parser state (libghostty or alacritty_terminal)
- A channel receiver for output bytes (fed by `TmuxBackend`)
- The `TmuxWindowId` for sending input back

### 1.3 Adapt `Board` / `Panel`

- `Board` owns the `TmuxBackend` singleton
- `Board::create_panel()` calls `tmux_backend.create_window()` instead of
  `Terminal::spawn()`
- `Board::process_events()` calls `tmux_backend.poll_events()` and routes
  `Output` events to the correct panel's VT100 parser
- Panel close → `tmux_backend.kill_window()`
- Horizon quit → detach from tmux (sessions survive)

### 1.4 Session persistence

With tmux, persistence is mostly free:
- On Horizon quit: `tmux detach` — sessions keep running
- On Horizon start: `tmux list-sessions` → restore workspace/panel mapping
- Canvas positions/sizes still persisted in SQLite (tmux doesn't know about those)
- Transcript replay no longer needed (tmux has its own scrollback)

## Phase 2 — Replace alacritty_terminal with libghostty

### 2.1 Challenge

Ghostty's terminal emulator (`libghostty`) is written in **Zig** and is not
published as a standalone library with C/Rust bindings. Options:

**Option A: Zig FFI** (recommended if ghostty stabilizes their API)
- Build libghostty as a C-compatible shared library via `zig build`
- Write a `ghostty-sys` Rust crate with `bindgen` or manual FFI
- Wrap in safe `ghostty-terminal` crate matching our `Terminal` API

**Option B: Extract and rewrite in Rust** (not practical — too large)

**Option C: Use ghostty as a subprocess** (loses the benefit)

### 2.2 Integration surface

The VT100 parser interface we need is small:

```rust
trait TerminalEmulator {
    /// Feed raw bytes from the PTY/tmux.
    fn process_bytes(&mut self, bytes: &[u8]);

    /// Resize the virtual terminal.
    fn resize(&mut self, rows: u16, cols: u16);

    /// Iterate over the cell grid for rendering.
    fn renderable_content(&self) -> impl Iterator<Item = Cell>;

    /// Scroll position.
    fn scroll_display(&mut self, scroll: Scroll);

    /// Terminal mode flags.
    fn mode(&self) -> TermMode;

    /// Selection management.
    fn set_selection(&mut self, ...);
    fn selection_to_string(&self) -> Option<String>;
}
```

Currently this is tightly coupled to `alacritty_terminal::Term`. Phase 2 would:

1. Define a `TerminalEmulator` trait in `horizon-core`
2. Implement it for `alacritty_terminal::Term` (backward compat)
3. Implement it for libghostty via FFI
4. Make the implementation selectable at compile time (`cfg` feature) or runtime

### 2.3 Rendering adapter

`terminal_widget/render.rs` currently iterates `alacritty_terminal`'s
`RenderableContent` / `IndexedCell`. The trait abstraction from 2.2 would
provide a generic cell iterator, and `render_grid()` would work against that.

## Phase 3 — tmux Keybindings & UX

- Ctrl-b prefix key → intercept in `input/keyboard.rs`, route to tmux commands
- `Ctrl-b c` → new panel (tmux window)
- `Ctrl-b n/p` → next/prev panel
- `Ctrl-b d` → detach (close Horizon, tmux survives)
- `Ctrl-b :` → tmux command prompt (rendered in Horizon)
- Configurable prefix key in `config.yaml`
- Status bar showing tmux session info

## Migration Path

1. **Phase 1** can land incrementally — add `TmuxBackend` alongside current
   direct-PTY mode. Feature-flag it. Both modes work.
2. **Phase 2** is independent — can swap VT100 parser with or without tmux.
3. **Phase 3** is UX polish on top of Phase 1.

## Open Questions

- Should each Horizon workspace map 1:1 to a tmux session, or should there be
  a single tmux session with windows grouped by Horizon workspace metadata?
- Do we want tmux pane splits inside Horizon panels, or keep panels as the
  only unit?
- libghostty FFI feasibility — needs spike to assess build complexity and API
  stability.
