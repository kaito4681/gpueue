//! Terminal styling, mirroring pueue's `OutputStyle`: colors are only emitted
//! when enabled (`--color always`, or `auto` on a terminal).

use std::io::IsTerminal;

use comfy_table::{Attribute as CellAttribute, Cell, Color as CellColor};

use crate::cli::ColorChoice;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Color {
    Green,
    Red,
    Yellow,
    Cyan,
    White,
}

impl Color {
    fn ansi(self) -> &'static str {
        match self {
            Color::Green => "32",
            Color::Red => "31",
            Color::Yellow => "33",
            Color::Cyan => "36",
            Color::White => "37",
        }
    }

    fn cell(self) -> CellColor {
        match self {
            Color::Green => CellColor::Green,
            Color::Red => CellColor::Red,
            Color::Yellow => CellColor::Yellow,
            Color::Cyan => CellColor::Cyan,
            Color::White => CellColor::White,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Style {
    pub enabled: bool,
}

impl Style {
    #[cfg(test)]
    pub const PLAIN: Style = Style { enabled: false };

    /// Colors for stdout according to `--color`.
    pub fn for_stdout(choice: ColorChoice) -> Self {
        let enabled = match choice {
            ColorChoice::Always => true,
            ColorChoice::Never => false,
            ColorChoice::Auto => std::io::stdout().is_terminal(),
        };
        Self { enabled }
    }

    /// Styles `text` with an optional color and bold attribute.
    pub fn paint(&self, text: impl ToString, color: Option<Color>, bold: bool) -> String {
        let text = text.to_string();
        if !self.enabled || (color.is_none() && !bold) {
            return text;
        }
        let mut codes = Vec::new();
        if bold {
            codes.push("1");
        }
        if let Some(color) = color {
            codes.push(color.ansi());
        }
        format!("\x1b[{}m{text}\x1b[0m", codes.join(";"))
    }

    pub fn bold(&self, text: impl ToString) -> String {
        self.paint(text, None, true)
    }

    /// A table cell with an optional color and bold attribute.
    pub fn cell(&self, text: impl ToString, color: Option<Color>, bold: bool) -> Cell {
        let mut cell = Cell::new(text.to_string());
        if !self.enabled {
            return cell;
        }
        if let Some(color) = color {
            cell = cell.fg(color.cell());
        }
        if bold {
            cell = cell.add_attribute(CellAttribute::Bold);
        }
        cell
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_style_never_emits_escapes() {
        assert_eq!(Style::PLAIN.paint("x", Some(Color::Red), true), "x");
    }

    #[test]
    fn enabled_style_wraps_text() {
        let style = Style { enabled: true };
        assert_eq!(
            style.paint("ok", Some(Color::Green), true),
            "\x1b[1;32mok\x1b[0m"
        );
        assert_eq!(style.paint("ok", None, false), "ok");
    }
}
