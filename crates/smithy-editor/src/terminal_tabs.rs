//! Multi-terminal tab management
//!
//! Each tab owns an independent PTY session via `TerminalViewState`.

use crate::terminal_view::{SharedTerminalState, TerminalViewState};
use std::cell::RefCell;
use std::rc::Rc;

/// Unique identifier for a terminal tab
pub type TerminalId = usize;

/// A single terminal tab with its own PTY session
pub struct TerminalTab {
    pub id: TerminalId,
    pub label: String,
    pub state: SharedTerminalState,
}

/// Manages multiple terminal tabs
pub struct TerminalTabManager {
    tabs: Vec<TerminalTab>,
    active: Option<TerminalId>,
    next_id: TerminalId,
    /// Directory new tabs start in. `None` inherits the editor's own working
    /// directory, which is almost never what you want — set it to the project
    /// root so a new terminal opens where the code is.
    cwd: Option<std::path::PathBuf>,
}

impl TerminalTabManager {
    pub fn new() -> Self {
        Self {
            tabs: Vec::new(),
            active: None,
            next_id: 1,
            cwd: None,
        }
    }

    /// Set the directory new tabs open in. Existing tabs keep theirs.
    pub fn set_cwd(&mut self, cwd: impl Into<std::path::PathBuf>) {
        self.cwd = Some(cwd.into());
    }

    /// Create a new terminal tab with a default shell. Returns the tab ID.
    pub fn new_tab(&mut self) -> Result<TerminalId, crate::error::TerminalError> {
        let id = self.next_id;
        self.next_id += 1;

        let mut state = TerminalViewState::new();
        match &self.cwd {
            Some(dir) => state.spawn_default_shell_in(dir)?,
            None => state.spawn_default_shell()?,
        }
        let state = Rc::new(RefCell::new(state));

        let label = format!("Terminal {}", id);
        self.tabs.push(TerminalTab { id, label, state });
        self.active = Some(id);
        Ok(id)
    }

    /// Close and remove a tab. Returns true if the tab existed.
    pub fn close_tab(&mut self, id: TerminalId) -> bool {
        if let Some(idx) = self.tabs.iter().position(|t| t.id == id) {
            let tab = self.tabs.remove(idx);
            let _ = tab.state.borrow_mut().close();

            // Update active tab if we closed the active one
            if self.active == Some(id) {
                self.active = if self.tabs.is_empty() {
                    None
                } else {
                    // Pick the tab at the same index (or the last one)
                    let new_idx = idx.min(self.tabs.len() - 1);
                    Some(self.tabs[new_idx].id)
                };
            }
            true
        } else {
            false
        }
    }

    /// Get a reference to the active tab
    pub fn active_tab(&self) -> Option<&TerminalTab> {
        self.active
            .and_then(|id| self.tabs.iter().find(|t| t.id == id))
    }

    /// Get a mutable reference to the active tab
    pub fn active_tab_mut(&mut self) -> Option<&mut TerminalTab> {
        let active = self.active?;
        self.tabs.iter_mut().find(|t| t.id == active)
    }

    /// Switch to a different tab
    pub fn set_active(&mut self, id: TerminalId) {
        if self.tabs.iter().any(|t| t.id == id) {
            self.active = Some(id);
        }
    }

    /// Get the active tab ID
    pub fn active_id(&self) -> Option<TerminalId> {
        self.active
    }

    /// Get all tabs (for rendering the tab bar)
    pub fn tabs(&self) -> &[TerminalTab] {
        &self.tabs
    }

    /// Number of open tabs
    pub fn len(&self) -> usize {
        self.tabs.len()
    }

    /// Whether there are no tabs
    pub fn is_empty(&self) -> bool {
        self.tabs.is_empty()
    }
}

impl Default for TerminalTabManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_terminal_manager_has_no_tabs() {
        let mgr = TerminalTabManager::new();
        assert!(mgr.is_empty());
        assert_eq!(mgr.len(), 0);
        assert!(mgr.active_id().is_none());
    }

    // Note: new_tab() spawns a real PTY, so we test the close/management logic
    // by checking the structural invariants rather than spawning shells in CI.

    #[test]
    fn closing_a_tab_that_is_gone_is_not_an_error() {
        let mut mgr = TerminalTabManager::new();
        assert!(!mgr.close_tab(999));
    }
}
