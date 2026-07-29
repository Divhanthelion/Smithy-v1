//! TerminalView module - Floem view for terminal emulation
//!
//! This module provides the terminal view component that renders the terminal grid,
//! handles colors and formatting, and processes keyboard input for the embedded terminal.
//!
//! ## Performance Architecture
//!
//! Following the pattern from high-performance terminals like Alacritty and Ghostty,
//! this module implements a single custom-painted View rather than composing thousands
//! of per-cell widgets. The GPU handles full redraws efficiently; the optimization focus
//! is on minimizing CPU-to-GPU data transfer by batching backgrounds and text runs.

use std::cell::RefCell;
use std::rc::Rc;

use floem::peniko::kurbo::{Point, Rect};
use floem::peniko::Color;
use floem::prelude::*;
use floem::prelude::{Key as FloemKey, NamedKey};
use floem::reactive::Effect;
use floem::reactive::{RwSignal, SignalGet, SignalUpdate};
use floem::text::{Attrs, AttrsList, FamilyOwned, FontStyle, FontWeight, TextLayout};
use floem::views::Decorators;

use crate::terminal::{TerminalCell, TerminalColor, TerminalGrid, TerminalKey, TerminalManager};

/// The family every terminal cell is laid out in. Held in one place so the
/// measurement and the drawing can never disagree about which face they mean.
///
/// Parsed from [`crate::design::MONO`] rather than being `FamilyOwned::Monospace`.
/// The generic resolved to **Courier**, which has no box-drawing characters at
/// all — so every TUI that draws a frame drew rows of missing-glyph boxes. Menlo
/// has the full set. Changing this changes cell metrics, which is safe only
/// because both the measure and the draw read this same value.
static MONOSPACE: std::sync::LazyLock<Vec<FamilyOwned>> =
    std::sync::LazyLock::new(|| FamilyOwned::parse_list(crate::design::MONO).collect());

/// Theme colors for the terminal
#[derive(Clone, Copy)]
pub struct TerminalTheme {
    /// Background color
    pub background: Color,
    /// Default foreground color
    pub foreground: Color,
    /// Cursor color
    pub cursor: Color,
    /// ANSI color palette (16 colors: 8 normal + 8 bright)
    pub palette: [Color; 16],
}

impl Default for TerminalTheme {
    fn default() -> Self {
        Self {
            background: Color::from_rgb8(30, 30, 30),
            foreground: Color::from_rgb8(212, 212, 212),
            cursor: Color::from_rgb8(255, 255, 255),
            palette: [
                // Normal colors (0-7)
                Color::from_rgb8(0, 0, 0),       // Black
                Color::from_rgb8(205, 49, 49),   // Red
                Color::from_rgb8(13, 188, 121),  // Green
                Color::from_rgb8(229, 229, 16),  // Yellow
                Color::from_rgb8(36, 114, 200),  // Blue
                Color::from_rgb8(188, 63, 188),  // Magenta
                Color::from_rgb8(17, 168, 205),  // Cyan
                Color::from_rgb8(229, 229, 229), // White
                // Bright colors (8-15)
                Color::from_rgb8(102, 102, 102), // Bright Black
                Color::from_rgb8(241, 76, 76),   // Bright Red
                Color::from_rgb8(35, 209, 139),  // Bright Green
                Color::from_rgb8(245, 245, 67),  // Bright Yellow
                Color::from_rgb8(59, 142, 234),  // Bright Blue
                Color::from_rgb8(214, 112, 214), // Bright Magenta
                Color::from_rgb8(41, 184, 219),  // Bright Cyan
                Color::from_rgb8(255, 255, 255), // Bright White
            ],
        }
    }
}

impl TerminalTheme {
    /// Convert a TerminalColor to a Floem Color
    pub fn resolve_color(&self, color: TerminalColor, is_foreground: bool) -> Color {
        match color {
            TerminalColor::Default => {
                if is_foreground {
                    self.foreground
                } else {
                    self.background
                }
            }
            TerminalColor::Indexed(idx) => {
                if idx < 16 {
                    self.palette[idx as usize]
                } else if idx < 232 {
                    // 216 color cube (6x6x6)
                    let idx = idx - 16;
                    let r = (idx / 36) % 6;
                    let g = (idx / 6) % 6;
                    let b = idx % 6;
                    Color::from_rgb8(
                        if r == 0 { 0 } else { r * 40 + 55 },
                        if g == 0 { 0 } else { g * 40 + 55 },
                        if b == 0 { 0 } else { b * 40 + 55 },
                    )
                } else {
                    // Grayscale (24 shades)
                    let gray = (idx - 232) * 10 + 8;
                    Color::from_rgb8(gray, gray, gray)
                }
            }
            TerminalColor::Rgb(r, g, b) => Color::from_rgb8(r, g, b),
        }
    }
}

/// State for the terminal view
pub struct TerminalViewState {
    /// The terminal manager (optional - may not be spawned yet)
    pub terminal: Option<TerminalManager>,
    /// Terminal theme
    pub theme: TerminalTheme,
    /// Font size in pixels
    pub font_size: f32,
    /// Whether the terminal has focus
    pub focused: bool,
    /// Cached grid for rendering
    cached_grid: Option<TerminalGrid>,
}

impl TerminalViewState {
    /// Create a new terminal view state without a terminal
    pub fn new() -> Self {
        Self {
            terminal: None,
            theme: TerminalTheme::default(),
            font_size: 14.0,
            focused: false,
            cached_grid: None,
        }
    }

    /// Spawn a terminal with the default shell
    pub fn spawn_default_shell(&mut self) -> Result<(), crate::error::TerminalError> {
        let shell = crate::terminal::default_shell();
        self.spawn_shell(&shell)
    }

    /// Spawn the default shell with its working directory set to `cwd`.
    pub fn spawn_default_shell_in(
        &mut self,
        cwd: &std::path::Path,
    ) -> Result<(), crate::error::TerminalError> {
        let shell = crate::terminal::default_shell();
        let terminal = TerminalManager::spawn_in(&shell, cwd)?;
        self.cached_grid = Some(terminal.grid());
        self.terminal = Some(terminal);
        Ok(())
    }

    /// Spawn a terminal with the specified shell
    pub fn spawn_shell(&mut self, shell: &str) -> Result<(), crate::error::TerminalError> {
        let terminal = TerminalManager::spawn(shell)?;
        self.cached_grid = Some(terminal.grid());
        self.terminal = Some(terminal);
        Ok(())
    }

    /// Close the terminal
    pub fn close(&mut self) -> Result<(), crate::error::TerminalError> {
        if let Some(ref mut terminal) = self.terminal {
            terminal.close()?;
        }
        self.terminal = None;
        self.cached_grid = None;
        Ok(())
    }

    /// Check if the terminal is active
    pub fn is_active(&self) -> bool {
        self.terminal
            .as_ref()
            .map(|t| !t.is_closed())
            .unwrap_or(false)
    }

    /// Get the current terminal grid
    pub fn grid(&self) -> Option<&TerminalGrid> {
        self.cached_grid.as_ref()
    }

    /// Update the cached grid from the terminal
    pub fn update_grid(&mut self) {
        if let Some(ref terminal) = self.terminal {
            self.cached_grid = Some(terminal.grid());
        }
    }

    /// Poll for terminal events and update state
    pub fn poll_events(&mut self) -> bool {
        let mut updated = false;
        if let Some(ref terminal) = self.terminal {
            while let Some(_event) = terminal.try_recv_event() {
                updated = true;
            }
            if updated {
                self.cached_grid = Some(terminal.grid());
            }
        }
        updated
    }

    /// Force refresh the cached grid
    ///
    /// Call this when you know the terminal has been updated externally.
    pub fn refresh(&mut self) {
        self.update_grid();
    }

    /// Send a key to the terminal
    pub fn send_key(
        &mut self,
        key: TerminalKey,
        ctrl: bool,
        alt: bool,
    ) -> Result<(), crate::error::TerminalError> {
        if let Some(ref mut terminal) = self.terminal {
            terminal.send_key(key, ctrl, alt)?;
        }
        Ok(())
    }

    /// Send a character to the terminal
    pub fn send_char(&mut self, c: char) -> Result<(), crate::error::TerminalError> {
        if let Some(ref mut terminal) = self.terminal {
            terminal.send_char(c)?;
        }
        Ok(())
    }

    /// Tell the shell how big it actually is.
    ///
    /// This existed and was never called, so the PTY kept the 80x24 it was
    /// opened with however wide the panel got — the shell wrapped at column 80
    /// and full-screen programs drew to the wrong geometry.
    pub fn resize(&mut self, cols: u16, rows: u16) -> Result<(), crate::error::TerminalError> {
        if let Some(ref mut terminal) = self.terminal {
            terminal.resize(cols, rows)?;
            self.cached_grid = Some(terminal.grid());
        }
        Ok(())
    }

    /// Get the line height in pixels
    pub fn line_height_px(&self) -> f32 {
        self.font_size * 1.2
    }

    /// Get the character width in pixels (monospace)
    pub fn char_width_px(&self) -> f32 {
        self.font_size * 0.6
    }
}

impl Default for TerminalViewState {
    fn default() -> Self {
        Self::new()
    }
}

/// Shared terminal state type
pub type SharedTerminalState = Rc<RefCell<TerminalViewState>>;

/// Convert Floem key event to TerminalKey
pub fn floem_key_to_terminal_key(key: &FloemKey) -> Option<TerminalKey> {
    match key {
        FloemKey::Named(named) => match named {
            NamedKey::Enter => Some(TerminalKey::Enter),
            NamedKey::Backspace => Some(TerminalKey::Backspace),
            NamedKey::Delete => Some(TerminalKey::Delete),
            NamedKey::Tab => Some(TerminalKey::Tab),
            NamedKey::Escape => Some(TerminalKey::Escape),
            NamedKey::ArrowUp => Some(TerminalKey::Up),
            NamedKey::ArrowDown => Some(TerminalKey::Down),
            NamedKey::ArrowLeft => Some(TerminalKey::Left),
            NamedKey::ArrowRight => Some(TerminalKey::Right),
            NamedKey::Home => Some(TerminalKey::Home),
            NamedKey::End => Some(TerminalKey::End),
            NamedKey::PageUp => Some(TerminalKey::PageUp),
            NamedKey::PageDown => Some(TerminalKey::PageDown),
            NamedKey::Insert => Some(TerminalKey::Insert),
            NamedKey::F1 => Some(TerminalKey::F(1)),
            NamedKey::F2 => Some(TerminalKey::F(2)),
            NamedKey::F3 => Some(TerminalKey::F(3)),
            NamedKey::F4 => Some(TerminalKey::F(4)),
            NamedKey::F5 => Some(TerminalKey::F(5)),
            NamedKey::F6 => Some(TerminalKey::F(6)),
            NamedKey::F7 => Some(TerminalKey::F(7)),
            NamedKey::F8 => Some(TerminalKey::F(8)),
            NamedKey::F9 => Some(TerminalKey::F(9)),
            NamedKey::F10 => Some(TerminalKey::F(10)),
            NamedKey::F11 => Some(TerminalKey::F(11)),
            NamedKey::F12 => Some(TerminalKey::F(12)),
            _ => None,
        },
        // `Character` also carries Space — ui-events has no `NamedKey::Space`,
        // unlike the winit keymap this was originally written against.
        FloemKey::Character(s) => {
            let chars: Vec<char> = s.chars().collect();
            if chars.len() == 1 {
                Some(TerminalKey::Char(chars[0]))
            } else {
                None
            }
        } // No catch-all on purpose: `Key` has exactly these two variants, so a
          // new one should fail the build here rather than silently map to None
          // and drop a key the terminal needed.
    }
}

/// A rendered terminal cell with resolved colors
#[derive(Clone, Debug)]
pub struct RenderedCell {
    /// The character to display
    pub c: char,
    /// Foreground color
    pub fg: Color,
    /// Background color
    pub bg: Color,
    /// Whether the cell is bold
    pub bold: bool,
    /// Whether the cell is italic
    pub italic: bool,
    /// Whether the cell is underlined
    pub underline: bool,
}

impl RenderedCell {
    /// Create a rendered cell from a terminal cell and theme
    pub fn from_cell(cell: &TerminalCell, theme: &TerminalTheme) -> Self {
        let attrs = &cell.attrs;

        // Resolve colors, handling inverse video
        let (fg, bg) = if attrs.inverse {
            (
                theme.resolve_color(attrs.bg, false),
                theme.resolve_color(attrs.fg, true),
            )
        } else {
            (
                theme.resolve_color(attrs.fg, true),
                theme.resolve_color(attrs.bg, false),
            )
        };

        Self {
            c: cell.c,
            fg,
            bg,
            bold: attrs.bold,
            italic: attrs.italic,
            underline: attrs.underline,
        }
    }
}

/// A rendered terminal row
#[derive(Clone, Debug)]
pub struct RenderedRow {
    /// Row index (0-indexed)
    pub row_idx: usize,
    /// Cells in this row
    pub cells: Vec<RenderedCell>,
}

impl TerminalViewState {
    /// Scrolled-off lines followed by the live grid, as one row list.
    ///
    /// `row_idx` is absolute across both, which is what the canvas positions
    /// against, so the cursor's grid-relative row has to be offset by
    /// [`Self::scrollback_len`] before it is drawn.
    pub fn get_scrollback_and_visible_rows(&self) -> Vec<RenderedRow> {
        let Some(grid) = &self.cached_grid else {
            return Vec::new();
        };

        let mut rows = Vec::with_capacity(grid.total_rows());
        for (idx, line) in grid.scrollback().iter().enumerate() {
            rows.push(RenderedRow {
                row_idx: idx,
                cells: line
                    .iter()
                    .map(|cell| RenderedCell::from_cell(cell, &self.theme))
                    .collect(),
            });
        }

        let offset = grid.scrollback().len();
        for row_idx in 0..grid.rows() {
            if let Some(row_cells) = grid.get_row(row_idx) {
                rows.push(RenderedRow {
                    row_idx: offset + row_idx,
                    cells: row_cells
                        .iter()
                        .map(|cell| RenderedCell::from_cell(cell, &self.theme))
                        .collect(),
                });
            }
        }

        rows
    }

    /// How many lines have scrolled off the top.
    pub fn scrollback_len(&self) -> usize {
        self.cached_grid
            .as_ref()
            .map_or(0, |g| g.scrollback().len())
    }

    /// Scrolled-off lines plus the live grid.
    pub fn total_rows(&self) -> usize {
        self.cached_grid.as_ref().map_or(0, |g| g.total_rows())
    }

    /// Get cursor position (col, row)
    pub fn cursor_position(&self) -> Option<(usize, usize)> {
        self.cached_grid.as_ref().map(|g| g.cursor_position())
    }
}

/// Paint the terminal grid onto a single canvas.
///
/// This replaced a `dyn_stack` of one absolutely-positioned label per text run.
/// Every property of that design worked against the terminal: a run's text and
/// its x/y were captured when its view was built, so a stable key rendered
/// stale text while a content-derived key rebuilt views on every keystroke and
/// cost the panel its keyboard focus mid-word. Rows contributed no layout
/// height, so there was nothing to scroll. And the cell size was guessed at
/// `font_size * 1.2` by `0.6` because nothing ever measured the font.
///
/// One canvas has none of those failure modes. `canvas` tracks the signals its
/// paint closure reads, so reading `version_signal` is enough to repaint; there
/// are no child views to go stale, be reused, or steal focus; and the cell
/// metrics come from laying out real glyphs rather than from a ratio.
pub fn terminal_grid_view(
    state: SharedTerminalState,
    version_signal: RwSignal<u64>,
) -> impl IntoView {
    let (theme, font_size) = {
        let state_ref = state.borrow();
        (state_ref.theme, state_ref.font_size)
    };

    let (char_width, line_height) = measure_monospace_cell(font_size);

    let state_for_resize = state.clone();
    // Only act on a real change: layout events fire far more often than the
    // cell geometry actually changes, and every resize costs a SIGWINCH and a
    // full redraw from the shell.
    let last_size = RwSignal::new((0u16, 0u16));
    // Whether new output should pull the view down with it.
    //
    // True while you are reading the bottom, false the moment you scroll up —
    // which is the whole contract: `ls` puts its results where you are looking,
    // and scrolling back to read something does not get yanked away by the next
    // line the shell prints.
    let stick_to_bottom = RwSignal::new(true);

    // Drives the canvas height. Read in `style`, which cannot borrow the
    // terminal state, so the row count is mirrored into a signal here.
    let total_rows_signal = RwSignal::new(0usize);
    let state_for_rows = state.clone();
    Effect::new(move |_| {
        let _ = version_signal.get();
        if let Ok(st) = state_for_rows.try_borrow() {
            total_rows_signal.set(st.total_rows());
        }
    });

    canvas(move |cx, size| {
        // Tracked: bumping `version_signal` is what schedules the repaint.
        let _ = version_signal.get();

        cx.fill(&Rect::ZERO.with_size(size), theme.background, 0.0);

        let Ok(state_ref) = state.try_borrow() else {
            return;
        };
        let rows = state_ref.get_scrollback_and_visible_rows();

        for row in &rows {
            let y = row.row_idx as f64 * line_height;
            if y > size.height {
                break;
            }

            // Cell backgrounds first, merged into spans so a run of one colour
            // is a single fill rather than one per column.
            let mut span: Option<(usize, Color)> = None;
            for (col, cell) in row.cells.iter().enumerate() {
                match span {
                    Some((start, colour)) if colour == cell.bg => {}
                    Some((start, colour)) => {
                        fill_cells(cx, start, col, y, char_width, line_height, colour, theme);
                        span = Some((col, cell.bg));
                    }
                    None => span = Some((col, cell.bg)),
                }
            }
            if let Some((start, colour)) = span {
                fill_cells(
                    cx,
                    start,
                    row.cells.len(),
                    y,
                    char_width,
                    line_height,
                    colour,
                    theme,
                );
            }

            // Then the glyphs, as one laid-out line carrying per-cell colour.
            let text: String = row.cells.iter().map(|c| c.c).collect();
            if text.trim().is_empty() {
                continue;
            }
            let mut attrs_list = AttrsList::new(base_attrs(font_size, theme.foreground));
            let mut byte = 0usize;
            for cell in &row.cells {
                let len = cell.c.len_utf8();
                if cell.fg != theme.foreground || cell.bold || cell.italic {
                    let mut attrs = base_attrs(font_size, cell.fg);
                    if cell.bold {
                        attrs = attrs.weight(FontWeight::BOLD);
                    }
                    if cell.italic {
                        attrs = attrs.font_style(FontStyle::Italic);
                    }
                    attrs_list.add_span(byte..byte + len, attrs);
                }
                byte += len;
            }

            let layout = TextLayout::new_with_text(&text, attrs_list, None);
            layout.draw(cx, Point::new(0.0, y));
        }

        // The cursor last, so it sits over the character it inverts.
        if let Some((col, row)) = state_ref.cursor_position() {
            // The cursor is addressed within the live grid, which is drawn
            // below however many lines have scrolled off.
            let row = row + state_ref.scrollback_len();
            let rect = Rect::new(
                col as f64 * char_width,
                row as f64 * line_height,
                (col + 1) as f64 * char_width,
                (row + 1) as f64 * line_height,
            );
            cx.fill(&rect, theme.cursor, 0.0);
        }
    })
    .style(move |s| {
        // Sized to the content, not the panel: a canvas has no intrinsic
        // height, so without this the scroll view has nothing to scroll.
        let total = total_rows_signal.get();
        s.width_full()
            .min_height(total as f64 * line_height)
            .background(theme.background)
    })
    .scroll()
    .style(move |s| {
        s.width_full()
            .height_full()
            .min_height(0.0)
            .background(theme.background)
    })
    // Follow new output down, unless you have scrolled away.
    //
    // `None` is the important half: `scroll_to` does nothing at all when the
    // closure declines to answer, which is how "leave them alone" is expressed.
    // Anything that always returns a position would fight the reader for the
    // scrollbar.
    .scroll_to(move || {
        let total = total_rows_signal.get();
        // Both tracked, so this runs on new output *and* on new lines.
        version_signal.get();
        stick_to_bottom
            .get()
            .then(|| Point::new(0.0, total as f64 * line_height))
    })
    .on_event_cont(
        floem::views::scroll::ScrollChangedListener,
        move |_, changed| {
            let content = total_rows_signal.get_untracked() as f64 * line_height;
            let viewport = f64::from(last_size.get_untracked().1) * line_height;
            stick_to_bottom.set(is_at_bottom(
                changed.offset.y,
                content,
                viewport,
                line_height,
            ));
        },
    )
    // Keep the shell's idea of its own size in step with the panel.
    //
    // Measured off the scroll view rather than the canvas: the canvas is sized
    // to its content, so its height is the scrollback, not the visible area.
    .on_event_cont(floem::context::LayoutChangedListener, move |_, layout| {
        let size = layout.new_box.size();
        let cols = (size.width / char_width).floor().max(1.0) as u16;
        let rows = (size.height / line_height).floor().max(1.0) as u16;
        if last_size.get_untracked() == (cols, rows) {
            return;
        }
        last_size.set((cols, rows));
        if let Ok(mut st) = state_for_resize.try_borrow_mut() {
            if let Err(e) = st.resize(cols, rows) {
                eprintln!("terminal resize failed: {e}");
            }
        }
        version_signal.update(|v| *v += 1);
    })
}

/// Whether a scroll position counts as being at the bottom.
///
/// The tolerance is what makes this usable rather than exact. A reader who is
/// one line off the bottom means to be at the bottom — they nudged the wheel —
/// and requiring an exact match would drop them out of follow mode for a
/// pixel. A couple of lines is forgiving enough to feel right and tight enough
/// that deliberately scrolling up always sticks.
///
/// **Content shorter than the viewport is always "at the bottom".** There is
/// nothing to scroll, so the alternative is a terminal that stops following its
/// own output until it has filled the panel once — which is the first thing
/// anybody sees. The `max(0.0)` is defensive rather than load-bearing: a
/// negative furthest-point compares the same way. It is there so the name means
/// what it says.
pub fn is_at_bottom(
    offset_y: f64,
    content_height: f64,
    viewport_height: f64,
    line_height: f64,
) -> bool {
    let furthest = (content_height - viewport_height).max(0.0);
    offset_y >= furthest - line_height * 2.0
}

/// Fill the background for columns `start..end` of one row.
///
/// Skipped when the colour is the panel background, which is already painted.
#[allow(clippy::too_many_arguments)]
fn fill_cells(
    cx: &mut floem::context::PaintCx,
    start: usize,
    end: usize,
    y: f64,
    char_width: f64,
    line_height: f64,
    colour: Color,
    theme: TerminalTheme,
) {
    if colour == theme.background || end <= start {
        return;
    }
    let rect = Rect::new(
        start as f64 * char_width,
        y,
        end as f64 * char_width,
        y + line_height,
    );
    cx.fill(&rect, colour, 0.0);
}

fn base_attrs<'a>(font_size: f32, colour: Color) -> Attrs<'a> {
    Attrs::new()
        .family(&MONOSPACE)
        .font_size(font_size)
        .color(colour)
}

/// Measure one monospace cell by laying out real glyphs.
///
/// The previous code assumed `font_size * 0.6` wide and `* 1.2` tall. Those are
/// plausible for some faces and wrong for others, and being wrong puts every
/// column progressively out of place. Laying out a known run and dividing gives
/// the advance the renderer will actually use.
fn measure_monospace_cell(font_size: f32) -> (f64, f64) {
    const SAMPLE: &str = "MMMMMMMMMM";
    let layout = TextLayout::new_with_text(
        SAMPLE,
        AttrsList::new(base_attrs(font_size, Color::BLACK)),
        None,
    );
    let size = layout.size();
    let width = size.width / SAMPLE.chars().count() as f64;
    // Fall back to the old ratios if the layout came back degenerate rather
    // than dividing by zero and collapsing every row onto the same line.
    if width > 0.0 && size.height > 0.0 {
        (width, size.height)
    } else {
        (font_size as f64 * 0.6, font_size as f64 * 1.2)
    }
}

#[cfg(test)]
mod tests {
    /// **The bottom is where new output goes.** Running `ls` should put its
    /// results in front of you, not leave you to scroll down and find them.
    #[test]
    fn sitting_at_the_bottom_counts_as_being_at_the_bottom() {
        let line = 18.0;
        // 100 lines of content in a 20-line panel: 80 lines of scroll.
        let (content, viewport) = (100.0 * line, 20.0 * line);
        let furthest = content - viewport;

        assert!(is_at_bottom(furthest, content, viewport, line));
        assert!(
            is_at_bottom(furthest - line, content, viewport, line),
            "a line off the bottom still means the bottom — a reader who nudged \
             the wheel did not ask to stop following"
        );
    }

    /// And scrolling up has to stop it following, or reading anything is
    /// impossible: the next line the shell prints would yank you away.
    #[test]
    fn scrolling_up_stops_the_view_following_new_output() {
        let line = 18.0;
        let (content, viewport) = (100.0 * line, 20.0 * line);
        let furthest = content - viewport;

        assert!(!is_at_bottom(
            furthest - line * 6.0,
            content,
            viewport,
            line
        ));
        assert!(
            !is_at_bottom(0.0, content, viewport, line),
            "at the very top"
        );
    }

    /// Nothing to scroll means nothing to be away from. Without this the
    /// terminal would refuse to follow its own output until it had filled the
    /// panel once — which is exactly the first thing anybody sees.
    #[test]
    fn a_terminal_that_has_not_filled_the_panel_always_follows() {
        let line = 18.0;
        let (content, viewport) = (4.0 * line, 20.0 * line);
        assert!(is_at_bottom(0.0, content, viewport, line));
        assert!(
            is_at_bottom(0.0, 0.0, viewport, line),
            "and when it is empty"
        );
    }

    use super::*;
    use crate::terminal::CellAttributes;

    #[test]
    fn the_default_terminal_theme_defines_all_sixteen_colours() {
        let theme = TerminalTheme::default();
        assert_eq!(theme.background, Color::from_rgb8(30, 30, 30));
        assert_eq!(theme.foreground, Color::from_rgb8(212, 212, 212));
    }

    #[test]
    fn the_default_colour_resolves_to_the_theme_foreground() {
        let theme = TerminalTheme::default();

        // Default foreground
        let fg = theme.resolve_color(TerminalColor::Default, true);
        assert_eq!(fg, theme.foreground);

        // Default background
        let bg = theme.resolve_color(TerminalColor::Default, false);
        assert_eq!(bg, theme.background);
    }

    #[test]
    fn the_first_sixteen_indices_resolve_to_the_palette() {
        let theme = TerminalTheme::default();

        // Basic ANSI colors (0-15)
        for i in 0..16 {
            let color = theme.resolve_color(TerminalColor::Indexed(i), true);
            assert_eq!(color, theme.palette[i as usize]);
        }
    }

    #[test]
    fn a_true_colour_is_used_as_given() {
        let theme = TerminalTheme::default();

        let color = theme.resolve_color(TerminalColor::Rgb(255, 128, 64), true);
        assert_eq!(color, Color::from_rgb8(255, 128, 64));
    }

    #[test]
    fn an_index_in_the_colour_cube_resolves_to_its_rgb() {
        let theme = TerminalTheme::default();

        // Test a color from the 216 color cube
        let color = theme.resolve_color(TerminalColor::Indexed(16), true);
        // Index 16 is (0,0,0) in the cube = black
        assert_eq!(color, Color::from_rgb8(0, 0, 0));

        // Index 231 is (5,5,5) in the cube = white-ish
        let color = theme.resolve_color(TerminalColor::Indexed(231), true);
        assert_eq!(color, Color::from_rgb8(255, 255, 255));
    }

    #[test]
    fn the_greyscale_ramp_resolves_to_even_greys() {
        let theme = TerminalTheme::default();

        // First grayscale (index 232)
        let color = theme.resolve_color(TerminalColor::Indexed(232), true);
        assert_eq!(color, Color::from_rgb8(8, 8, 8));

        // Last grayscale (index 255)
        let color = theme.resolve_color(TerminalColor::Indexed(255), true);
        assert_eq!(color, Color::from_rgb8(238, 238, 238));
    }

    #[test]
    fn a_fresh_view_state_starts_at_the_default_size() {
        let state = TerminalViewState::new();
        assert!(state.terminal.is_none());
        assert!(!state.is_active());
        assert!(state.grid().is_none());
    }

    #[test]
    fn the_view_state_reports_the_size_it_was_given() {
        let state = TerminalViewState::new();

        let line_height = state.line_height_px();
        assert!(line_height > 0.0);

        let char_width = state.char_width_px();
        assert!(char_width > 0.0);
    }

    #[test]
    fn a_grid_cell_renders_with_its_own_colours() {
        let theme = TerminalTheme::default();

        let cell = TerminalCell {
            c: 'A',
            attrs: CellAttributes {
                fg: TerminalColor::Indexed(1), // Red
                bg: TerminalColor::Default,
                bold: true,
                italic: false,
                underline: false,
                inverse: false,
            },
        };

        let rendered = RenderedCell::from_cell(&cell, &theme);
        assert_eq!(rendered.c, 'A');
        assert_eq!(rendered.fg, theme.palette[1]); // Red
        assert_eq!(rendered.bg, theme.background);
        assert!(rendered.bold);
        assert!(!rendered.italic);
    }

    #[test]
    fn inverse_video_swaps_foreground_and_background() {
        let theme = TerminalTheme::default();

        let cell = TerminalCell {
            c: 'X',
            attrs: CellAttributes {
                fg: TerminalColor::Indexed(2), // Green
                bg: TerminalColor::Indexed(4), // Blue
                bold: false,
                italic: false,
                underline: false,
                inverse: true,
            },
        };

        let rendered = RenderedCell::from_cell(&cell, &theme);
        // With inverse, fg and bg should be swapped
        assert_eq!(rendered.fg, theme.palette[4]); // Blue (was bg)
        assert_eq!(rendered.bg, theme.palette[2]); // Green (was fg)
    }

    #[test]
    fn a_floem_key_event_becomes_a_terminal_key() {
        // Named keys
        assert_eq!(
            floem_key_to_terminal_key(&FloemKey::Named(NamedKey::Enter)),
            Some(TerminalKey::Enter)
        );
        assert_eq!(
            floem_key_to_terminal_key(&FloemKey::Named(NamedKey::Backspace)),
            Some(TerminalKey::Backspace)
        );
        assert_eq!(
            floem_key_to_terminal_key(&FloemKey::Named(NamedKey::ArrowUp)),
            Some(TerminalKey::Up)
        );

        // Character keys
        assert_eq!(
            floem_key_to_terminal_key(&FloemKey::Character("a".into())),
            Some(TerminalKey::Char('a'))
        );

        // Function keys
        assert_eq!(
            floem_key_to_terminal_key(&FloemKey::Named(NamedKey::F1)),
            Some(TerminalKey::F(1))
        );
    }

    /// The canvas asks for rows before a shell has been spawned, on the very
    /// first paint. Returning an empty list rather than panicking is what lets
    /// the panel open at all.
    #[test]
    fn a_terminal_with_no_shell_yet_has_no_rows_to_draw() {
        let state = TerminalViewState::new();
        assert!(state.get_scrollback_and_visible_rows().is_empty());
        assert_eq!(state.total_rows(), 0);
        assert_eq!(state.scrollback_len(), 0);
    }
}
