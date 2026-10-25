//! Waiting-for-data and all-hidden messages. Terminal dimensions never
//! block normal input; the layout engine supplies compact presentations.

use crate::app::TerminalSize;
use crate::app::lifecycle::style_terminal_output;
use crate::app::state::AppState;
use crate::config;
use crate::draw::buffer::AnsiBuffer;
use crate::term;
use crate::theme;
use crate::theme_keys as tc;

fn render_waiting_for_snapshot(size: TerminalSize, theme: &theme::Theme) -> String {
    let msg = "Collecting data...";
    let msg_y = (size.height / 2).max(1);
    let msg_x = size.width.saturating_sub(msg.len()) / 2 + 1;
    let mut buf = AnsiBuffer::new();
    buf.clear_screen()
        .mv(msg_x, msg_y)
        .bold()
        .color(theme.color(tc::HI_FG))
        .text(msg);
    buf.finish()
}

/// Render the "Collecting data..." message if dirty flags indicate it's needed.
pub(crate) fn render_if_dirty_waiting(
    state: &mut AppState,
    config: &config::Config,
    terminal: &mut term::Terminal,
    theme: &theme::Theme,
    size: TerminalSize,
) {
    if state.render.dirty.needs_layout() || state.render.dirty.is_any_widget_dirty() {
        let output =
            style_terminal_output(&render_waiting_for_snapshot(size, theme), config, theme);
        if let Err(e) = terminal.write_synced(&output) {
            tracing::warn!(
                subsystem = %crate::log::Subsystem::Terminal,
                error = %e,
                "terminal write failed",
            );
        }
        state.render.clear_dirty();
    }
}

/// Build the "all widgets hidden" overlay text. Lists `Shift+R` as
/// the bulk-reset key plus the per-widget toggle keys for the
/// widgets present in `active_layout` (so users on `cpu+proc` never
/// see "press 6 to show gpu").
fn render_all_hidden(
    size: TerminalSize,
    active_layout: &crate::domain::layout_spec::Slot,
    theme: &theme::Theme,
) -> String {
    let lines = build_all_hidden_lines(active_layout);
    let max_line = lines.iter().map(|l| l.len()).max().unwrap_or(0);
    let total = lines.len();
    let start_y = size.height.saturating_sub(total) / 2 + 1;
    let title_color = theme.color(tc::HI_FG);
    let body_color = theme.color(tc::TITLE);
    let mut buf = AnsiBuffer::new();
    buf.clear_screen();
    for (i, line) in lines.iter().enumerate() {
        let x = size.width.saturating_sub(max_line) / 2 + 1;
        let y = start_y + i;
        buf.mv(x, y);
        if i == 0 {
            buf.bold().color(title_color);
        } else {
            buf.color(body_color);
        }
        buf.text(line).reset();
    }
    buf.finish()
}

/// Compose the overlay's text lines. Pure function so the layout
/// is testable without rendering.
fn build_all_hidden_lines(active_layout: &crate::domain::layout_spec::Slot) -> Vec<String> {
    let mut lines = vec![
        "All widgets hidden.".to_string(),
        String::new(),
        "Press Shift+R to restore everything,".to_string(),
        "or press a number key to show:".to_string(),
        String::new(),
    ];
    let mut hints: Vec<String> = Vec::new();
    for (key, kind) in widget_toggle_hints() {
        if active_layout.contains(kind) {
            hints.push(format!("  {key}  {kind}"));
        }
    }
    if hints.is_empty() {
        // Active layout has nothing toggleable — only the bulk
        // reset key is meaningful. Drop the "or press" hint.
        lines.truncate(3);
    } else {
        lines.extend(hints);
    }
    lines
}

/// All numeric toggle keybinds in display order. Built from
/// [`WidgetKind::toggle_key`] so the overlay listing cannot drift
/// from the actual normal-mode bindings — adding a new individually-
/// addressable widget requires only updating `toggle_key`.
fn widget_toggle_hints() -> Vec<(char, crate::domain::widget_kind::WidgetKind)> {
    use crate::domain::widget_kind::WidgetKind;
    WidgetKind::all()
        .filter_map(|k| k.toggle_key().map(|c| (c, k)))
        .collect()
}

/// Render the all-hidden overlay if dirty flags indicate it's needed.
pub(crate) fn render_if_dirty_all_hidden(
    state: &mut AppState,
    config: &config::Config,
    terminal: &mut term::Terminal,
    theme: &theme::Theme,
    size: TerminalSize,
) {
    if state.render.dirty.needs_layout() || state.render.dirty.is_any_widget_dirty() {
        let active = config.layout_spec().clone();
        let output = style_terminal_output(&render_all_hidden(size, &active, theme), config, theme);
        if let Err(e) = terminal.write_synced(&output) {
            tracing::warn!(
                subsystem = %crate::log::Subsystem::Terminal,
                error = %e,
                "terminal write failed",
            );
        }
        state.render.clear_dirty();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::domain::layout_spec::Slot;
    use crate::domain::widget_kind::WidgetKind;

    #[test]
    fn all_hidden_lines_list_only_widgets_present_in_active_layout() {
        // Active layout = cpu+proc. Only those toggle keys appear.
        let active = Slot::VStack(vec![
            Slot::Widget(WidgetKind::Cpu),
            Slot::Widget(WidgetKind::Proc),
        ]);
        let lines = build_all_hidden_lines(&active);
        let body = lines.join("\n");
        assert!(body.contains("All widgets hidden."));
        assert!(body.contains("Shift+R"));
        assert!(body.contains("1") && body.contains("cpu"));
        assert!(body.contains("4") && body.contains("proc"));
        // Mem/net/disk/gpu hints must NOT appear.
        assert!(!body.contains("mem"));
        assert!(!body.contains("net"));
        assert!(!body.contains("disk"));
        assert!(!body.contains("gpu"));
    }

    #[test]
    fn all_hidden_lines_list_gpu_singleton_when_present() {
        // Active layout contains the singleton GPU widget. The
        // toggle-key hint table emits a single "6  gpu" line.
        let active = Slot::VStack(vec![
            Slot::Widget(WidgetKind::Cpu),
            Slot::Widget(WidgetKind::Gpu),
        ]);
        let lines = build_all_hidden_lines(&active);
        let body = lines.join("\n");
        assert!(body.contains("6  gpu"));
    }

    #[test]
    fn all_hidden_lines_omit_gpu_when_layout_excludes_gpu_widget() {
        // Active layout has no GPU widget — the GPU toggle key
        // must not appear.
        let active = Slot::Widget(WidgetKind::Cpu);
        let lines = build_all_hidden_lines(&active);
        let body = lines.join("\n");
        assert!(!body.contains("gpu"));
    }
}
