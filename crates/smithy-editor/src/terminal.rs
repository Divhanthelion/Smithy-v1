//! Terminal module - Embedded terminal emulator for Smithy
//!
//! This module provides terminal emulation using portable-pty for cross-platform
//! PTY support and vte for ANSI escape sequence parsing.

use std::io::{Read, Write};
use std::ops::Range;
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread;

use portable_pty::{native_pty_system, CommandBuilder, PtyPair, PtySize};
use vte::{Params, Parser, Perform};

use crate::error::TerminalError;

/// Terminal cell attributes
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CellAttributes {
    /// Foreground color (ANSI color index or RGB)
    pub fg: TerminalColor,
    /// Background color (ANSI color index or RGB)
    pub bg: TerminalColor,
    /// Bold text
    pub bold: bool,
    /// Italic text
    pub italic: bool,
    /// Underlined text
    pub underline: bool,
    /// Inverse video (swap fg/bg)
    pub inverse: bool,
}

/// Terminal color representation
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum TerminalColor {
    /// Default terminal color
    #[default]
    Default,
    /// ANSI color index (0-255)
    Indexed(u8),
    /// RGB color
    Rgb(u8, u8, u8),
}

/// A single cell in the terminal grid
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TerminalCell {
    /// The character in this cell
    pub c: char,
    /// Cell attributes (colors, styles)
    pub attrs: CellAttributes,
}

impl TerminalCell {
    /// Create a new empty cell
    pub fn new() -> Self {
        Self {
            c: ' ',
            attrs: CellAttributes::default(),
        }
    }

    /// Create a cell with a character
    pub fn with_char(c: char) -> Self {
        Self {
            c,
            attrs: CellAttributes::default(),
        }
    }
}

/// Terminal grid containing all cells
#[derive(Debug)]
pub struct TerminalGrid {
    /// Grid cells stored row-major
    cells: Vec<TerminalCell>,
    /// Number of columns
    cols: usize,
    /// Number of rows
    rows: usize,
    /// Cursor column position (0-indexed)
    cursor_col: usize,
    /// Cursor row position (0-indexed)
    cursor_row: usize,
    /// Current cell attributes for new characters
    current_attrs: CellAttributes,
    /// Deferred wrap: set when a character lands in the last column. The
    /// cursor stays put, and the wrap to the next line only happens if the
    /// *next* thing to arrive is another printable character — a CR, LF, or
    /// cursor-movement sequence cancels it. Real terminals work this way, and
    /// zsh's PROMPT_SP partial-line marker (the stray `%`) is tuned to it: it
    /// pads to exactly the terminal width and then carriage-returns, which an
    /// eager wrap turns into a stranded `%` on a line of its own.
    wrap_pending: bool,
    /// Lines that have scrolled off the top, oldest first.
    ///
    /// Without this the grid is a fixed window onto the last `rows` lines and
    /// everything above is discarded — so there is nothing for a scrollbar to
    /// reach, however the view is wrapped. Capped, because a long-running
    /// process will otherwise fill memory with output nobody will read.
    scrollback: std::collections::VecDeque<Vec<TerminalCell>>,
}

/// How many scrolled-off lines to retain.
const SCROLLBACK_LIMIT: usize = 5_000;

/// The part of a terminal grid needed to render one viewport.
///
/// Rows are absolute across scrollback and the live grid. Keeping the absolute
/// start beside them prevents a caller from accidentally painting a viewport
/// snapshot at row zero.
#[derive(Clone, Debug)]
pub struct TerminalSnapshot {
    pub start_row: usize,
    pub rows: Vec<Vec<TerminalCell>>,
    pub total_rows: usize,
    pub cols: usize,
    pub cursor: (usize, usize),
}

impl TerminalGrid {
    /// Create a new terminal grid with the given dimensions
    pub fn new(cols: usize, rows: usize) -> Self {
        let cells = vec![TerminalCell::new(); cols * rows];
        Self {
            cells,
            cols,
            rows,
            cursor_col: 0,
            cursor_row: 0,
            current_attrs: CellAttributes::default(),
            wrap_pending: false,
            scrollback: std::collections::VecDeque::new(),
        }
    }

    /// Lines that have scrolled off the top, oldest first.
    #[cfg(test)]
    pub fn scrollback(&self) -> &std::collections::VecDeque<Vec<TerminalCell>> {
        &self.scrollback
    }

    /// Scrolled-off lines plus the live grid.
    pub fn total_rows(&self) -> usize {
        self.scrollback.len() + self.rows
    }

    /// Clone only the requested absolute rows plus the metadata needed to
    /// position them.
    pub fn snapshot_rows(&self, range: Range<usize>) -> TerminalSnapshot {
        let total_rows = self.total_rows();
        let start = range.start.min(total_rows);
        let end = range.end.min(total_rows).max(start);
        let scrollback_len = self.scrollback.len();
        let mut rows = Vec::with_capacity(end - start);
        for row in start..end {
            if row < scrollback_len {
                rows.push(self.scrollback[row].clone());
            } else if let Some(cells) = self.get_row(row - scrollback_len) {
                rows.push(cells.to_vec());
            }
        }
        TerminalSnapshot {
            start_row: start,
            rows,
            total_rows,
            cols: self.cols,
            cursor: (self.cursor_col, scrollback_len + self.cursor_row),
        }
    }

    /// Get the number of columns
    pub fn cols(&self) -> usize {
        self.cols
    }

    /// Get the number of rows
    pub fn rows(&self) -> usize {
        self.rows
    }

    /// Get the cursor position (col, row)
    #[cfg(test)]
    pub fn cursor_position(&self) -> (usize, usize) {
        (self.cursor_col, self.cursor_row)
    }

    /// Get a cell at the given position
    #[cfg(test)]
    pub fn get_cell(&self, col: usize, row: usize) -> Option<&TerminalCell> {
        if col < self.cols && row < self.rows {
            Some(&self.cells[row * self.cols + col])
        } else {
            None
        }
    }

    /// Get a row of cells
    pub fn get_row(&self, row: usize) -> Option<&[TerminalCell]> {
        if row < self.rows {
            let start = row * self.cols;
            Some(&self.cells[start..start + self.cols])
        } else {
            None
        }
    }

    /// Set a character at the cursor position and advance cursor
    fn put_char(&mut self, c: char) {
        // A printable character is what triggers a pending wrap — nothing
        // else does.
        if self.wrap_pending {
            self.wrap_pending = false;
            self.cursor_col = 0;
            self.line_feed();
        }
        if self.cursor_col < self.cols && self.cursor_row < self.rows {
            let idx = self.cursor_row * self.cols + self.cursor_col;
            self.cells[idx].c = c;
            self.cells[idx].attrs = self.current_attrs;

            // In the last column, defer the wrap rather than wrapping now.
            if self.cursor_col + 1 >= self.cols {
                self.wrap_pending = true;
            } else {
                self.cursor_col += 1;
            }
        }
    }

    /// Move cursor to a new position
    fn move_cursor(&mut self, col: usize, row: usize) {
        self.cursor_col = col.min(self.cols.saturating_sub(1));
        self.cursor_row = row.min(self.rows.saturating_sub(1));
        self.wrap_pending = false;
    }

    /// Scroll the grid up by one line
    fn scroll_up(&mut self) {
        // Keep the line about to be lost.
        let departing: Vec<TerminalCell> = self.cells[0..self.cols].to_vec();
        self.scrollback.push_back(departing);
        while self.scrollback.len() > SCROLLBACK_LIMIT {
            self.scrollback.pop_front();
        }

        // Move all rows up by one
        for row in 1..self.rows {
            let src_start = row * self.cols;
            let dst_start = (row - 1) * self.cols;
            for col in 0..self.cols {
                self.cells[dst_start + col] = self.cells[src_start + col].clone();
            }
        }
        // Clear the last row
        let last_row_start = (self.rows - 1) * self.cols;
        for col in 0..self.cols {
            self.cells[last_row_start + col] = TerminalCell::new();
        }
    }

    /// Clear the entire grid
    fn clear(&mut self) {
        for cell in &mut self.cells {
            *cell = TerminalCell::new();
        }
        self.cursor_col = 0;
        self.cursor_row = 0;
        self.wrap_pending = false;
    }

    /// Clear from cursor to end of line
    fn clear_to_eol(&mut self) {
        for col in self.cursor_col..self.cols {
            let idx = self.cursor_row * self.cols + col;
            self.cells[idx] = TerminalCell::new();
        }
    }

    /// Clear from cursor to end of screen
    fn clear_to_eos(&mut self) {
        // Clear rest of current line
        self.clear_to_eol();
        // Clear all lines below
        for row in (self.cursor_row + 1)..self.rows {
            for col in 0..self.cols {
                let idx = row * self.cols + col;
                self.cells[idx] = TerminalCell::new();
            }
        }
    }

    /// Handle carriage return
    fn carriage_return(&mut self) {
        self.cursor_col = 0;
        self.wrap_pending = false;
    }

    /// Handle line feed
    fn line_feed(&mut self) {
        self.wrap_pending = false;
        self.cursor_row += 1;
        if self.cursor_row >= self.rows {
            self.scroll_up();
            self.cursor_row = self.rows - 1;
        }
    }

    /// Handle backspace
    fn backspace(&mut self) {
        self.wrap_pending = false;
        if self.cursor_col > 0 {
            self.cursor_col -= 1;
        }
    }

    /// Resize the grid
    pub fn resize(&mut self, new_cols: usize, new_rows: usize) {
        let mut new_cells = vec![TerminalCell::new(); new_cols * new_rows];

        // Copy existing content
        let copy_cols = self.cols.min(new_cols);
        let copy_rows = self.rows.min(new_rows);

        for row in 0..copy_rows {
            for col in 0..copy_cols {
                let old_idx = row * self.cols + col;
                let new_idx = row * new_cols + col;
                new_cells[new_idx] = self.cells[old_idx].clone();
            }
        }

        self.cells = new_cells;
        self.cols = new_cols;
        self.rows = new_rows;

        // Clamp cursor position
        self.cursor_col = self.cursor_col.min(new_cols.saturating_sub(1));
        self.cursor_row = self.cursor_row.min(new_rows.saturating_sub(1));
        self.wrap_pending = false;
    }
}

/// VTE performer that updates the terminal grid
struct TerminalPerformer<G> {
    grid: G,
}

#[cfg(test)]
struct OwnedGrid(TerminalGrid);

#[cfg(test)]
impl Deref for OwnedGrid {
    type Target = TerminalGrid;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[cfg(test)]
impl DerefMut for OwnedGrid {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

#[cfg(test)]
impl TerminalPerformer<OwnedGrid> {
    fn new(cols: usize, rows: usize) -> Self {
        Self {
            grid: OwnedGrid(TerminalGrid::new(cols, rows)),
        }
    }
}

impl<G> TerminalPerformer<G> {
    fn from_grid(grid: G) -> Self {
        Self { grid }
    }
}

impl<G> Perform for TerminalPerformer<G>
where
    G: Deref<Target = TerminalGrid> + DerefMut,
{
    fn print(&mut self, c: char) {
        self.grid.put_char(c);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            // Carriage return
            0x0D => self.grid.carriage_return(),
            // Line feed / New line
            0x0A..=0x0C => self.grid.line_feed(),
            // Backspace
            0x08 => self.grid.backspace(),
            // Tab
            0x09 => {
                // Move to next tab stop (every 8 columns)
                let next_tab = ((self.grid.cursor_col / 8) + 1) * 8;
                self.grid.cursor_col = next_tab.min(self.grid.cols.saturating_sub(1));
                self.grid.wrap_pending = false;
            }
            // Bell - ignore
            0x07 => {}
            _ => {}
        }
    }

    fn hook(&mut self, _params: &Params, _intermediates: &[u8], _ignore: bool, _action: char) {
        // DCS sequences - not implemented
    }

    fn put(&mut self, _byte: u8) {
        // DCS data - not implemented
    }

    fn unhook(&mut self) {
        // End DCS - not implemented
    }

    fn osc_dispatch(&mut self, _params: &[&[u8]], _bell_terminated: bool) {
        // OSC sequences (e.g., window title) - not implemented
    }

    fn csi_dispatch(
        &mut self,
        params: &Params,
        _intermediates: &[u8],
        _ignore: bool,
        action: char,
    ) {
        let params: Vec<u16> = params
            .iter()
            .map(|p| p.first().copied().unwrap_or(0))
            .collect();

        // Any control sequence cancels a pending wrap.
        self.grid.wrap_pending = false;

        match action {
            // Cursor Up
            'A' => {
                let n = params.first().copied().unwrap_or(1).max(1) as usize;
                self.grid.cursor_row = self.grid.cursor_row.saturating_sub(n);
            }
            // Cursor Down
            'B' => {
                let n = params.first().copied().unwrap_or(1).max(1) as usize;
                self.grid.cursor_row =
                    (self.grid.cursor_row + n).min(self.grid.rows.saturating_sub(1));
            }
            // Cursor Forward
            'C' => {
                let n = params.first().copied().unwrap_or(1).max(1) as usize;
                self.grid.cursor_col =
                    (self.grid.cursor_col + n).min(self.grid.cols.saturating_sub(1));
            }
            // Cursor Back
            'D' => {
                let n = params.first().copied().unwrap_or(1).max(1) as usize;
                self.grid.cursor_col = self.grid.cursor_col.saturating_sub(n);
            }
            // Cursor Position (CUP)
            'H' | 'f' => {
                let row = params.first().copied().unwrap_or(1).max(1) as usize - 1;
                let col = params.get(1).copied().unwrap_or(1).max(1) as usize - 1;
                self.grid.move_cursor(col, row);
            }
            // Erase in Display
            'J' => {
                let mode = params.first().copied().unwrap_or(0);
                match mode {
                    0 => self.grid.clear_to_eos(),
                    1 => {
                        // Clear from start to cursor - simplified
                        for row in 0..self.grid.cursor_row {
                            for col in 0..self.grid.cols {
                                let idx = row * self.grid.cols + col;
                                self.grid.cells[idx] = TerminalCell::new();
                            }
                        }
                        for col in 0..=self.grid.cursor_col {
                            let idx = self.grid.cursor_row * self.grid.cols + col;
                            self.grid.cells[idx] = TerminalCell::new();
                        }
                    }
                    2 | 3 => self.grid.clear(),
                    _ => {}
                }
            }
            // Erase in Line
            'K' => {
                let mode = params.first().copied().unwrap_or(0);
                match mode {
                    0 => self.grid.clear_to_eol(),
                    1 => {
                        // Clear from start of line to cursor
                        for col in 0..=self.grid.cursor_col {
                            let idx = self.grid.cursor_row * self.grid.cols + col;
                            self.grid.cells[idx] = TerminalCell::new();
                        }
                    }
                    2 => {
                        // Clear entire line
                        for col in 0..self.grid.cols {
                            let idx = self.grid.cursor_row * self.grid.cols + col;
                            self.grid.cells[idx] = TerminalCell::new();
                        }
                    }
                    _ => {}
                }
            }
            // Select Graphic Rendition (SGR) - colors and attributes
            'm' => {
                if params.is_empty() {
                    // Reset all attributes
                    self.grid.current_attrs = CellAttributes::default();
                } else {
                    self.handle_sgr(&params);
                }
            }
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, _intermediates: &[u8], _ignore: bool, _byte: u8) {
        // ESC sequences - not implemented
    }
}

impl<G> TerminalPerformer<G>
where
    G: Deref<Target = TerminalGrid> + DerefMut,
{
    /// Handle SGR (Select Graphic Rendition) parameters
    fn handle_sgr(&mut self, params: &[u16]) {
        let mut i = 0;
        while i < params.len() {
            match params[i] {
                0 => self.grid.current_attrs = CellAttributes::default(),
                1 => self.grid.current_attrs.bold = true,
                3 => self.grid.current_attrs.italic = true,
                4 => self.grid.current_attrs.underline = true,
                7 => self.grid.current_attrs.inverse = true,
                22 => self.grid.current_attrs.bold = false,
                23 => self.grid.current_attrs.italic = false,
                24 => self.grid.current_attrs.underline = false,
                27 => self.grid.current_attrs.inverse = false,
                // Foreground colors (30-37, 90-97)
                30..=37 => {
                    self.grid.current_attrs.fg = TerminalColor::Indexed((params[i] - 30) as u8)
                }
                38 => {
                    // Extended foreground color
                    if i + 2 < params.len() && params[i + 1] == 5 {
                        self.grid.current_attrs.fg = TerminalColor::Indexed(params[i + 2] as u8);
                        i += 2;
                    } else if i + 4 < params.len() && params[i + 1] == 2 {
                        self.grid.current_attrs.fg = TerminalColor::Rgb(
                            params[i + 2] as u8,
                            params[i + 3] as u8,
                            params[i + 4] as u8,
                        );
                        i += 4;
                    }
                }
                39 => self.grid.current_attrs.fg = TerminalColor::Default,
                // Background colors (40-47, 100-107)
                40..=47 => {
                    self.grid.current_attrs.bg = TerminalColor::Indexed((params[i] - 40) as u8)
                }
                48 => {
                    // Extended background color
                    if i + 2 < params.len() && params[i + 1] == 5 {
                        self.grid.current_attrs.bg = TerminalColor::Indexed(params[i + 2] as u8);
                        i += 2;
                    } else if i + 4 < params.len() && params[i + 1] == 2 {
                        self.grid.current_attrs.bg = TerminalColor::Rgb(
                            params[i + 2] as u8,
                            params[i + 3] as u8,
                            params[i + 4] as u8,
                        );
                        i += 4;
                    }
                }
                49 => self.grid.current_attrs.bg = TerminalColor::Default,
                90..=97 => {
                    self.grid.current_attrs.fg = TerminalColor::Indexed((params[i] - 90 + 8) as u8)
                }
                100..=107 => {
                    self.grid.current_attrs.bg = TerminalColor::Indexed((params[i] - 100 + 8) as u8)
                }
                _ => {}
            }
            i += 1;
        }
    }
}

/// Events emitted by the terminal
#[derive(Clone, Debug)]
pub enum TerminalEvent {
    /// At least one PTY read completed. The revision is monotonic, but wakes
    /// are coalesced so a stalled UI retains one small value, never the bytes.
    Activity(u64),
    /// Terminal process exited with status code
    Exit(i32),
    /// Bell character received
    Bell,
}

/// Terminal manager that handles PTY spawning and I/O
pub struct TerminalManager {
    /// The PTY pair (master/slave)
    pty_pair: PtyPair,
    /// Writer to send data to the PTY
    writer: Box<dyn Write + Send>,
    /// Terminal grid state
    grid: Arc<Mutex<TerminalGrid>>,
    /// One queued activity wake is enough: the grid already contains the
    /// newest state and the revision says whether anything changed.
    activity_rx: Receiver<u64>,
    activity_revision: Arc<AtomicU64>,
    /// Exit and bell coalesce independently, so a bell storm cannot crowd an
    /// exit out of a queue while the UI is not polling.
    lifecycle: Arc<Mutex<LifecycleState>>,
    /// Whether the terminal is closed
    closed: bool,
    /// Reader thread handle
    _reader_thread: Option<thread::JoinHandle<()>>,
    /// The shell process.
    ///
    /// Retained so it can be **killed**. It used to be bound to `_child` and
    /// dropped at the end of `spawn`, which left no way to signal it at all:
    /// `portable_pty`'s `Child` does not kill on drop. Closing the master
    /// normally sends SIGHUP and most shells exit, but a pty session leader that
    /// has called `setsid` and ignores SIGHUP would outlive the app with no
    /// handle left to stop it — which is the one process leak `kill_on_drop` and
    /// the foreground process group do not cover between them.
    child: ShellHandle,
}

#[derive(Default)]
struct LifecycleState {
    exit: Option<i32>,
    bell: bool,
}

fn publish_activity(tx: &SyncSender<u64>, revision: &AtomicU64) {
    let revision = revision.fetch_add(1, Ordering::Release) + 1;
    // A full channel already represents the same fact: there is unread
    // activity. Never wait behind the UI.
    let _ = tx.try_send(revision);
}

/// Change parser geometry before notifying the PTY.
///
/// Redraw bytes can arrive immediately after SIGWINCH. Updating the shared
/// parser/view grid first means every byte parsed from that point onward sees
/// the new geometry. The lock is deliberately released before `notify`: a PTY
/// backend is an external call and must never block while excluding the reader
/// thread.
fn resize_before_notify<E>(
    grid: &Arc<Mutex<TerminalGrid>>,
    cols: usize,
    rows: usize,
    notify: impl FnOnce() -> Result<(), E>,
) -> Result<(), E> {
    {
        let mut grid = grid.lock().unwrap();
        grid.resize(cols, rows);
    }
    notify()
}

/// A shell process, shared so a signal handler can reach it.
///
/// `Send + Sync` and behind an `Arc` specifically so [`kill_all_shells`] can be
/// called from anywhere — the terminal tabs themselves live in an `Rc<RefCell<_>>`
/// on the UI thread and cannot be touched from a signal task.
type ShellHandle = Arc<Mutex<Option<Box<dyn portable_pty::Child + Send + Sync>>>>;

/// Every shell Smithy has spawned and not yet reaped.
///
/// This exists for one case that nothing else covers. `Ctrl-C` sends SIGINT to the
/// foreground *process group*, and a pty child is deliberately not in it — putting
/// the child in its own session is what a pty is for. So the shell does not get the
/// signal, the app dies without running `Drop`, and the shell is reparented to
/// `launchd`. Neither `kill_on_drop` (which covers the language server) nor the
/// process group (which covers everything else) reaches it.
static LIVE_SHELLS: std::sync::OnceLock<Mutex<Vec<ShellHandle>>> = std::sync::OnceLock::new();

fn live_shells() -> &'static Mutex<Vec<ShellHandle>> {
    LIVE_SHELLS.get_or_init(|| Mutex::new(Vec::new()))
}

/// Kill every shell this process has spawned.
///
/// Idempotent, and safe to call from a signal handler or an exit path. Each handle
/// is emptied as it is reaped, so a later `close` or `Drop` finds nothing to do.
pub fn kill_all_shells() {
    let handles: Vec<ShellHandle> = match live_shells().lock() {
        Ok(mut live) => live.drain(..).collect(),
        Err(_) => return,
    };
    for handle in handles {
        let child = handle.lock().ok().and_then(|mut slot| slot.take());
        if let Some(mut child) = child {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Kill the shell when the manager goes away.
///
/// `close` is the explicit path and the app calls it on exit, but a manager
/// dropped any other way — a panic, a tab torn down, a future refactor — must not
/// leak its shell. `portable_pty`'s `Child` has no kill-on-drop of its own.
impl Drop for TerminalManager {
    fn drop(&mut self) {
        self.reap();
    }
}

impl TerminalManager {
    /// Spawn a new terminal with the given shell
    ///
    /// # Arguments
    /// * `shell` - Path to the shell executable (e.g., "/bin/bash" or "cmd.exe")
    ///
    /// # Returns
    /// * `Ok(TerminalManager)` on success
    /// * `Err(TerminalError)` if spawning fails
    pub fn spawn(shell: &str) -> Result<Self, TerminalError> {
        Self::spawn_with_size(shell, 80, 24, None)
    }

    /// Spawn a shell whose working directory is `cwd`.
    ///
    /// Without this the PTY inherits the *editor's* working directory — which
    /// is wherever the binary happened to be launched from, typically your home
    /// directory — so the terminal opened somewhere unrelated to the project.
    pub fn spawn_in(shell: &str, cwd: &std::path::Path) -> Result<Self, TerminalError> {
        Self::spawn_with_size(shell, 80, 24, Some(cwd))
    }

    /// Spawn a new terminal with the given shell and size
    ///
    /// # Arguments
    /// * `shell` - Path to the shell executable
    /// * `cols` - Number of columns
    /// * `rows` - Number of rows
    ///
    /// # Returns
    /// * `Ok(TerminalManager)` on success
    /// * `Err(TerminalError)` if spawning fails
    pub fn spawn_with_size(
        shell: &str,
        cols: u16,
        rows: u16,
        cwd: Option<&std::path::Path>,
    ) -> Result<Self, TerminalError> {
        let pty_system = native_pty_system();

        let pty_pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| TerminalError::PtySpawnFailed(e.to_string()))?;

        let mut cmd = CommandBuilder::new(shell);
        if let Some(dir) = cwd {
            cmd.cwd(dir);
        }

        // Set up environment
        #[cfg(windows)]
        {
            cmd.env("TERM", "xterm-256color");
        }
        #[cfg(not(windows))]
        {
            cmd.env("TERM", "xterm-256color");
            cmd.env("COLORTERM", "truecolor");
        }

        let child: ShellHandle = Arc::new(Mutex::new(Some(
            pty_pair
                .slave
                .spawn_command(cmd)
                .map_err(|e| TerminalError::PtySpawnFailed(e.to_string()))?,
        )));
        if let Ok(mut live) = live_shells().lock() {
            live.push(child.clone());
        }

        let writer = pty_pair
            .master
            .take_writer()
            .map_err(|e| TerminalError::PtySpawnFailed(e.to_string()))?;

        let mut reader = pty_pair
            .master
            .try_clone_reader()
            .map_err(|e| TerminalError::PtySpawnFailed(e.to_string()))?;

        let (activity_tx, activity_rx) = mpsc::sync_channel(1);
        let activity_revision = Arc::new(AtomicU64::new(0));
        let lifecycle = Arc::new(Mutex::new(LifecycleState::default()));
        let grid = Arc::new(Mutex::new(TerminalGrid::new(cols as usize, rows as usize)));

        // Clone for the reader thread
        let grid_clone = Arc::clone(&grid);
        let revision_clone = Arc::clone(&activity_revision);
        let lifecycle_clone = Arc::clone(&lifecycle);

        // Spawn reader thread
        let reader_thread = thread::spawn(move || {
            let mut buf = [0u8; 4096];
            let mut parser = Parser::new();

            loop {
                match reader.read(&mut buf) {
                    Ok(0) => {
                        // EOF - terminal closed
                        if let Ok(mut state) = lifecycle_clone.lock() {
                            state.exit = Some(0);
                        }
                        break;
                    }
                    Ok(n) => {
                        // Parsing and resize mutate this same grid. The mutex
                        // is held for one 4 KiB parser batch, not for the
                        // blocking PTY read or the UI's text layout work.
                        if let Ok(mut grid) = grid_clone.lock() {
                            let mut performer = TerminalPerformer::from_grid(&mut *grid);
                            parser.advance(&mut performer, &buf[..n]);
                        }

                        if buf[..n].contains(&0x07) {
                            if let Ok(mut state) = lifecycle_clone.lock() {
                                state.bell = true;
                            }
                        }
                        publish_activity(&activity_tx, &revision_clone);
                    }
                    Err(_) => {
                        if let Ok(mut state) = lifecycle_clone.lock() {
                            state.exit = Some(-1);
                        }
                        break;
                    }
                }
            }
        });

        Ok(Self {
            pty_pair,
            writer,
            grid,
            activity_rx,
            activity_revision,
            lifecycle,
            closed: false,
            _reader_thread: Some(reader_thread),
            child,
        })
    }

    /// Write data to the terminal (send keystrokes)
    ///
    /// # Arguments
    /// * `data` - Bytes to write to the PTY
    ///
    /// # Returns
    /// * `Ok(())` on success
    /// * `Err(TerminalError)` if writing fails
    pub fn write(&mut self, data: &[u8]) -> Result<(), TerminalError> {
        if self.closed {
            return Err(TerminalError::AlreadyClosed);
        }
        self.writer.write_all(data)?;
        self.writer.flush()?;
        Ok(())
    }

    /// Write a string to the terminal
    pub fn write_str(&mut self, s: &str) -> Result<(), TerminalError> {
        self.write(s.as_bytes())
    }

    /// Resize the terminal
    ///
    /// # Arguments
    /// * `cols` - New number of columns
    /// * `rows` - New number of rows
    ///
    /// # Returns
    /// * `Ok(())` on success
    /// * `Err(TerminalError)` if resizing fails
    pub fn resize(&mut self, cols: u16, rows: u16) -> Result<(), TerminalError> {
        if self.closed {
            return Err(TerminalError::AlreadyClosed);
        }

        // Grid first, PTY second. SIGWINCH can trigger output synchronously
        // from the shell's point of view, so notifying first permits those
        // bytes to race ahead and parse with the old width.
        resize_before_notify(&self.grid, cols as usize, rows as usize, || {
            self.pty_pair.master.resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| TerminalError::PtySpawnFailed(e.to_string()))
        })
    }

    /// Close the terminal, killing the shell.
    ///
    /// Previously this set a flag and left a comment claiming "the PTY will be
    /// closed when dropped". Setting a `bool` drops nothing, and the child handle
    /// had already been discarded at spawn, so there was no way to end the shell
    /// at all — the process survived until the master happened to close and the
    /// shell happened to honour SIGHUP.
    ///
    /// `kill` then `wait`: the wait is what reaps the zombie, and skipping it
    /// leaves a defunct entry per terminal for the life of the app.
    pub fn close(&mut self) -> Result<(), TerminalError> {
        if self.closed {
            return Err(TerminalError::AlreadyClosed);
        }
        self.closed = true;
        self.reap();
        Ok(())
    }

    /// Kill and reap this terminal's shell, and stop tracking it.
    fn reap(&mut self) {
        self.reap_child(true);
    }

    /// Take sole ownership of the child, untrack it, then wait exactly once.
    ///
    /// Taking the `Option` is the synchronization point shared with
    /// [`kill_all_shells`]: exactly one path receives the child and therefore
    /// exactly one path calls `wait`. Neither the child lock nor the global
    /// registry lock is held during `kill`/`wait`.
    fn reap_child(&mut self, kill_first: bool) {
        let child = self.child.lock().ok().and_then(|mut slot| slot.take());
        if let Ok(mut live) = live_shells().lock() {
            live.retain(|h| !Arc::ptr_eq(h, &self.child));
        }
        if let Some(mut child) = child {
            if kill_first {
                let _ = child.kill();
            }
            let _ = child.wait();
        }
    }

    /// Reap a child that has already exited, retaining its final grid.
    fn observe_exit(&mut self) {
        self.closed = true;
        self.reap_child(false);
    }

    /// Check if the terminal is closed
    pub fn is_closed(&self) -> bool {
        self.closed
    }

    /// Clone only the absolute rows needed by a viewport.
    pub fn snapshot_rows(&self, range: Range<usize>) -> TerminalSnapshot {
        self.grid.lock().unwrap().snapshot_rows(range)
    }

    /// Get the terminal dimensions (cols, rows)
    pub fn size(&self) -> (usize, usize) {
        let grid = self.grid.lock().unwrap();
        (grid.cols(), grid.rows())
    }

    /// Try to receive a terminal event (non-blocking)
    pub fn try_recv_event(&mut self) -> Option<TerminalEvent> {
        let lifecycle_event = if let Ok(mut lifecycle) = self.lifecycle.lock() {
            if let Some(code) = lifecycle.exit.take() {
                Some(TerminalEvent::Exit(code))
            } else if lifecycle.bell {
                lifecycle.bell = false;
                Some(TerminalEvent::Bell)
            } else {
                None
            }
        } else {
            None
        };
        if let Some(event) = lifecycle_event {
            if matches!(event, TerminalEvent::Exit(_)) {
                self.observe_exit();
            }
            return Some(event);
        }
        self.activity_rx.try_recv().ok().map(|_| {
            TerminalEvent::Activity(self.activity_revision.load(Ordering::Acquire))
        })
    }

    /// Send a key press to the terminal
    ///
    /// Converts common key events to their terminal escape sequences.
    ///
    /// # Arguments
    /// * `key` - The key that was pressed
    /// * `ctrl` - Whether Ctrl was held
    /// * `alt` - Whether Alt was held
    ///
    /// # Returns
    /// * `Ok(())` on success
    /// * `Err(TerminalError)` if writing fails
    pub fn send_key(
        &mut self,
        key: TerminalKey,
        ctrl: bool,
        alt: bool,
    ) -> Result<(), TerminalError> {
        let bytes = key_to_escape_sequence(key, ctrl, alt);
        self.write(&bytes)
    }

    /// Send a character to the terminal
    pub fn send_char(&mut self, c: char) -> Result<(), TerminalError> {
        let mut buf = [0u8; 4];
        let s = c.encode_utf8(&mut buf);
        self.write(s.as_bytes())
    }
}

/// Terminal key types for input handling
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TerminalKey {
    /// Regular character
    Char(char),
    /// Enter/Return key
    Enter,
    /// Backspace key
    Backspace,
    /// Tab key
    Tab,
    /// Escape key
    Escape,
    /// Arrow up
    Up,
    /// Arrow down
    Down,
    /// Arrow left
    Left,
    /// Arrow right
    Right,
    /// Home key
    Home,
    /// End key
    End,
    /// Page up
    PageUp,
    /// Page down
    PageDown,
    /// Insert key
    Insert,
    /// Delete key
    Delete,
    /// Function key (F1-F12)
    F(u8),
}

/// Convert a terminal key to its escape sequence
pub fn key_to_escape_sequence(key: TerminalKey, ctrl: bool, alt: bool) -> Vec<u8> {
    let mut result = Vec::new();

    // Alt prefix
    if alt {
        result.push(0x1B); // ESC
    }

    match key {
        TerminalKey::Char(c) => {
            if ctrl && c.is_ascii_alphabetic() {
                // Ctrl+A = 0x01, Ctrl+B = 0x02, etc.
                let ctrl_char = (c.to_ascii_lowercase() as u8) - b'a' + 1;
                result.push(ctrl_char);
            } else {
                let mut buf = [0u8; 4];
                let s = c.encode_utf8(&mut buf);
                result.extend_from_slice(s.as_bytes());
            }
        }
        TerminalKey::Enter => result.push(0x0D),     // CR
        TerminalKey::Backspace => result.push(0x7F), // DEL (most terminals use this)
        TerminalKey::Tab => {
            if ctrl {
                // Ctrl+Tab is not standard, send regular tab
                result.push(0x09);
            } else {
                result.push(0x09);
            }
        }
        TerminalKey::Escape => result.push(0x1B),
        TerminalKey::Up => result.extend_from_slice(b"\x1B[A"),
        TerminalKey::Down => result.extend_from_slice(b"\x1B[B"),
        TerminalKey::Right => result.extend_from_slice(b"\x1B[C"),
        TerminalKey::Left => result.extend_from_slice(b"\x1B[D"),
        TerminalKey::Home => result.extend_from_slice(b"\x1B[H"),
        TerminalKey::End => result.extend_from_slice(b"\x1B[F"),
        TerminalKey::PageUp => result.extend_from_slice(b"\x1B[5~"),
        TerminalKey::PageDown => result.extend_from_slice(b"\x1B[6~"),
        TerminalKey::Insert => result.extend_from_slice(b"\x1B[2~"),
        TerminalKey::Delete => result.extend_from_slice(b"\x1B[3~"),
        TerminalKey::F(n) => {
            let seq = match n {
                1 => b"\x1BOP".as_slice(),
                2 => b"\x1BOQ",
                3 => b"\x1BOR",
                4 => b"\x1BOS",
                5 => b"\x1B[15~",
                6 => b"\x1B[17~",
                7 => b"\x1B[18~",
                8 => b"\x1B[19~",
                9 => b"\x1B[20~",
                10 => b"\x1B[21~",
                11 => b"\x1B[23~",
                12 => b"\x1B[24~",
                _ => b"",
            };
            result.extend_from_slice(seq);
        }
    }

    result
}

/// Get the default shell for the current platform
pub fn default_shell() -> String {
    #[cfg(windows)]
    {
        std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_string())
    }
    #[cfg(not(windows))]
    {
        std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Whether a particular shell handle is still in the global list.
    ///
    /// Asserted by identity rather than by counting, because the list is
    /// process-global and these tests run in parallel with everything else that
    /// spawns a shell — a before/after count is a race, not a measurement.
    fn is_tracked(handle: &ShellHandle) -> bool {
        live_shells()
            .lock()
            .map(|live| live.iter().any(|h| Arc::ptr_eq(h, handle)))
            .unwrap_or(false)
    }

    /// A spawned shell must be reachable for killing, and reaped when the manager
    /// is closed.
    ///
    /// The child used to be bound to `_child` and dropped at the end of `spawn`,
    /// leaving no handle at all: `portable_pty`'s `Child` does not kill on drop, so
    /// the shell survived until the master happened to close and the shell happened
    /// to honour SIGHUP. `close` meanwhile set a `bool` and left a comment claiming
    /// the PTY would be closed on drop.
    #[test]
    fn a_shell_is_tracked_while_it_lives_and_reaped_when_closed() {
        let mut manager = TerminalManager::spawn(&default_shell()).expect("spawn a shell");
        let handle = manager.child.clone();

        assert!(
            handle.lock().unwrap().is_some(),
            "the child handle must be retained, or nothing can ever kill it"
        );
        assert!(
            is_tracked(&handle),
            "a live shell must be reachable from a signal handler"
        );

        manager.close().expect("close");

        assert!(
            handle.lock().unwrap().is_none(),
            "close must actually reap the shell, not just set a flag"
        );
        assert!(
            !is_tracked(&handle),
            "a reaped shell must stop being tracked, or the list grows for the life of the app"
        );
        assert!(manager.is_closed());
    }

    /// A shell that exits by itself is already beyond killing, but still needs
    /// one `wait` or it remains a zombie. This must happen when any tab's exit
    /// event is polled, while its final parsed output remains available.
    #[test]
    fn a_naturally_exited_shell_is_reaped_once_and_keeps_its_final_grid() {
        let mut manager = TerminalManager::spawn(&default_shell()).expect("spawn a shell");
        let handle = manager.child.clone();
        manager
            .write_str("printf '\\nSMITHY_FINAL_GRID\\n'; exit\n")
            .expect("tell shell to exit");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        let exit = loop {
            if let Some(TerminalEvent::Exit(code)) = manager.try_recv_event() {
                break code;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "shell never reported exit"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        };

        assert_eq!(exit, 0);
        assert!(manager.is_closed());
        assert!(handle.lock().unwrap().is_none(), "child was not waited");
        assert!(!is_tracked(&handle), "exited child stayed globally tracked");
        let snapshot = manager.snapshot_rows(0..usize::MAX);
        let text: String = snapshot
            .rows
            .iter()
            .flat_map(|row| row.iter().map(|cell| cell.c))
            .collect();
        assert!(
            text.contains("SMITHY_FINAL_GRID"),
            "reaping discarded the final terminal grid"
        );

        // The child Option was taken by the first observer. Drop, close, and
        // signal cleanup therefore have no second child on which to call wait.
        manager.reap_child(false);
        assert!(handle.lock().unwrap().is_none());
    }

    /// Dropping a manager any other way — a panic, a torn-down tab — must also
    /// reap, since `close` is only the explicit path.
    #[test]
    fn dropping_a_manager_reaps_its_shell() {
        let handle = {
            let manager = TerminalManager::spawn(&default_shell()).expect("spawn a shell");
            let handle = manager.child.clone();
            assert!(is_tracked(&handle));
            handle
        };

        assert!(
            handle.lock().unwrap().is_none(),
            "a dropped manager left its shell running"
        );
        assert!(!is_tracked(&handle));
    }

    /// `kill_all_shells` is what the signal handler calls, so it has to reap
    /// everything and be safe to call twice.
    #[test]
    fn killing_all_shells_reaps_every_one_and_is_idempotent() {
        let a = TerminalManager::spawn(&default_shell()).expect("spawn");
        let b = TerminalManager::spawn(&default_shell()).expect("spawn");
        let (ha, hb) = (a.child.clone(), b.child.clone());

        kill_all_shells();

        assert!(ha.lock().unwrap().is_none(), "first shell survived");
        assert!(hb.lock().unwrap().is_none(), "second shell survived");
        assert!(!is_tracked(&ha) && !is_tracked(&hb));

        // Safe on a signal path, where it may well be reached twice.
        kill_all_shells();
    }

    #[test]
    fn a_fresh_cell_is_blank_with_default_attributes() {
        let cell = TerminalCell::new();
        assert_eq!(cell.c, ' ');
        assert_eq!(cell.attrs.fg, TerminalColor::Default);
        assert_eq!(cell.attrs.bg, TerminalColor::Default);
        assert!(!cell.attrs.bold);
    }

    #[test]
    fn a_cell_carries_the_character_it_was_given() {
        let cell = TerminalCell::with_char('A');
        assert_eq!(cell.c, 'A');
    }

    #[test]
    fn a_new_grid_is_blank_at_the_requested_size() {
        let grid = TerminalGrid::new(80, 24);
        assert_eq!(grid.cols(), 80);
        assert_eq!(grid.rows(), 24);
        assert_eq!(grid.cursor_position(), (0, 0));
    }

    #[test]
    fn a_cell_can_be_read_back_and_out_of_bounds_is_none() {
        let grid = TerminalGrid::new(80, 24);

        // Valid cell
        let cell = grid.get_cell(0, 0);
        assert!(cell.is_some());
        assert_eq!(cell.unwrap().c, ' ');

        // Out of bounds
        assert!(grid.get_cell(80, 0).is_none());
        assert!(grid.get_cell(0, 24).is_none());
    }

    #[test]
    fn a_written_character_lands_where_the_cursor_is() {
        let mut grid = TerminalGrid::new(80, 24);

        grid.put_char('H');
        grid.put_char('i');

        assert_eq!(grid.get_cell(0, 0).unwrap().c, 'H');
        assert_eq!(grid.get_cell(1, 0).unwrap().c, 'i');
        assert_eq!(grid.cursor_position(), (2, 0));
    }

    /// The wrap is *deferred*: a character in the last column leaves the
    /// cursor where it is, and only the next printable character crosses to
    /// the next row. Anything else — a carriage return, a cursor move —
    /// cancels the crossing.
    #[test]
    fn text_past_the_right_edge_wraps_to_the_next_row() {
        let mut grid = TerminalGrid::new(5, 3);

        // Fill first line
        for c in "Hello".chars() {
            grid.put_char(c);
        }

        // Not yet wrapped: the cursor waits in the last column.
        assert_eq!(grid.cursor_position(), (4, 0));

        grid.put_char('!');
        assert_eq!(grid.get_cell(0, 1).unwrap().c, '!');
        assert_eq!(grid.cursor_position(), (1, 1));
    }

    /// zsh's PROMPT_SP: output that misses its trailing newline is terminated
    /// by a `%` padded with spaces to exactly the terminal width, then a
    /// carriage return — and the prompt overwrites the marker. An eager wrap
    /// used to strand that `%` on a line of its own above every prompt.
    #[test]
    fn a_full_width_marker_line_is_overwritten_after_a_carriage_return() {
        let mut grid = TerminalGrid::new(5, 3);

        for c in "%    ".chars() {
            grid.put_char(c);
        }
        grid.carriage_return();
        for c in "$ ".chars() {
            grid.put_char(c);
        }

        assert_eq!(grid.cursor_position(), (2, 0), "the prompt must not wrap");
        assert_eq!(grid.get_cell(0, 0).unwrap().c, '$');
        assert_eq!(grid.get_cell(1, 0).unwrap().c, ' ');
        assert!(
            grid.get_row(1).unwrap().iter().all(|cell| cell.c == ' '),
            "nothing may spill onto the next row"
        );
    }

    #[test]
    fn scrolling_drops_the_top_row_and_opens_one_at_the_bottom() {
        let mut grid = TerminalGrid::new(5, 2);

        // Fill both lines
        for c in "AAAAA".chars() {
            grid.put_char(c);
        }
        for c in "BBBBB".chars() {
            grid.put_char(c);
        }

        // Add one more character to trigger scroll
        grid.put_char('C');

        // First row should now have B's (scrolled up)
        assert_eq!(grid.get_cell(0, 0).unwrap().c, 'B');
        // Second row should have C at position 0
        assert_eq!(grid.get_cell(0, 1).unwrap().c, 'C');
    }

    #[test]
    fn a_carriage_return_moves_across_without_moving_down() {
        let mut grid = TerminalGrid::new(80, 24);

        grid.put_char('A');
        grid.put_char('B');
        grid.carriage_return();

        assert_eq!(grid.cursor_position(), (0, 0));
    }

    #[test]
    fn a_line_feed_moves_down_without_moving_across() {
        let mut grid = TerminalGrid::new(80, 24);

        grid.put_char('A');
        grid.line_feed();

        assert_eq!(grid.cursor_position(), (1, 1));
    }

    #[test]
    fn clearing_blanks_every_cell() {
        let mut grid = TerminalGrid::new(80, 24);

        grid.put_char('A');
        grid.put_char('B');
        grid.clear();

        assert_eq!(grid.get_cell(0, 0).unwrap().c, ' ');
        assert_eq!(grid.get_cell(1, 0).unwrap().c, ' ');
        assert_eq!(grid.cursor_position(), (0, 0));
    }

    #[test]
    fn resizing_keeps_the_text_that_still_fits() {
        let mut grid = TerminalGrid::new(80, 24);

        grid.put_char('A');
        grid.resize(40, 12);

        assert_eq!(grid.cols(), 40);
        assert_eq!(grid.rows(), 12);
        // Content should be preserved
        assert_eq!(grid.get_cell(0, 0).unwrap().c, 'A');
    }

    /// A real shell exchange, byte for byte, through the real parser.
    ///
    /// Reported symptom: after running `ls`, the typed command does not appear
    /// and the cursor sits on the first row instead of after the newest prompt.
    /// Reading the grid code found nothing, so this reproduces the whole
    /// sequence rather than testing a method in isolation.
    /// Lines pushed off the top must be retained, or there is nothing for a
    /// scrollbar to reach — which is exactly why wrapping the view in a scroll
    /// container achieved nothing until the grid kept its history.
    /// A resize has to reach the grid the *parser* writes into, not just the
    /// shared snapshot. The reader thread copies its performer's grid over the
    /// shared one after every read, so resizing only the snapshot is undone by
    /// the next byte that arrives.
    #[test]
    fn resizing_the_grid_preserves_visible_content() {
        let mut grid = TerminalGrid::new(20, 5);
        for ch in "hello".chars() {
            grid.put_char(ch);
        }

        grid.resize(40, 10);

        let first_row: String = (0..5)
            .filter_map(|c| grid.get_cell(c, 0).map(|cell| cell.c))
            .collect();
        assert_eq!(first_row, "hello", "content survives a widening resize");
        assert_eq!(grid.cols(), 40);
        assert_eq!(grid.rows(), 10);
    }

    /// The parser used to own a private grid and copy it over the UI's resized
    /// grid after every read. The first byte after SIGWINCH therefore restored
    /// the stale dimensions and laid text out against the wrong width.
    #[test]
    fn resize_and_parser_mutate_the_same_grid_without_stale_overwrite() {
        let grid = Arc::new(Mutex::new(TerminalGrid::new(5, 2)));
        resize_before_notify(&grid, 10, 3, || -> Result<(), ()> {
            // This closure stands in for redraw bytes racing directly behind
            // SIGWINCH. It must already observe the new dimensions, and it can
            // acquire the grid lock because no external PTY call holds it.
            let mut guard = grid.lock().unwrap();
            assert_eq!((guard.cols(), guard.rows()), (10, 3));
            let mut performer = TerminalPerformer::from_grid(&mut *guard);
            let mut parser = Parser::new();
            parser.advance(&mut performer, b"after");
            Ok(())
        })
        .unwrap();
        let guard = grid.lock().unwrap();
        assert_eq!((guard.cols(), guard.rows()), (10, 3));
        assert_eq!(guard.get_cell(4, 0).unwrap().c, 'r');
    }

    /// Rendering a small viewport in a 5,000-line history must not clone any
    /// row outside that viewport. The old full-grid snapshot paid for every
    /// retained cell on every poll.
    #[test]
    fn a_row_snapshot_contains_no_rows_outside_the_request() {
        let mut grid = TerminalGrid::new(4, 2);
        for line in ["aaaa", "bbbb", "cccc", "dddd"] {
            for ch in line.chars() {
                grid.put_char(ch);
            }
        }
        let snapshot = grid.snapshot_rows(1..3);
        assert_eq!(snapshot.start_row, 1);
        assert_eq!(snapshot.rows.len(), 2);
        assert_eq!(
            snapshot
                .rows
                .iter()
                .map(|row| row.iter().map(|cell| cell.c).collect::<String>())
                .collect::<Vec<_>>(),
            ["bbbb", "cccc"]
        );
    }

    /// The cursor row is live-grid-relative internally. Once scrollback
    /// exists, exposing that raw number draws the cursor over old output.
    #[test]
    fn a_snapshot_reports_the_cursor_at_its_absolute_row() {
        let mut grid = TerminalGrid::new(3, 2);
        for ch in "abcdefg".chars() {
            grid.put_char(ch);
        }
        let snapshot = grid.snapshot_rows(0..0);
        assert_eq!(snapshot.cursor.1, grid.scrollback.len() + grid.cursor_row);
        assert!(snapshot.cursor.1 >= grid.scrollback.len());
    }

    /// A PTY flood used to allocate one `Vec<u8>` and one unbounded channel
    /// node per read while an inactive tab could not consume them. Activity is
    /// a monotonic scalar now, and one queued wake covers the whole flood.
    #[test]
    fn an_output_flood_retains_one_wake_and_constant_sized_state() {
        let (tx, rx) = mpsc::sync_channel(1);
        let revision = AtomicU64::new(0);
        for _ in 0..100_000 {
            publish_activity(&tx, &revision);
        }
        assert_eq!(revision.load(Ordering::Acquire), 100_000);
        assert_eq!(rx.try_iter().count(), 1);
    }

    #[test]
    fn lines_scrolled_off_the_top_are_kept() {
        let mut grid = TerminalGrid::new(20, 3);
        for line in ["first", "second", "third", "fourth", "fifth"] {
            for ch in line.chars() {
                grid.put_char(ch);
            }
            grid.carriage_return();
            grid.line_feed();
        }

        let scrollback: Vec<String> = grid
            .scrollback()
            .iter()
            .map(|row| {
                row.iter()
                    .map(|c| c.c)
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect();

        assert_eq!(scrollback, vec!["first", "second", "third"]);
        assert_eq!(
            grid.total_rows(),
            scrollback.len() + 3,
            "total rows is scrollback plus the live grid"
        );
    }

    #[test]
    fn a_shell_session_leaves_the_cursor_after_the_last_prompt() {
        let mut parser = vte::Parser::new();
        let mut performer = TerminalPerformer::new(80, 24);

        // zsh emits a bracketed-paste enable, a colourised prompt, the echo of
        // what you typed, then CR/LF before the command output.
        let session: &[u8] = b"\x1b[?2004h\x1b[1;32mrj@rjs-Mac-Studio\x1b[0m smithy % ls\r\n\x1b[?2004lCargo.lock  README.md\r\nCargo.toml  apps\r\n\x1b[?2004h\x1b[1;32mrj@rjs-Mac-Studio\x1b[0m smithy % ";
        parser.advance(&mut performer, session);

        let grid = &performer.grid;
        let row_text = |row: usize| -> String {
            (0..grid.cols())
                .filter_map(|c| grid.get_cell(c, row).map(|cell| cell.c))
                .collect::<String>()
                .trim_end()
                .to_string()
        };

        assert!(
            row_text(0).ends_with("ls"),
            "the echoed command should be on row 0, got {:?}",
            row_text(0)
        );

        // Rows: 0 = prompt+ls, 1 and 2 = output, 3 = the new prompt.
        let (_, cursor_row) = grid.cursor_position();
        assert_eq!(
            cursor_row,
            3,
            "cursor should sit on the newest prompt (row 3), not row {cursor_row};              rows are {:?}",
            (0..5).map(row_text).collect::<Vec<_>>()
        );
    }

    #[test]
    fn printing_puts_characters_into_the_grid() {
        let mut performer = TerminalPerformer::new(80, 24);

        performer.print('H');
        performer.print('i');

        assert_eq!(performer.grid.get_cell(0, 0).unwrap().c, 'H');
        assert_eq!(performer.grid.get_cell(1, 0).unwrap().c, 'i');
    }

    #[test]
    fn carriage_return_and_line_feed_move_the_cursor() {
        let mut performer = TerminalPerformer::new(80, 24);

        performer.print('A');
        performer.execute(0x0D); // CR
        performer.execute(0x0A); // LF
        performer.print('B');

        assert_eq!(performer.grid.get_cell(0, 0).unwrap().c, 'A');
        assert_eq!(performer.grid.get_cell(0, 1).unwrap().c, 'B');
    }

    #[test]
    fn a_cursor_position_escape_moves_the_cursor_there() {
        let mut performer = TerminalPerformer::new(80, 24);

        // Move cursor to row 5, col 10 (1-indexed in ANSI, so 0-indexed is 4, 9)
        // Simulate CSI 5;10H
        performer.grid.move_cursor(9, 4);

        assert_eq!(performer.grid.cursor_position(), (9, 4));
    }

    #[test]
    fn sgr_sets_foreground_and_background_colours() {
        let mut performer = TerminalPerformer::new(80, 24);

        // Set red foreground (31)
        performer.handle_sgr(&[31]);
        assert_eq!(performer.grid.current_attrs.fg, TerminalColor::Indexed(1));

        // Set blue background (44)
        performer.handle_sgr(&[44]);
        assert_eq!(performer.grid.current_attrs.bg, TerminalColor::Indexed(4));

        // Reset (0)
        performer.handle_sgr(&[0]);
        assert_eq!(performer.grid.current_attrs.fg, TerminalColor::Default);
        assert_eq!(performer.grid.current_attrs.bg, TerminalColor::Default);
    }

    #[test]
    fn sgr_sets_bold_and_underline_and_resets_them() {
        let mut performer = TerminalPerformer::new(80, 24);

        // Bold (1)
        performer.handle_sgr(&[1]);
        assert!(performer.grid.current_attrs.bold);

        // Italic (3)
        performer.handle_sgr(&[3]);
        assert!(performer.grid.current_attrs.italic);

        // Underline (4)
        performer.handle_sgr(&[4]);
        assert!(performer.grid.current_attrs.underline);

        // Reset bold (22)
        performer.handle_sgr(&[22]);
        assert!(!performer.grid.current_attrs.bold);
    }

    #[test]
    fn a_shell_is_chosen_from_the_environment_or_falls_back() {
        let shell = default_shell();
        assert!(!shell.is_empty());

        #[cfg(windows)]
        {
            // On Windows, should be cmd.exe or similar
            assert!(shell.contains("cmd") || shell.contains("powershell"));
        }
    }

    #[test]
    fn the_default_colour_is_the_terminal_default() {
        let color = TerminalColor::default();
        assert_eq!(color, TerminalColor::Default);
    }

    #[test]
    fn default_cell_attributes_have_nothing_set() {
        let attrs = CellAttributes::default();
        assert_eq!(attrs.fg, TerminalColor::Default);
        assert_eq!(attrs.bg, TerminalColor::Default);
        assert!(!attrs.bold);
        assert!(!attrs.italic);
        assert!(!attrs.underline);
        assert!(!attrs.inverse);
    }

    #[test]
    fn a_row_reads_back_as_the_text_that_was_written() {
        let mut grid = TerminalGrid::new(5, 3);

        // Put some characters
        for c in "Hello".chars() {
            grid.put_char(c);
        }

        let row = grid.get_row(0).unwrap();
        assert_eq!(row.len(), 5);
        assert_eq!(row[0].c, 'H');
        assert_eq!(row[4].c, 'o');

        // Out of bounds
        assert!(grid.get_row(3).is_none());
    }

    #[test]
    fn a_backspace_moves_the_cursor_left() {
        let mut grid = TerminalGrid::new(80, 24);

        grid.put_char('A');
        grid.put_char('B');
        assert_eq!(grid.cursor_position(), (2, 0));

        grid.backspace();
        assert_eq!(grid.cursor_position(), (1, 0));

        // Backspace at column 0 should stay at 0
        grid.move_cursor(0, 0);
        grid.backspace();
        assert_eq!(grid.cursor_position(), (0, 0));
    }

    #[test]
    fn clearing_to_the_end_of_a_line_leaves_the_rest_alone() {
        let mut grid = TerminalGrid::new(10, 3);

        // Fill first row
        for c in "ABCDEFGHIJ".chars() {
            grid.put_char(c);
        }

        // Move cursor to column 5 and clear to end of line
        grid.move_cursor(5, 0);
        grid.clear_to_eol();

        // First 5 chars should remain
        assert_eq!(grid.get_cell(0, 0).unwrap().c, 'A');
        assert_eq!(grid.get_cell(4, 0).unwrap().c, 'E');
        // Rest should be cleared
        assert_eq!(grid.get_cell(5, 0).unwrap().c, ' ');
        assert_eq!(grid.get_cell(9, 0).unwrap().c, ' ');
    }

    // Keyboard input tests
    #[test]
    fn an_ordinary_character_is_sent_as_itself() {
        let seq = key_to_escape_sequence(TerminalKey::Char('a'), false, false);
        assert_eq!(seq, vec![b'a']);

        let seq = key_to_escape_sequence(TerminalKey::Char('Z'), false, false);
        assert_eq!(seq, vec![b'Z']);
    }

    #[test]
    fn a_control_combination_becomes_its_control_code() {
        // Ctrl+A = 0x01
        let seq = key_to_escape_sequence(TerminalKey::Char('a'), true, false);
        assert_eq!(seq, vec![0x01]);

        // Ctrl+C = 0x03
        let seq = key_to_escape_sequence(TerminalKey::Char('c'), true, false);
        assert_eq!(seq, vec![0x03]);

        // Ctrl+Z = 0x1A
        let seq = key_to_escape_sequence(TerminalKey::Char('z'), true, false);
        assert_eq!(seq, vec![0x1A]);
    }

    #[test]
    fn an_alt_combination_is_prefixed_with_escape() {
        // Alt+a = ESC + 'a'
        let seq = key_to_escape_sequence(TerminalKey::Char('a'), false, true);
        assert_eq!(seq, vec![0x1B, b'a']);
    }

    #[test]
    fn enter_tab_and_backspace_send_their_control_codes() {
        assert_eq!(
            key_to_escape_sequence(TerminalKey::Enter, false, false),
            vec![0x0D]
        );
        assert_eq!(
            key_to_escape_sequence(TerminalKey::Backspace, false, false),
            vec![0x7F]
        );
        assert_eq!(
            key_to_escape_sequence(TerminalKey::Tab, false, false),
            vec![0x09]
        );
        assert_eq!(
            key_to_escape_sequence(TerminalKey::Escape, false, false),
            vec![0x1B]
        );
    }

    #[test]
    fn the_arrow_keys_send_their_csi_sequences() {
        assert_eq!(
            key_to_escape_sequence(TerminalKey::Up, false, false),
            b"\x1B[A".to_vec()
        );
        assert_eq!(
            key_to_escape_sequence(TerminalKey::Down, false, false),
            b"\x1B[B".to_vec()
        );
        assert_eq!(
            key_to_escape_sequence(TerminalKey::Right, false, false),
            b"\x1B[C".to_vec()
        );
        assert_eq!(
            key_to_escape_sequence(TerminalKey::Left, false, false),
            b"\x1B[D".to_vec()
        );
    }

    #[test]
    fn home_end_and_page_keys_send_their_sequences() {
        assert_eq!(
            key_to_escape_sequence(TerminalKey::Home, false, false),
            b"\x1B[H".to_vec()
        );
        assert_eq!(
            key_to_escape_sequence(TerminalKey::End, false, false),
            b"\x1B[F".to_vec()
        );
        assert_eq!(
            key_to_escape_sequence(TerminalKey::PageUp, false, false),
            b"\x1B[5~".to_vec()
        );
        assert_eq!(
            key_to_escape_sequence(TerminalKey::PageDown, false, false),
            b"\x1B[6~".to_vec()
        );
        assert_eq!(
            key_to_escape_sequence(TerminalKey::Insert, false, false),
            b"\x1B[2~".to_vec()
        );
        assert_eq!(
            key_to_escape_sequence(TerminalKey::Delete, false, false),
            b"\x1B[3~".to_vec()
        );
    }

    #[test]
    fn the_function_keys_send_their_sequences() {
        assert_eq!(
            key_to_escape_sequence(TerminalKey::F(1), false, false),
            b"\x1BOP".to_vec()
        );
        assert_eq!(
            key_to_escape_sequence(TerminalKey::F(2), false, false),
            b"\x1BOQ".to_vec()
        );
        assert_eq!(
            key_to_escape_sequence(TerminalKey::F(3), false, false),
            b"\x1BOR".to_vec()
        );
        assert_eq!(
            key_to_escape_sequence(TerminalKey::F(4), false, false),
            b"\x1BOS".to_vec()
        );
        assert_eq!(
            key_to_escape_sequence(TerminalKey::F(5), false, false),
            b"\x1B[15~".to_vec()
        );
    }

    #[test]
    fn two_keys_are_equal_only_with_the_same_modifiers() {
        assert_eq!(TerminalKey::Char('a'), TerminalKey::Char('a'));
        assert_ne!(TerminalKey::Char('a'), TerminalKey::Char('b'));
        assert_eq!(TerminalKey::Enter, TerminalKey::Enter);
        assert_ne!(TerminalKey::Enter, TerminalKey::Tab);
        assert_eq!(TerminalKey::F(1), TerminalKey::F(1));
        assert_ne!(TerminalKey::F(1), TerminalKey::F(2));
    }

    // Property-based tests
    use proptest::prelude::*;

    // **Feature: forge-foundation, Property 12: ANSI Escape Sequence Interpretation**
    // *For any* valid ANSI escape sequence, the terminal state machine SHALL update
    // its internal state (colors, formatting, cursor position) according to the
    // ANSI specification.
    // **Validates: Requirements 6.5**
    proptest! {
        #![proptest_config(ProptestConfig::with_cases(100))]

        #[test]
        fn prop_ansi_cursor_position_updates_correctly(
            row in 1u16..=24,
            col in 1u16..=80,
        ) {
            let mut performer = TerminalPerformer::new(80, 24);
            let mut parser = Parser::new();

            // Send CSI row;col H (cursor position)
            let seq = format!("\x1B[{};{}H", row, col);
            parser.advance(&mut performer, seq.as_bytes());

            // Cursor should be at (col-1, row-1) since ANSI is 1-indexed
            let (cursor_col, cursor_row) = performer.grid.cursor_position();
            prop_assert_eq!(cursor_col, (col - 1) as usize);
            prop_assert_eq!(cursor_row, (row - 1) as usize);
        }

        #[test]
        fn prop_ansi_cursor_up_moves_correctly(
            initial_row in 5u16..=24,
            move_count in 1u16..=4,
        ) {
            let mut performer = TerminalPerformer::new(80, 24);
            let mut parser = Parser::new();

            // First move cursor to initial position
            let pos_seq = format!("\x1B[{};1H", initial_row);
            parser.advance(&mut performer, pos_seq.as_bytes());

            // Then send cursor up
            let up_seq = format!("\x1B[{}A", move_count);
            parser.advance(&mut performer, up_seq.as_bytes());

            let (_, cursor_row) = performer.grid.cursor_position();
            let expected_row = (initial_row - 1).saturating_sub(move_count) as usize;
            prop_assert_eq!(cursor_row, expected_row);
        }

        #[test]
        fn prop_ansi_cursor_down_moves_correctly(
            initial_row in 1u16..=20,
            move_count in 1u16..=4,
        ) {
            let mut performer = TerminalPerformer::new(80, 24);
            let mut parser = Parser::new();

            // First move cursor to initial position
            let pos_seq = format!("\x1B[{};1H", initial_row);
            parser.advance(&mut performer, pos_seq.as_bytes());

            // Then send cursor down
            let down_seq = format!("\x1B[{}B", move_count);
            parser.advance(&mut performer, down_seq.as_bytes());

            let (_, cursor_row) = performer.grid.cursor_position();
            let expected_row = ((initial_row - 1) + move_count).min(23) as usize;
            prop_assert_eq!(cursor_row, expected_row);
        }

        #[test]
        fn prop_ansi_cursor_forward_moves_correctly(
            initial_col in 1u16..=76,
            move_count in 1u16..=4,
        ) {
            let mut performer = TerminalPerformer::new(80, 24);
            let mut parser = Parser::new();

            // First move cursor to initial position
            let pos_seq = format!("\x1B[1;{}H", initial_col);
            parser.advance(&mut performer, pos_seq.as_bytes());

            // Then send cursor forward
            let fwd_seq = format!("\x1B[{}C", move_count);
            parser.advance(&mut performer, fwd_seq.as_bytes());

            let (cursor_col, _) = performer.grid.cursor_position();
            let expected_col = ((initial_col - 1) + move_count).min(79) as usize;
            prop_assert_eq!(cursor_col, expected_col);
        }

        #[test]
        fn prop_ansi_cursor_back_moves_correctly(
            initial_col in 5u16..=80,
            move_count in 1u16..=4,
        ) {
            let mut performer = TerminalPerformer::new(80, 24);
            let mut parser = Parser::new();

            // First move cursor to initial position
            let pos_seq = format!("\x1B[1;{}H", initial_col);
            parser.advance(&mut performer, pos_seq.as_bytes());

            // Then send cursor back
            let back_seq = format!("\x1B[{}D", move_count);
            parser.advance(&mut performer, back_seq.as_bytes());

            let (cursor_col, _) = performer.grid.cursor_position();
            let expected_col = (initial_col - 1).saturating_sub(move_count) as usize;
            prop_assert_eq!(cursor_col, expected_col);
        }

        #[test]
        fn prop_ansi_sgr_foreground_color_sets_correctly(
            color_code in 30u16..=37,
        ) {
            let mut performer = TerminalPerformer::new(80, 24);
            let mut parser = Parser::new();

            // Send SGR with foreground color
            let seq = format!("\x1B[{}m", color_code);
            parser.advance(&mut performer, seq.as_bytes());

            // Check that foreground color is set
            let expected_color = TerminalColor::Indexed((color_code - 30) as u8);
            prop_assert_eq!(performer.grid.current_attrs.fg, expected_color);
        }

        #[test]
        fn prop_ansi_sgr_background_color_sets_correctly(
            color_code in 40u16..=47,
        ) {
            let mut performer = TerminalPerformer::new(80, 24);
            let mut parser = Parser::new();

            // Send SGR with background color
            let seq = format!("\x1B[{}m", color_code);
            parser.advance(&mut performer, seq.as_bytes());

            // Check that background color is set
            let expected_color = TerminalColor::Indexed((color_code - 40) as u8);
            prop_assert_eq!(performer.grid.current_attrs.bg, expected_color);
        }

        #[test]
        fn prop_ansi_sgr_reset_clears_all_attributes(
            // Set some random attributes first
            fg_color in 30u16..=37,
            bg_color in 40u16..=47,
            bold in any::<bool>(),
        ) {
            let mut performer = TerminalPerformer::new(80, 24);
            let mut parser = Parser::new();

            // Set foreground color
            let fg_seq = format!("\x1B[{}m", fg_color);
            parser.advance(&mut performer, fg_seq.as_bytes());

            // Set background color
            let bg_seq = format!("\x1B[{}m", bg_color);
            parser.advance(&mut performer, bg_seq.as_bytes());

            // Set bold if requested
            if bold {
                parser.advance(&mut performer, b"\x1B[1m");
            }

            // Now reset
            parser.advance(&mut performer, b"\x1B[0m");

            // All attributes should be reset
            prop_assert_eq!(performer.grid.current_attrs.fg, TerminalColor::Default);
            prop_assert_eq!(performer.grid.current_attrs.bg, TerminalColor::Default);
            prop_assert!(!performer.grid.current_attrs.bold);
            prop_assert!(!performer.grid.current_attrs.italic);
            prop_assert!(!performer.grid.current_attrs.underline);
        }

        #[test]
        fn prop_ansi_erase_display_clears_from_cursor(
            cursor_row in 1u16..=12,
            cursor_col in 1u16..=40,
        ) {
            let mut performer = TerminalPerformer::new(80, 24);
            let mut parser = Parser::new();

            // Fill the screen with 'X'
            for _ in 0..(80 * 24) {
                performer.grid.put_char('X');
            }

            // Move cursor to position
            let pos_seq = format!("\x1B[{};{}H", cursor_row, cursor_col);
            parser.advance(&mut performer, pos_seq.as_bytes());

            // Erase from cursor to end of display (CSI 0 J)
            parser.advance(&mut performer, b"\x1B[0J");

            // Check that cells from cursor to end are cleared
            let (cur_col, cur_row) = performer.grid.cursor_position();

            // Rest of current line should be cleared
            for col in cur_col..80 {
                let cell = performer.grid.get_cell(col, cur_row).unwrap();
                prop_assert_eq!(cell.c, ' ', "Cell at ({}, {}) should be cleared", col, cur_row);
            }

            // All lines below should be cleared
            for row in (cur_row + 1)..24 {
                for col in 0..80 {
                    let cell = performer.grid.get_cell(col, row).unwrap();
                    prop_assert_eq!(cell.c, ' ', "Cell at ({}, {}) should be cleared", col, row);
                }
            }
        }

        #[test]
        fn prop_ansi_print_preserves_attributes(
            text in "[a-z]{1,10}",
            fg_color in 30u16..=37,
        ) {
            let mut performer = TerminalPerformer::new(80, 24);
            let mut parser = Parser::new();

            // Set foreground color
            let fg_seq = format!("\x1B[{}m", fg_color);
            parser.advance(&mut performer, fg_seq.as_bytes());

            // Print text
            parser.advance(&mut performer, text.as_bytes());

            // Check that all printed characters have the correct color
            let expected_color = TerminalColor::Indexed((fg_color - 30) as u8);
            for (i, _) in text.chars().enumerate() {
                let cell = performer.grid.get_cell(i, 0).unwrap();
                prop_assert_eq!(cell.attrs.fg, expected_color,
                    "Character at position {} should have correct color", i);
            }
        }
    }
}
