//! `vt100` cell → `ratatui` cell mapping.
//!
//! Vendored from `tui-term`'s `src/vt100_imp.rs` (MIT) so that we can track
//! `vt100` 0.18 without waiting for `tui-term` to move off 0.16. Attribute
//! coverage is deliberately identical to what we had before: symbol plus
//! bold/dim/italic/underline/inverse and fg/bg. Rich attributes (underline
//! colour, hyperlinks) arrive with the emitter in S2.

use ratatui::{
    buffer,
    style::{Color, Modifier, Style},
};

/// Copy one parsed screen cell into a `ratatui` buffer cell.
pub fn apply_cell(screen_cell: &vt100::Cell, buf_cell: &mut buffer::Cell) {
    // Continuation cells of a wide glyph have no contents of their own; leaving
    // the buffer cell blank lets the wide glyph in the previous column show
    // through, which is what the terminal does.
    if screen_cell.has_contents() {
        buf_cell.set_symbol(screen_cell.contents());
    }

    let mut modifier = Modifier::empty();
    if screen_cell.bold() {
        modifier |= Modifier::BOLD;
    }
    if screen_cell.dim() {
        modifier |= Modifier::DIM;
    }
    if screen_cell.italic() {
        modifier |= Modifier::ITALIC;
    }
    if screen_cell.underline() {
        modifier |= Modifier::UNDERLINED;
    }
    if screen_cell.inverse() {
        modifier |= Modifier::REVERSED;
    }

    buf_cell.set_style(
        Style::reset()
            .fg(map_color(screen_cell.fgcolor()))
            .bg(map_color(screen_cell.bgcolor()))
            .add_modifier(modifier),
    );
}

/// Map a parsed colour onto `ratatui`'s colour model.
pub fn map_color(color: vt100::Color) -> Color {
    match color {
        vt100::Color::Default => Color::Reset,
        vt100::Color::Idx(idx) => Color::Indexed(idx),
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}
