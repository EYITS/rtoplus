//! Dirty-flag execution: rebuild derived state, recompute the
//! cached layout, and produce the per-frame ANSI output.
//!
//! `execute_dirty_work` does the pre-render normalisation
//! (rebuild proc list, recompute layout, clamp proc selection).
//! `render_dirty_frame` and `render_all` produce the output string;
//! `write_dirty_frame` ships it to the terminal.
//!
//! `render_all` is a **pure function** of (state, dirty); all
//! mutation happens upstream so this module can stay easy to
//! reason about.

use crate::app::TerminalSize;
use crate::app::lifecycle::style_terminal_output;
use crate::app::state::{
    AppState, DetailPanel, DimComposeMode, DimUnderlayKeyRef, GpuViewState, LiveData,
    ModalFootprint, NetworkViewState, ProcessViewState, RuntimeView, WidgetFilter,
};
use crate::config;
use crate::dirty::RenderDirty;
use crate::domain::process::{ProcDisplayEntry, ProcInfo};
use crate::draw;
use crate::overlay::ActiveModal;
use crate::runner;
use crate::term;
use crate::theme;
use crate::ui;
use std::collections::HashSet;
use std::sync::OnceLock;

pub(crate) fn execute_dirty_work(
    state: &mut AppState,
    config: &mut config::Config,
    size: TerminalSize,
) {
    if state.render.dirty.needs_proc_list() {
        rebuild_proc_list(state, config);
    }

    if state.render.dirty.needs_layout() || state.render.cached_layout().is_none() {
        let layout = calculate_layout(state, config, size);
        state.render.set_cached_layout(layout);
    }

    // Pre-render normalisation: clamp the proc widget's view-state to
    // the current entry count and widget dimensions. Done here (not
    // inside render_all) so that the render path stays a pure
    // function of (state, dirty).
    if let Some(layout) = state.render.cached_layout()
        && let Some(proc_dim) = layout.dims_for(crate::domain::widget_kind::WidgetKind::Proc)
    {
        let compact = layout.is_compact(crate::domain::widget_kind::WidgetKind::Proc);
        let detail_rows = if !compact && state.process.detail_info().is_some() {
            ui::proc_widget::detail_row_count(proc_dim.width, proc_dim.height, true)
        } else {
            0
        };
        let max_visible = if compact {
            ui::responsive::process_row_count(
                proc_dim.height,
                state.process.detail_info().is_some(),
            )
        } else {
            ui::proc_widget::visible_row_count(proc_dim.height, detail_rows)
        };
        clamp_selection_to_rows(
            state.process.entries.len(),
            max_visible,
            &mut state.process.selected,
            &mut state.process.start,
        );
    }
}

fn rebuild_proc_list(state: &mut AppState, config: &config::Config) {
    state
        .process
        .rebuild_entries(config, &state.view, &state.live);
}

fn calculate_layout(
    state: &AppState,
    config: &config::Config,
    size: TerminalSize,
) -> draw::layout::Layout {
    draw::layout::calc_sizes(&draw::layout::LayoutConfig {
        term_width: size.width,
        term_height: size.height,
        root: config.layout_spec().clone(),
        hints: state.live.layout_hints(config, &state.view, &state.filter),
        hidden: state.compose_hidden(config),
    })
}

pub(crate) fn write_dirty_frame(
    state: &mut AppState,
    config: &config::Config,
    terminal: &mut term::Terminal,
    theme: &theme::Theme,
    size: TerminalSize,
) {
    // Clone to break the borrow on `state.overlay` so we can also
    // mutate `state.render` below. `ActiveModal` is cheap to
    // clone — variants are unit-like or carry small state.
    let active = state.overlay.active.clone();
    // Reconcile the dim cache against the current modal footprint
    // (Main, Help, Options, NoModal) and the current render-input
    // key (theme name, theme_background, terminal size). The cache
    // encodes whether the underlay snapshot is still valid AND
    // whether the previous modal's pixels need wiping; the returned
    // mode tells the compose path exactly what to emit this frame.
    let footprint = ModalFootprint::from_active(&active);
    let key = DimUnderlayKeyRef {
        theme_name: &config.ui.color_theme,
        theme_background: config.ui.theme_background,
        terminal_size: size,
    };
    let mode = state.render.reconcile_dim_compose(footprint, key);
    let output = match mode {
        DimComposeMode::Skip => {
            let raw = render_dirty_frame(state, config, theme);
            style_terminal_output(&raw, config, theme)
        }
        DimComposeMode::BuildAndEmit
        | DimComposeMode::EmitFromCache
        | DimComposeMode::ModalOnly => {
            compose_modal_frame(state, config, theme, &active, size, mode, key)
        }
    };
    if let Err(e) = terminal.write_synced(&output) {
        tracing::warn!(
            subsystem = %crate::log::Subsystem::Terminal,
            error = %e,
            "terminal write failed",
        );
    }
    state.render.clear_dirty();
}

/// Compose a single atomic frame consisting of a dimmed snapshot of
/// the widget layer plus the active modal painted on top at full
/// brightness.
///
/// The dimmed underlay snapshot is held inside [`crate::app::RenderState`]
/// for the lifetime of the modal — modal-internal navigation
/// repaints only the modal layer ([`DimComposeMode::ModalOnly`]).
/// The cache is invalidated by modal open/close transitions and
/// by terminal resize.
///
/// `mode` is the [`DimComposeMode`] returned by
/// [`crate::app::RenderState::reconcile_dim_compose`]; it carries
/// the per-frame decision (build vs. re-emit vs. modal-only).
///
/// **CLEAR_SCREEN gating.** Both [`DimComposeMode::BuildAndEmit`]
/// (cache miss — first paint after open, or after resize) and
/// [`DimComposeMode::EmitFromCache`] (modal kind changed mid-flight,
/// cache still valid) emit `CLEAR_SCREEN` as a frame prefix so the
/// transition starts from a clean canvas. The `EmitFromCache`
/// branch is what wipes the previous modal's footprint on a
/// kind-change transition (e.g. closing Options back to Main from
/// the main menu); without it, the larger previous modal's outer
/// ring would remain visible behind the smaller new modal.
///
/// [`DimComposeMode::ModalOnly`] (modal-internal navigation: same
/// modal kind, cache present) emits only the modal layer painted
/// over its own rectangle — no `CLEAR_SCREEN`, no re-emit of the
/// underlay. This is what lets a user keep an active mouse text-
/// selection on the dimmed area while navigating within a single
/// menu.
fn compose_modal_frame(
    state: &mut AppState,
    config: &config::Config,
    theme: &theme::Theme,
    active: &crate::overlay::ActiveModal,
    size: TerminalSize,
    mode: DimComposeMode,
    key: DimUnderlayKeyRef<'_>,
) -> String {
    let raw_modal = crate::overlay::render(active, size, config, theme);
    let styled_modal = style_terminal_output(&raw_modal, config, theme);

    match mode {
        DimComposeMode::BuildAndEmit => {
            // Build a fresh underlay: render every widget, theme-style
            // the result, then run the dim transform. Order matters —
            // theme styling runs before dim so every `\x1b[0m` reset
            // expanded by `style_output` gets the same fg-fade treatment
            // as every other fg in the underlay; otherwise resets would
            // re-establish full-brightness fg cells in the middle of
            // the dimmed surround.
            let raw = render_widget_layer_full(state, config, theme);
            let styled = style_terminal_output(&raw, config, theme);
            state.render.store_dimmed_underlay(
                crate::draw::dim::dim_truecolor(&styled, theme.rgb(crate::theme_keys::MAIN_BG)),
                key.to_owned(),
            );
            let dimmed = state
                .render
                .cached_dimmed_underlay()
                .expect("cached_dimmed_underlay just populated above");
            format!("{}{dimmed}{styled_modal}", term::CLEAR_SCREEN)
        }
        DimComposeMode::EmitFromCache => {
            // Modal kind changed but the dimmed underlay is still
            // valid (widgets unchanged while a modal was on top).
            // Re-emit the cached underlay to wipe the previous
            // modal's footprint, then paint the new modal on top.
            let dimmed = state
                .render
                .cached_dimmed_underlay()
                .expect("EmitFromCache is only returned by reconcile when the snapshot is present");
            format!("{}{dimmed}{styled_modal}", term::CLEAR_SCREEN)
        }
        DimComposeMode::ModalOnly => {
            // Cache hit, footprint unchanged: the dimmed underlay
            // is already on screen from a previous frame and the
            // modal box bounds haven't moved. Paint only the modal
            // layer over its own rectangle — cells outside the
            // modal are untouched.
            styled_modal
        }
        DimComposeMode::Skip => unreachable!(
            "compose_modal_frame is only entered for the three dim-modal modes; \
             write_dirty_frame routes Skip to the widget render path"
        ),
    }
}

/// Render every widget at full intensity, ignoring the per-widget
/// dirty bits (the dim cache must be a complete snapshot, not a
/// partial repaint of only the dirty widgets).
///
/// The output begins with `CLEAR_SCREEN` so the rendered frame is
/// a self-contained "draw me from scratch" string. Important: this
/// function is the sole producer of the dim cache, and that cache
/// is what gets emitted on the modal-open transition. The leading
/// clear is the right place for the canvas wipe — it travels with
/// the underlay through the dim transform and into the cache.
fn render_widget_layer_full(
    state: &AppState,
    config: &config::Config,
    theme: &theme::Theme,
) -> String {
    let layout = state
        .render
        .cached_layout()
        .expect("layout must be initialized before rendering");
    let params = RenderInputs {
        layout,
        live: &state.live,
        process: &state.process,
        network: &state.network,
        gpu: &state.gpu,
        view: &state.view,
        filter: &state.filter,
        config,
        theme,
        dirty: RenderDirty::all_widgets(),
        is_filtering: false,
    }
    .build();
    let mut output = String::new();
    output.push_str(term::CLEAR_SCREEN);
    output.push_str(&render_all(&params));
    output
}

fn render_dirty_frame(
    state: &mut AppState,
    config: &config::Config,
    theme: &theme::Theme,
) -> String {
    let layout = state
        .render
        .cached_layout()
        .expect("layout must be initialized before rendering");
    let mut output = String::new();

    if state.render.dirty.needs_layout() {
        output.push_str(term::CLEAR_SCREEN);
    }

    let params = RenderInputs {
        layout,
        live: &state.live,
        process: &state.process,
        network: &state.network,
        gpu: &state.gpu,
        view: &state.view,
        filter: &state.filter,
        config,
        theme,
        dirty: state.render.dirty,
        is_filtering: matches!(state.overlay.active, ActiveModal::Filter(_)),
    }
    .build();
    output.push_str(&render_all(&params));
    output
}

fn clamp_selection_to_rows(
    count: usize,
    max_visible: usize,
    selected: &mut usize,
    start: &mut usize,
) {
    if count == 0 {
        *selected = 0;
        *start = 0;
        return;
    }
    if *selected >= count {
        *selected = count - 1;
    }
    if max_visible == 0 {
        *start = *selected;
        return;
    }
    if *selected >= *start + max_visible {
        *start = *selected - max_visible + 1;
    }
    if *selected < *start {
        *start = *selected;
    }
}

/// Parameters for rendering the UI widgets.
pub(crate) struct RenderParams<'a> {
    pub(crate) dirty: RenderDirty,
    pub(crate) layout: &'a draw::layout::Layout,
    pub(crate) cpu: Option<&'a runner::CpuSnapshot>,
    pub(crate) mem: Option<&'a runner::MemSnapshot>,
    pub(crate) disk: Option<&'a runner::DiskSnapshot>,
    pub(crate) net: Option<&'a runner::NetSnapshot>,
    pub(crate) gpu: Option<&'a runner::GpuSnapshot>,
    pub(crate) proc_data: Option<&'a runner::ProcSnapshot>,
    pub(crate) statusbar: Option<&'a runner::StatusbarSnapshot>,
    pub(crate) proc_entries: &'a [ProcDisplayEntry],
    /// Process slice the proc widget renders rows from. Equals
    /// `pause.snapshot.procs` when paused; otherwise
    /// `proc_data.procs`. Borrowed for the lifetime of the frame so
    /// the renderer never has to repeat the paused/live choice.
    pub(crate) proc_source: &'a [ProcInfo],
    /// `true` when the proc-list pause is active. Drives the
    /// top-border `paused` chip and the dead-row layering rule.
    pub(crate) proc_paused: bool,
    /// PIDs from the paused snapshot that are no longer in the live
    /// snapshot. Empty when not paused. The proc widget consults
    /// this to render dead-row styling and the bottom-border
    /// `terminate` chip dim treatment.
    pub(crate) dead_pids: &'a HashSet<u32>,
    pub(crate) selected_net_iface: &'a str,
    pub(crate) selected_gpu_iface: &'a str,
    pub(crate) config: &'a config::Config,
    pub(crate) view: &'a RuntimeView,
    pub(crate) theme: &'a theme::Theme,
    pub(crate) rounded: bool,
    pub(crate) update_ms: u64,
    pub(crate) is_filtering: bool,
    /// `true` when the runtime view filter has at least one widget
    /// in it. Threaded into the CPU widget settings so the bottom
    /// preset hint can render a `*` suffix on the preset name.
    pub(crate) filter_active: bool,
    pub(crate) core_count: usize,
    pub(crate) total_mem: u64,
    /// Resolved detail panel input passed straight to the proc
    /// widget. `Some` when the panel is open; the `proc` reference
    /// points either into `proc_source` (when the source still
    /// contains the open PID) or into the cached `last_seen` on
    /// `state.process.detail` (when the watched process has
    /// exited). `dead = true` iff the live snapshot does not
    /// contain the open PID — the same rule for both paused and
    /// live modes.
    pub(crate) detail_view: Option<ui::DetailView<'a>>,
    pub(crate) followed_pid: u32,
    pub(crate) armed_terminate: Option<(&'a str, bool)>,
    pub(crate) proc_selected: usize,
    pub(crate) proc_start: usize,
}

/// Inputs required to build a [`RenderParams`].
///
/// Bundles the per-frame state borrows so the builder takes one
/// argument instead of nine. Used by [`RenderInputs::build`].
pub(crate) struct RenderInputs<'a> {
    pub(crate) layout: &'a draw::layout::Layout,
    pub(crate) live: &'a LiveData,
    pub(crate) process: &'a ProcessViewState,
    pub(crate) network: &'a NetworkViewState,
    pub(crate) gpu: &'a GpuViewState,
    pub(crate) view: &'a RuntimeView,
    pub(crate) filter: &'a WidgetFilter,
    pub(crate) config: &'a config::Config,
    pub(crate) theme: &'a theme::Theme,
    /// Which widgets to render this frame.
    pub(crate) dirty: RenderDirty,
    /// `true` while the user is in the filter overlay so the proc
    /// widget can show its inline filter prompt.
    pub(crate) is_filtering: bool,
}

/// Empty dead-PID set used as the borrow target when no pause is
/// active. `OnceLock` so the renderer can always borrow a real
/// `&HashSet<u32>` from `RenderParams::dead_pids` without dealing
/// with `Option` indirection.
fn empty_dead_pids() -> &'static HashSet<u32> {
    static EMPTY: OnceLock<HashSet<u32>> = OnceLock::new();
    EMPTY.get_or_init(HashSet::new)
}

impl<'a> RenderInputs<'a> {
    /// Materialise a [`RenderParams`] view of these inputs.
    ///
    /// Used by both `render_dirty_frame` (the main per-frame render
    /// path) and `handlers::redraw_after_overlay` (the post-overlay
    /// redraw path) so the per-widget field wiring lives in one
    /// place. Adding a new widget setting or per-frame state field
    /// touches one call site.
    pub(crate) fn build(self) -> RenderParams<'a> {
        let proc_source = self.process.procs_source(self.live).unwrap_or(&[]);
        let dead_pids = match self.process.pause.as_ref() {
            Some(p) => &p.dead_pids,
            None => empty_dead_pids(),
        };
        let detail_view = resolve_detail_view(self.process, self.live, proc_source);
        RenderParams {
            dirty: self.dirty,
            layout: self.layout,
            cpu: self.live.cpu.as_deref(),
            mem: self.live.mem.as_deref(),
            disk: self.live.disk.as_deref(),
            net: self.live.net.as_deref(),
            gpu: self.live.gpu.as_deref(),
            proc_data: self.live.proc_data.as_deref(),
            statusbar: self.live.statusbar.as_deref(),
            proc_entries: &self.process.entries,
            proc_source,
            proc_paused: self.process.pause.is_some(),
            dead_pids,
            selected_net_iface: self.network.selected_iface.as_str(),
            selected_gpu_iface: self.gpu.selected_iface.as_str(),
            config: self.config,
            view: self.view,
            theme: self.theme,
            rounded: self.config.ui.rounded_corners,
            update_ms: self.config.refresh.update_ms as u64,
            is_filtering: self.is_filtering,
            filter_active: !self.filter.hidden.is_empty(),
            core_count: self.live.core_count,
            total_mem: self.live.total_mem,
            detail_view,
            followed_pid: self.process.followed_pid,
            armed_terminate: self
                .process
                .armed_terminate
                .as_ref()
                .map(|(_, name, force)| (name.as_str(), *force)),
            proc_selected: self.process.selected,
            proc_start: self.process.start,
        }
    }
}

/// Resolve the proc widget's detail-panel render input.
///
/// Returns `None` when the panel is closed. Otherwise returns the
/// `ProcInfo` reference the renderer should display together with
/// the `dead` flag:
///
/// * `proc` borrows from `proc_source` when the source still
///   contains the open PID, falling back to the cached `last_seen`
///   on `process.detail` when it does not. This keeps the panel
///   functional after the watched process exits in live mode (the
///   row vanishes from `proc_source` but the cache survives) and
///   correctly mirrors the snapshot value in paused mode.
/// * `dead` is `true` iff the live snapshot does not contain the
///   open PID. The same rule applies in both paused and live modes
///   — in paused mode it reduces to "PID is in `dead_pids`" because
///   `dead_pids` is exactly the snapshot-minus-live diff intersected
///   with the snapshot's PID set.
fn resolve_detail_view<'a>(
    process: &'a ProcessViewState,
    live: &LiveData,
    proc_source: &'a [ProcInfo],
) -> Option<ui::DetailView<'a>> {
    let open_pid = match &process.detail {
        DetailPanel::Closed => return None,
        DetailPanel::Open(d) => d.pid,
    };
    let proc = proc_source
        .iter()
        .find(|p| p.pid == open_pid)
        .or_else(|| process.detail_info())?;
    let dead = live
        .proc_data
        .as_ref()
        .is_none_or(|s| !s.procs.iter().any(|p| p.pid == open_pid));
    Some(ui::DetailView { proc, dead })
}

/// Render UI widgets into an ANSI output string.
///
/// **Pure function** of `(params, params.layout, params.dirty)`.
/// All view-state normalisation (proc-list rebuild, selection
/// clamping, network-interface reconciliation) happens before this
/// is called, in `pull::pull_subsystem_data` or `execute_dirty_work`.
/// No state mutation, no I/O.
///
/// Only renders widgets whose corresponding dirty flag is set.
/// Pass `Dirty::ALL_WIDGETS` to render everything.
pub(crate) fn render_all(params: &RenderParams) -> String {
    let mut output = String::new();
    for widget in ui::WIDGETS {
        if widget.is_dirty(&params.dirty) {
            for &kind in widget.kinds() {
                if params.layout.is_compact(kind) {
                    ui::responsive::render(kind, params, &mut output);
                } else {
                    widget.render(params, &mut output);
                }
            }
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collect::CollectStatus;
    use crate::domain::process::ProcInfo;
    use std::sync::Arc;

    #[test]
    fn clamp_proc_selection_allows_last_visible_row() {
        let mut selected = 3;
        let mut start = 0;

        clamp_selection_to_rows(10, 4, &mut selected, &mut start);

        assert_eq!(selected, 3);
        assert_eq!(start, 0);
    }

    fn proc_with(pid: u32, name: &str) -> ProcInfo {
        ProcInfo {
            pid,
            name: name.into(),
            ..Default::default()
        }
    }

    fn snap(pids: &[u32]) -> Arc<runner::ProcSnapshot> {
        Arc::new(runner::ProcSnapshot {
            procs: pids.iter().map(|&pid| proc_with(pid, "p")).collect(),
            status: CollectStatus::Ok,
        })
    }

    /// Build a fresh `AppState` for resolver tests, then return
    /// `(process, live)` borrows for the call. Centralising the
    /// construction means the resolver tests do not depend on
    /// the privacy of `ProcessViewState::new` / `LiveData::new`.
    fn make_state() -> AppState {
        let config = config::Config::new();
        AppState::new(&config)
    }

    fn responsive_fixture(config: &config::Config) -> AppState {
        let mut state = AppState::new(config);
        state.live.core_count = 4;
        state.live.total_mem = 16 * 1024 * 1024 * 1024;
        let mut cpu = crate::domain::cpu::CpuInfo {
            core_count: 4,
            cpu_name: "Test CPU".into(),
            ..Default::default()
        };
        cpu.cpu_percent.total = [10, 20, 40, 30].into();
        cpu.core_percent = vec![[20, 40, 30].into(); 4];
        state.live.cpu = Some(Arc::new(runner::CpuSnapshot {
            info: cpu,
            status: CollectStatus::Ok,
        }));
        let mut mem = crate::domain::memory::MemInfo::default();
        mem.stats.used = state.live.total_mem / 2;
        mem.stats.available = state.live.total_mem / 2;
        mem.stats.swap_total = state.live.total_mem;
        mem.percent.used.push_back(50);
        state.live.mem = Some(Arc::new(runner::MemSnapshot {
            info: mem,
            status: CollectStatus::Ok,
        }));
        let mut gpu = crate::domain::gpu::GpuInfo {
            stable_id: "TEST:gpu".into(),
            name: "Test GPU".into(),
            ..Default::default()
        };
        gpu.gpu_percent.utilization.push_back(25);
        state.live.gpu = Some(Arc::new(runner::GpuSnapshot {
            devices: vec![gpu],
            status: CollectStatus::Ok,
        }));
        state.live.disk = Some(Arc::new(runner::DiskSnapshot {
            info: crate::domain::disk::DiskData {
                disks: vec![crate::domain::disk::DiskInfo {
                    name: "C:".into(),
                    total: 1000,
                    used: 500,
                    used_percent: 50,
                    ..Default::default()
                }],
            },
            status: CollectStatus::Ok,
        }));
        state.live.net = Some(Arc::new(runner::NetSnapshot {
            nets: vec![crate::domain::network::NetInfo {
                stable_id: "TEST:net".into(),
                description: "Test NIC".into(),
                ..Default::default()
            }],
            status: CollectStatus::Ok,
        }));
        state.live.proc_data = Some(Arc::new(runner::ProcSnapshot {
            procs: (1..=16)
                .map(|pid| ProcInfo {
                    pid,
                    name: if pid < 3 {
                        "ChatGPT.exe".into()
                    } else {
                        format!("程序{pid}.exe")
                    },
                    cmd: format!("test.exe --pid={pid}"),
                    exe_path: if pid < 3 {
                        Arc::from("c:\\apps\\chatgpt.exe")
                    } else {
                        Arc::from(format!("c:\\apps\\test{pid}.exe"))
                    },
                    cpu_p: 20.0,
                    mem: 1024 * 1024 * 100,
                    ..Default::default()
                })
                .collect(),
            status: CollectStatus::Ok,
        }));
        state
    }

    /// Independent ANSI output check: every cursor target and printable
    /// cell must remain in the terminal after the final viewport clip.
    fn assert_frame_bounds(frame: &str, width: usize, height: usize) -> usize {
        let mut tail = frame;
        let (mut x, mut y) = (1, 1);
        let mut cells = 0;
        while !tail.is_empty() {
            if tail.starts_with("\x1b[") {
                let end = tail[2..].find(|c: char| ('@'..='~').contains(&c)).unwrap() + 3;
                if tail.as_bytes()[end - 1] == b'H' {
                    let values: Vec<usize> = tail[2..end - 1]
                        .split(';')
                        .map(|s| s.parse().unwrap())
                        .collect();
                    y = values[0];
                    x = values[1];
                    assert!(
                        x > 0 && x <= width && y > 0 && y <= height,
                        "cursor outside {width}x{height}: {x},{y}"
                    );
                }
                tail = &tail[end..];
            } else {
                let ch = tail.chars().next().unwrap();
                let glyph_width = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
                if glyph_width > 0 {
                    assert!(
                        y <= height && x + glyph_width - 1 <= width,
                        "text wraps outside {width}x{height}: {x},{y}, {ch}"
                    );
                    cells += glyph_width;
                    x += glyph_width;
                }
                tail = &tail[ch.len_utf8()..];
            }
        }
        cells
    }

    #[test]
    fn full_render_presets_and_custom_layout_survive_requested_sizes_and_tiny_overlays() {
        let theme = theme::Theme::new();
        let mut roots: Vec<_> = crate::domain::preset::BuiltinPreset::ALL
            .into_iter()
            .map(|p| p.layout_spec())
            .collect();
        roots.push(
            "vstack(hstack(vstack(cpu, gpu), vstack(mem, proc)), statusbar)"
                .parse()
                .unwrap(),
        );
        for root in roots {
            for grouped in [false, true] {
                let mut config = config::Config::new();
                config.layout.set_custom(root.clone());
                config.set_active_preset(crate::domain::preset::ActivePreset::Custom);
                config.view.proc_grouped = grouped;
                let mut state = responsive_fixture(&config);
                for (width, height) in [
                    (120, 40),
                    (80, 24),
                    (60, 20),
                    (40, 12),
                    (30, 10),
                    (20, 8),
                    (8, 3),
                    (1, 1),
                    (0, 0),
                    (0, 8),
                    (8, 0),
                ] {
                    let size = TerminalSize { width, height };
                    state.render.mark_resize();
                    execute_dirty_work(&mut state, &mut config, size);
                    let raw = render_widget_layer_full(&state, &config, &theme);
                    let clipped = draw::buffer::clip_to_viewport(&raw, width, height);
                    let cells = assert_frame_bounds(&clipped, width, height);
                    if width > 0 && height > 0 {
                        assert!(cells > 0, "blank {width}x{height}: {root}");
                    }
                    assert!(!raw.contains("Terminal too small"));
                    assert_eq!(config.layout_spec(), &root);
                }
            }
        }
        let config = config::Config::new();
        let overlays = [
            ActiveModal::Main(crate::overlay::main_menu::MainMenuState::new()),
            ActiveModal::Help(crate::overlay::help::HelpState::new(
                crate::overlay::ReturnTarget::Normal,
            )),
            ActiveModal::Options(crate::overlay::options::OptionsState::new(
                crate::overlay::ReturnTarget::Normal,
            )),
        ];
        for active in overlays {
            for (width, height) in [
                (120, 40),
                (80, 24),
                (60, 20),
                (40, 12),
                (30, 10),
                (20, 8),
                (8, 3),
                (1, 1),
                (0, 0),
            ] {
                let raw = crate::overlay::render(
                    &active,
                    TerminalSize { width, height },
                    &config,
                    &theme,
                );
                assert_frame_bounds(
                    &draw::buffer::clip_to_viewport(&raw, width, height),
                    width,
                    height,
                );
            }
        }
    }

    #[test]
    fn compact_processes_show_group_count_totals_and_keep_last_selection_visible() {
        let mut config = config::Config::new();
        config.set_custom_layout("proc").unwrap();
        config.set_active_preset(crate::domain::preset::ActivePreset::Custom);
        config.view.proc_grouped = true;
        let mut state = responsive_fixture(&config);
        let theme = theme::Theme::new();
        let size = TerminalSize {
            width: 40,
            height: 8,
        };
        execute_dirty_work(&mut state, &mut config, size);
        let out = render_widget_layer_full(&state, &config, &theme);
        assert!(out.contains("ChatGPT (2)"));
        assert!(
            out.contains("10.0"),
            "aggregate CPU should use the same core normalization"
        );
        state.process.selected = state.process.entries.len() - 1;
        state.render.clear_dirty();
        state.render.dirty.mark_proc_widget();
        execute_dirty_work(&mut state, &mut config, size);
        assert!(state.process.selected >= state.process.start);
        assert!(
            state.process.selected
                < state.process.start + ui::responsive::process_row_count(8, false)
        );
    }

    #[test]
    fn compact_rendering_preserves_per_widget_dirty_flags() {
        let mut config = config::Config::new();
        let mut state = responsive_fixture(&config);
        execute_dirty_work(
            &mut state,
            &mut config,
            TerminalSize {
                width: 20,
                height: 8,
            },
        );
        let theme = theme::Theme::new();
        let mut dirty = RenderDirty::empty();
        dirty.mark_widget(crate::domain::widget_kind::WidgetKind::Cpu);
        let params = RenderInputs {
            layout: state.render.cached_layout().unwrap(),
            live: &state.live,
            process: &state.process,
            network: &state.network,
            gpu: &state.gpu,
            view: &state.view,
            filter: &state.filter,
            config: &config,
            theme: &theme,
            dirty,
            is_filtering: false,
        }
        .build();
        let out = render_all(&params);
        assert!(out.contains("CPU"));
        assert!(!out.contains("RAM"));
        assert!(!out.contains("Program"));
    }

    #[test]
    fn resolve_detail_view_returns_none_when_panel_closed() {
        let state = make_state();
        let proc_source: Vec<ProcInfo> = vec![];
        assert!(matches!(state.process.detail, DetailPanel::Closed));
        assert!(resolve_detail_view(&state.process, &state.live, &proc_source).is_none());
    }

    #[test]
    fn resolve_detail_view_uses_proc_source_when_pid_present() {
        // Live mode, process alive: the detail proc reference points
        // at the live snapshot row, NOT the cached last_seen.
        let mut state = make_state();
        state.process.open_detail(proc_with(100, "stale-cache"));
        state.live.proc_data = Some(snap(&[100, 200]));
        let proc_source = vec![proc_with(100, "fresh-live"), proc_with(200, "other")];

        let view = resolve_detail_view(&state.process, &state.live, &proc_source)
            .expect("panel resolution should succeed");
        assert_eq!(view.proc.name, "fresh-live");
        assert!(
            !view.dead,
            "PID 100 is in live snapshot, dead must be false"
        );
    }

    #[test]
    fn resolve_detail_view_falls_back_to_cache_when_pid_absent_from_source() {
        // Live mode, process exited: source no longer contains the
        // PID. resolve falls back to the cached last_seen and marks
        // dead = true.
        let mut state = make_state();
        state.process.open_detail(proc_with(100, "cached-name"));
        state.live.proc_data = Some(snap(&[200, 300]));
        let proc_source = vec![proc_with(200, "other"), proc_with(300, "another")];

        let view = resolve_detail_view(&state.process, &state.live, &proc_source)
            .expect("cache fallback succeeds");
        assert_eq!(view.proc.name, "cached-name");
        assert!(
            view.dead,
            "PID 100 is not in live snapshot, dead must be true"
        );
    }

    #[test]
    fn resolve_detail_view_marks_dead_when_no_live_data() {
        // First-frame race: panel was somehow opened (e.g. via
        // initial keystroke) before the first live snapshot
        // arrived. dead = true is the correct conservative answer
        // because there is no evidence the PID is alive.
        let mut state = make_state();
        state.process.open_detail(proc_with(100, "alpha"));
        let proc_source = vec![proc_with(100, "alpha")];

        let view = resolve_detail_view(&state.process, &state.live, &proc_source)
            .expect("source still resolves the proc");
        assert!(
            view.dead,
            "no live data means dead must default to true (no proof of life)"
        );
    }

    #[test]
    fn resolve_detail_view_dead_flag_unifies_paused_and_live_modes() {
        // The dead rule (`!live.contains(open_pid)`) gives the same
        // answer in paused mode that the existing `dead_pids`
        // machinery produces. Construct the paused-mode shape and
        // verify the rule fires.
        let mut state = make_state();
        state.process.open_detail(proc_with(100, "alpha"));

        // Paused mode: proc_source comes from the snapshot, which
        // still contains the open PID.
        let proc_source = vec![proc_with(100, "alpha"), proc_with(200, "beta")];

        // Live snapshot lost PID 100 (process exited after pause).
        state.live.proc_data = Some(snap(&[200]));

        let view = resolve_detail_view(&state.process, &state.live, &proc_source)
            .expect("snapshot still has PID 100");
        assert_eq!(view.proc.pid, 100);
        assert!(
            view.dead,
            "live lost PID 100, dead must be true even when source still has it"
        );
    }

    #[test]
    fn resolve_detail_view_dead_flag_clears_on_resurrection() {
        // PID dies, then a process with the same PID reappears in
        // the live snapshot. resolve must report dead = false
        // again. (The panel will then show the new live values; the
        // cached `last_seen` is overwritten by the next
        // `refresh_detail_cache` call in the pull path.)
        let mut state = make_state();
        state.process.open_detail(proc_with(100, "old"));

        state.live.proc_data = Some(snap(&[100, 200]));
        let proc_source = vec![proc_with(100, "resurrected"), proc_with(200, "other")];

        let view = resolve_detail_view(&state.process, &state.live, &proc_source).unwrap();
        assert!(!view.dead);
        assert_eq!(view.proc.name, "resurrected");
    }

    // `compose_modal_frame` now takes an explicit `DimComposeMode`.
    // The mode is produced by `RenderState::reconcile_dim_compose`
    // from the current `ModalFootprint`; these tests drive both the
    // reconcile and the compose to lock in the end-to-end behaviour.

    /// Build a fresh `AppState` ready for modal compose: empty layout
    /// cached, the requested modal active, and the dim-cache footprint
    /// reset to `NoModal` so the first reconcile sees an open
    /// transition.
    fn modal_state(active: crate::overlay::ActiveModal) -> AppState {
        let mut state = make_state();
        state
            .render
            .set_cached_layout(crate::draw::layout::Layout::default());
        state.overlay.active = active;
        state
    }

    fn fixture_size() -> TerminalSize {
        TerminalSize {
            width: 80,
            height: 30,
        }
    }

    /// Build a synthetic render-input key for tests, matching what
    /// `write_dirty_frame` constructs at runtime from the live
    /// config + terminal size. Tests use the default theme name so
    /// any concrete theme value works; the value only matters when
    /// a test deliberately mutates it to exercise key-mismatch
    /// invalidation (see the `state.rs` unit tests).
    fn fixture_key<'a>(config: &'a config::Config, size: TerminalSize) -> DimUnderlayKeyRef<'a> {
        DimUnderlayKeyRef {
            theme_name: &config.ui.color_theme,
            theme_background: config.ui.theme_background,
            terminal_size: size,
        }
    }

    /// Drive one full pass through the dim-cache pipeline:
    /// reconcile → compose. Returns the produced frame. Mirrors
    /// what `write_dirty_frame` does, minus the terminal write.
    fn compose_one_frame(
        state: &mut AppState,
        config: &config::Config,
        theme: &theme::Theme,
        size: TerminalSize,
    ) -> String {
        let active = state.overlay.active.clone();
        let footprint = ModalFootprint::from_active(&active);
        let key = fixture_key(config, size);
        let mode = state.render.reconcile_dim_compose(footprint, key);
        compose_modal_frame(state, config, theme, &active, size, mode, key)
    }

    #[test]
    fn first_modal_frame_emits_clear_screen_and_underlay() {
        let config = config::Config::new();
        let theme = theme::Theme::new();
        let mut state = modal_state(crate::overlay::ActiveModal::Main(
            crate::overlay::main_menu::MainMenuState::new(),
        ));

        let out = compose_one_frame(&mut state, &config, &theme, fixture_size());
        assert!(
            out.starts_with(term::CLEAR_SCREEN),
            "first modal frame (cache miss) must begin with CLEAR_SCREEN; got prefix: {:?}",
            out.chars().take(10).collect::<String>(),
        );
        assert!(
            state.render.cached_dimmed_underlay().is_some(),
            "first modal frame must populate the dim cache",
        );
    }

    #[test]
    fn same_modal_navigation_emits_modal_only_no_clear_screen() {
        // Regression guard for the mouse-text-selection invariant:
        // navigating *within* a single modal must not re-emit
        // CLEAR_SCREEN or the underlay.
        let config = config::Config::new();
        let theme = theme::Theme::new();
        let mut state = modal_state(crate::overlay::ActiveModal::Main(
            crate::overlay::main_menu::MainMenuState::new(),
        ));

        let _first = compose_one_frame(&mut state, &config, &theme, fixture_size());
        let second = compose_one_frame(&mut state, &config, &theme, fixture_size());
        assert!(
            !second.contains(term::CLEAR_SCREEN),
            "same-modal repaint must NOT emit CLEAR_SCREEN anywhere; got: {second:?}",
        );
    }

    #[test]
    fn modal_kind_change_re_emits_underlay_with_clear_screen() {
        // The bug this fix targets: pressing m → Enter (open Help
        // or Options from Main) → Esc (back to Main) used to leave
        // the previous modal's outer ring visible because the
        // cache-hit branch only emitted the new (smaller) modal.
        // After the fix, a footprint change re-emits the cached
        // dimmed underlay (wiping the previous modal's pixels)
        // before painting the new modal.
        let config = config::Config::new();
        let theme = theme::Theme::new();
        let mut state = modal_state(crate::overlay::ActiveModal::Main(
            crate::overlay::main_menu::MainMenuState::new(),
        ));

        // Frame 1: open Main (BuildAndEmit — populates the cache).
        let _main_frame = compose_one_frame(&mut state, &config, &theme, fixture_size());
        let cached_underlay = state
            .render
            .cached_dimmed_underlay()
            .expect("Main frame populates the cache")
            .to_string();

        // Frame 2: switch to Options. Footprint changes from
        // Main → Options while dim stays true → EmitFromCache.
        // The cached underlay must appear in the output prefixed
        // by CLEAR_SCREEN, NOT just the Options modal alone.
        state.overlay.active =
            crate::overlay::ActiveModal::Options(crate::overlay::options::OptionsState::new(
                crate::overlay::ReturnTarget::Main(crate::overlay::main_menu::MainMenuState::new()),
            ));
        let kind_change_frame = compose_one_frame(&mut state, &config, &theme, fixture_size());
        assert!(
            kind_change_frame.starts_with(term::CLEAR_SCREEN),
            "modal-kind-change frame must begin with CLEAR_SCREEN; got prefix: {:?}",
            kind_change_frame.chars().take(10).collect::<String>(),
        );
        assert!(
            kind_change_frame.contains(&cached_underlay),
            "modal-kind-change frame must re-emit the cached dimmed underlay so the \
             previous modal's pixels are wiped",
        );
    }

    #[test]
    fn modal_kind_change_does_not_rebuild_underlay() {
        // The cached underlay must be reused (not regenerated)
        // across a kind change — widgets did not change while a
        // modal was on top, so the snapshot is still valid; we
        // only need to re-emit it. Asserting snapshot identity by
        // value: the cached String content is identical before
        // and after the kind change.
        let config = config::Config::new();
        let theme = theme::Theme::new();
        let mut state = modal_state(crate::overlay::ActiveModal::Main(
            crate::overlay::main_menu::MainMenuState::new(),
        ));

        let _main_frame = compose_one_frame(&mut state, &config, &theme, fixture_size());
        let before = state
            .render
            .cached_dimmed_underlay()
            .expect("Main frame populates the cache")
            .to_string();

        state.overlay.active =
            crate::overlay::ActiveModal::Help(crate::overlay::help::HelpState::new(
                crate::overlay::ReturnTarget::Main(crate::overlay::main_menu::MainMenuState::new()),
            ));
        let _help_frame = compose_one_frame(&mut state, &config, &theme, fixture_size());
        let after = state
            .render
            .cached_dimmed_underlay()
            .expect("cache must survive the kind change")
            .to_string();
        assert_eq!(
            before, after,
            "kind-change re-emit must not rebuild the dimmed underlay",
        );
    }

    #[test]
    fn options_to_options_edit_is_same_footprint() {
        // Options ↔ Options-with-edit-buffer share a footprint
        // (same modal box, only the box's interior content
        // changes). The dim-cache reconcile must treat this
        // transition as same-modal, returning ModalOnly so no
        // CLEAR_SCREEN is emitted.
        let config = config::Config::new();
        let theme = theme::Theme::new();
        let mut opts =
            crate::overlay::options::OptionsState::new(crate::overlay::ReturnTarget::Normal);
        let mut state = modal_state(crate::overlay::ActiveModal::Options(opts.clone()));

        let _first = compose_one_frame(&mut state, &config, &theme, fixture_size());

        // Enter the inline-edit sub-state. ActiveModal::Options
        // discriminant is unchanged; OverlayKind would flip from
        // Options to OptionsEdit but ModalFootprint stays Options.
        opts.enter_edit(crate::overlay::options::edit::OptionEditState::placeholder());
        state.overlay.active = crate::overlay::ActiveModal::Options(opts);
        let edit_frame = compose_one_frame(&mut state, &config, &theme, fixture_size());
        assert!(
            !edit_frame.contains(term::CLEAR_SCREEN),
            "Options ↔ OptionsEdit shares a footprint; transition must NOT emit \
             CLEAR_SCREEN; got: {edit_frame:?}",
        );
    }

    #[test]
    fn modal_close_drops_cache_and_skips_compose() {
        // Closing the modal entirely (dim_now → false) drops the
        // cached underlay so the next modal-open transition
        // rebuilds. We can observe this via reconcile's return
        // value: after close, the next compose call for a fresh
        // modal must be BuildAndEmit (cache empty), not
        // EmitFromCache.
        let config = config::Config::new();
        let size = fixture_size();
        let mut state = make_state();
        let key = fixture_key(&config, size);

        // Open Main → BuildAndEmit (populates the cache).
        assert_eq!(
            state
                .render
                .reconcile_dim_compose(ModalFootprint::Main, key),
            DimComposeMode::BuildAndEmit,
        );
        state
            .render
            .store_dimmed_underlay("dummy".to_string(), key.to_owned());

        // Close → Skip, snapshot dropped.
        assert_eq!(
            state
                .render
                .reconcile_dim_compose(ModalFootprint::NoModal, key),
            DimComposeMode::Skip,
        );
        assert!(
            state.render.cached_dimmed_underlay().is_none(),
            "modal close must drop the dim-cache snapshot",
        );

        // Re-open Main → BuildAndEmit again (not EmitFromCache).
        assert_eq!(
            state
                .render
                .reconcile_dim_compose(ModalFootprint::Main, key),
            DimComposeMode::BuildAndEmit,
        );
    }
}
