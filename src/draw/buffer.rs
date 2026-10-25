use crate::term;
use std::fmt::Write;

/// A buffer for building ANSI terminal output with fluent positioning and color API.
///
/// Encapsulates cursor positioning and color escape codes so callers
/// don't need to know about ANSI internals.
pub struct AnsiBuffer {
    buf: String,
}

impl AnsiBuffer {
    /// Create a new empty buffer.
    pub fn new() -> Self {
        Self {
            buf: String::with_capacity(4096),
        }
    }

    /// Move cursor to column x, row y (1-based).
    pub fn mv(&mut self, x: usize, y: usize) -> &mut Self {
        // Writing into a `String` is infallible by construction —
        // `String`'s `fmt::Write` impl never returns `Err`. We
        // assert this rather than `unwrap_or_default()` so a future
        // change that swaps the writer for a fallible one fails
        // loudly.
        write!(self.buf, "\x1b[{y};{x}H").expect("writing into a String is infallible");
        self
    }

    /// Set the foreground color from a theme escape string.
    pub fn color(&mut self, color: &str) -> &mut Self {
        self.buf.push_str(color);
        self
    }

    /// Write text at the current cursor position.
    pub fn text(&mut self, s: &str) -> &mut Self {
        self.buf.push_str(s);
        self
    }

    /// Reset all formatting.
    pub fn reset(&mut self) -> &mut Self {
        self.buf.push_str(term::RESET);
        self
    }

    /// Enable bold attribute.
    pub fn bold(&mut self) -> &mut Self {
        self.buf.push_str(term::BOLD);
        self
    }

    /// Clear the entire screen.
    pub fn clear_screen(&mut self) -> &mut Self {
        self.buf.push_str(term::CLEAR_SCREEN);
        self
    }

    /// Consume the buffer and return the built string.
    pub fn finish(mut self) -> String {
        self.buf.push_str(term::RESET);
        self.buf
    }

    /// Get the current buffer contents without consuming.
    #[cfg(test)]
    pub fn as_str(&self) -> &str {
        &self.buf
    }
}

impl Default for AnsiBuffer {
    fn default() -> Self {
        Self::new()
    }
}

/// Clip a positioned ANSI frame to the current terminal viewport.
///
/// The logical cursor is deliberately allowed past the right/bottom edge:
/// clamping an out-of-bounds CUP would paint discarded content over the
/// last visible row. Text resumes only after a subsequent in-bounds move.
/// SGR, erase and terminal modes survive unchanged. This also prevents
/// long overlay labels from wrapping and scrolling the alternate screen.
pub(crate) fn clip_to_viewport(frame: &str, width: usize, height: usize) -> String {
    let mut out = String::with_capacity(frame.len());
    let mut cursor = ClipCursor { x: 1, y: 1 };
    let mut remaining = frame;
    let mut visible_base = false;
    while !remaining.is_empty() {
        if remaining.starts_with("\x1b[") {
            let Some(end) = remaining[2..].find(|c: char| ('@'..='~').contains(&c)) else {
                break;
            };
            let end = end + 3;
            let sequence = &remaining[..end];
            let command = sequence.as_bytes()[end - 1];
            let args = &sequence[2..end - 1];
            if cursor.apply(command, args) {
                visible_base = false;
                if cursor.in_view(width, height) {
                    write!(out, "\x1b[{};{}H", cursor.y, cursor.x)
                        .expect("writing into a String is infallible");
                }
            } else if matches!(command, b'm' | b'J' | b'h' | b'l')
                || (command == b'K' && cursor.in_view(width, height))
            {
                out.push_str(sequence);
            }
            remaining = &remaining[end..];
            continue;
        }
        let ch = remaining.chars().next().expect("nonempty frame suffix");
        remaining = &remaining[ch.len_utf8()..];
        if ch == '\n' || ch == '\r' {
            if ch == '\n' {
                cursor.y = cursor.y.saturating_add(1);
            }
            cursor.x = 1;
            visible_base = false;
            if cursor.in_view(width, height) {
                write!(out, "\x1b[{};1H", cursor.y).expect("writing into a String is infallible");
            }
        } else if !ch.is_control() {
            let cells = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
            if cells == 0 {
                if visible_base {
                    out.push(ch);
                }
            } else {
                visible_base =
                    cursor.in_view(width, height) && cells <= width.saturating_sub(cursor.x - 1);
                if visible_base {
                    out.push(ch);
                }
                cursor.x = cursor.x.saturating_add(cells);
            }
        }
    }
    out
}

struct ClipCursor {
    x: usize,
    y: usize,
}

impl ClipCursor {
    fn in_view(&self, width: usize, height: usize) -> bool {
        self.x <= width && self.y <= height
    }

    /// Apply the cursor commands emitted by widgets and graph padding.
    fn apply(&mut self, command: u8, args: &str) -> bool {
        let mut values = args
            .split(';')
            .map(|s| s.parse::<usize>().unwrap_or(1).max(1));
        let first = values.next().unwrap_or(1);
        match command {
            b'H' | b'f' => {
                self.y = first;
                self.x = values.next().unwrap_or(1);
            }
            b'A' => self.y = self.y.saturating_sub(first).max(1),
            b'B' => self.y = self.y.saturating_add(first),
            b'C' => self.x = self.x.saturating_add(first),
            b'D' => self.x = self.x.saturating_sub(first).max(1),
            b'G' => self.x = first,
            b'd' => self.y = first,
            _ => return false,
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buffer_builds_positioned_colored_text() {
        let mut buf = AnsiBuffer::new();
        buf.mv(5, 10).color("\x1b[32m").text("hello");
        let s = buf.as_str();
        assert!(s.contains("\x1b[10;5H"));
        assert!(s.contains("\x1b[32m"));
        assert!(s.contains("hello"));
    }

    #[test]
    fn finish_appends_reset() {
        let buf = AnsiBuffer::new();
        let s = buf.finish();
        assert!(s.ends_with("\x1b[0m"));
    }

    #[test]
    fn default_creates_empty() {
        let buf = AnsiBuffer::default();
        assert!(buf.as_str().is_empty());
    }

    #[test]
    fn clipping_discards_overflow_until_cursor_returns_to_viewport() {
        let frame = "\x1b[1;1HabcdeWRAP\x1b[3;1Hbelow\x1b[2;2Hok";
        let out = clip_to_viewport(frame, 5, 2);
        assert_eq!(out, "\x1b[1;1Habcde\x1b[2;2Hok");
    }

    #[test]
    fn clipping_preserves_styles_clear_and_sync_modes() {
        let frame = "\x1b[?2026h\x1b[2J\x1b[31m\x1b[1;1Hab\x1b[0m\x1b[?2026l";
        assert_eq!(clip_to_viewport(frame, 2, 1), frame);
    }

    #[test]
    fn clipping_handles_wide_and_combining_characters_without_splitting_utf8() {
        let frame = "\x1b[1;1HA\u{301}中Z\x1b[2;3H中\u{301}\x1b[2;1Hç";
        let out = clip_to_viewport(frame, 3, 2);
        assert_eq!(out, "\x1b[1;1HA\u{301}中\x1b[2;3H\x1b[2;1Hç");
    }

    #[test]
    fn graph_cursor_padding_cannot_escape_viewport() {
        let frame = "\x1b[1;1Hx\x1b[2Cz\x1b[1Dq\x1b[1;2Hy";
        assert_eq!(clip_to_viewport(frame, 3, 1), "\x1b[1;1Hx\x1b[1;2Hy");
    }

    #[test]
    fn erase_line_outside_viewport_cannot_erase_the_previous_visible_row() {
        let frame = "\x1b[1;1Hkeep\x1b[8;1H\x1b[Kdiscard\x1b[2;1H\x1b[Kok";
        assert_eq!(
            clip_to_viewport(frame, 4, 2),
            "\x1b[1;1Hkeep\x1b[2;1H\x1b[Kok"
        );
    }

    #[test]
    fn clipping_zero_sized_viewports_and_incomplete_escapes_is_safe() {
        let frame = "\x1b[2J\x1b[1;1Htext\x1b[38;2;1";
        for (width, height) in [(0, 0), (0, 1), (1, 0)] {
            assert_eq!(clip_to_viewport(frame, width, height), "\x1b[2J");
        }
    }
}
