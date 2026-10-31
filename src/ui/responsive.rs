//! Borderless renderers used only when a widget's full presentation cannot
//! fit its allocated rectangle. They reuse the current collector snapshots
//! and process display entries; no extra collection or timers are involved.

use crate::app::RenderParams;
use crate::domain::process::{ProcDisplayEntry, ProcInfo};
use crate::domain::widget_kind::WidgetKind;
use crate::draw::buffer::AnsiBuffer;
use crate::draw::graph::{Graph, GraphMode};
use crate::draw::layout::WidgetDimensions;
use crate::theme::Theme;
use crate::theme_keys as tc;
use crate::tools;

/// Dispatch one compact slot. The caller retains per-widget dirty gating.
pub(crate) fn render(kind: WidgetKind, params: &RenderParams<'_>, output: &mut String) {
    let Some(dim) = params.layout.dims_for(kind) else {
        return;
    };
    let mut canvas = Canvas::new(dim, params.theme);
    match kind {
        WidgetKind::Cpu => cpu(&mut canvas, params),
        WidgetKind::Mem => memory(&mut canvas, params),
        WidgetKind::Gpu => gpu(&mut canvas, params),
        WidgetKind::Net => network(&mut canvas, params),
        WidgetKind::Disk => disk(&mut canvas, params),
        WidgetKind::Proc => processes(&mut canvas, params),
        WidgetKind::Statusbar => statusbar(&mut canvas, params),
    }
    output.push_str(&canvas.buf.finish());
}

struct Canvas<'a> {
    buf: AnsiBuffer,
    dim: &'a WidgetDimensions,
    theme: &'a Theme,
}

impl<'a> Canvas<'a> {
    fn new(dim: &'a WidgetDimensions, theme: &'a Theme) -> Self {
        let mut buf = AnsiBuffer::new();
        let blank = " ".repeat(dim.width);
        for row in 0..dim.height {
            buf.mv(dim.x + 1, dim.y + row + 1).reset().text(&blank);
        }
        Self { buf, dim, theme }
    }

    fn line(&mut self, row: usize, text: &str, color: &str) {
        if row >= self.dim.height || self.dim.width == 0 {
            return;
        }
        self.buf
            .mv(self.dim.x + 1, self.dim.y + row + 1)
            .reset()
            .color(color)
            .text(&tools::ljust(text, self.dim.width, true))
            .reset();
    }

    fn graph_line(&mut self, row: usize, text: &str) {
        if row >= self.dim.height {
            return;
        }
        // Graph padding uses CUF (cursor-forward), not literal spaces.
        // Its width is already exactly the requested graph width; padding
        // it with a text-width helper would overwrite the next column.
        self.buf
            .mv(self.dim.x + 1, self.dim.y + row + 1)
            .text(text)
            .reset();
    }

    /// Render just the current meter value, avoiding the 101-value cache
    /// used by full widgets when their entire presentation is available.
    fn meter(&mut self, row: usize, value: i64, gradient_key: tc::GradientKey) {
        if row >= self.dim.height || self.dim.width == 0 {
            return;
        }
        let filled = self.dim.width.saturating_mul(value.clamp(0, 100) as usize) / 100;
        let gradient = self.theme.gradient(gradient_key);
        self.buf.mv(self.dim.x + 1, self.dim.y + row + 1);
        for col in 0..self.dim.width {
            let color = if col < filled {
                crate::theme::gradient_color(gradient, (col * 100 / self.dim.width) as i32)
            } else {
                self.theme.color(tc::METER_BG)
            };
            self.buf.color(color).text("■");
        }
        self.buf.reset();
    }
}

fn bytes(value: u64, params: &RenderParams<'_>) -> String {
    tools::floating_humanizer(value, true, 0, false, false, params.config.ui.base_10_sizes)
}

fn cpu(canvas: &mut Canvas<'_>, params: &RenderParams<'_>) {
    let Some(snapshot) = params.cpu else { return };
    let info = &snapshot.info;
    let value = info.cpu_percent.total.back().copied().unwrap_or(0);
    let title = if canvas.dim.width >= 26 {
        format!("CPU {value}%  {} cores  {}", info.core_count, info.cpu_hz)
    } else {
        format!("CPU {value}%")
    };
    canvas.line(0, &title, params.theme.color(tc::CPU_WIDGET));
    if canvas.dim.height > 1 {
        let mode = GraphMode::from_config(
            params.config.cpu.graph_symbol_cpu,
            params.config.ui.graph_symbol,
        );
        let mut graph = Graph::new(canvas.dim.width, canvas.dim.height - 1, mode, false, 100, 0);
        for (row, text) in graph
            .render_rows(
                &info.cpu_percent.total,
                params.theme.gradient(tc::GRAD_CPU_UPPER),
            )
            .iter()
            .enumerate()
        {
            canvas.graph_line(row + 1, text);
        }
    }
}

fn memory(canvas: &mut Canvas<'_>, params: &RenderParams<'_>) {
    let Some(snapshot) = params.mem else { return };
    let info = &snapshot.info;
    let value = info.percent.used.back().copied().unwrap_or(0);
    let title = if canvas.dim.width >= 18 {
        format!(
            "RAM {} / {} {value}%",
            bytes(info.stats.used, params),
            bytes(params.total_mem, params)
        )
    } else {
        format!("RAM {value}%")
    };
    canvas.line(0, &title, params.theme.color(tc::MEM_WIDGET));
    canvas.meter(1, value, tc::GRAD_USED);
    if params.config.mem.show_swap && info.stats.swap_total > 0 {
        canvas.line(
            2,
            &format!(
                "Swap {} / {}",
                bytes(info.stats.swap_used, params),
                bytes(info.stats.swap_total, params)
            ),
            params.theme.color(tc::MAIN_FG),
        );
    }
}

fn gpu(canvas: &mut Canvas<'_>, params: &RenderParams<'_>) {
    let Some(snapshot) = params.gpu else { return };
    let Some(info) = snapshot
        .devices
        .iter()
        .find(|d| d.stable_id == params.selected_gpu_iface)
        .or_else(|| snapshot.devices.first())
    else {
        return;
    };
    let value = info.gpu_percent.utilization.back().copied().unwrap_or(0);
    let title = if canvas.dim.width >= 24 {
        format!(
            "GPU {value}%  VRAM {}/{}",
            bytes(info.mem_used, params),
            bytes(info.mem_total, params)
        )
    } else {
        format!("GPU {value}%")
    };
    canvas.line(0, &title, params.theme.color(tc::GPU_WIDGET));
    canvas.meter(1, value, tc::GRAD_GPU);
    canvas.line(2, &info.name, params.theme.color(tc::TITLE));
    canvas.line(
        3,
        &format!(
            "{}MHz {}°C",
            info.gpu_clock_speed,
            info.temp.back().copied().unwrap_or(0)
        ),
        params.theme.color(tc::MAIN_FG),
    );
}

fn network(canvas: &mut Canvas<'_>, params: &RenderParams<'_>) {
    let Some(snapshot) = params.net else { return };
    let Some(info) = snapshot
        .nets
        .iter()
        .find(|n| n.stable_id == params.selected_net_iface)
        .or_else(|| snapshot.nets.first())
    else {
        return;
    };
    canvas.line(
        0,
        &format!(
            "NET ↓{}/s ↑{}/s",
            bytes(info.stat.download.speed, params),
            bytes(info.stat.upload.speed, params)
        ),
        params.theme.color(tc::NET_WIDGET),
    );
    canvas.line(1, &info.description, params.theme.color(tc::TITLE));
}

fn disk(canvas: &mut Canvas<'_>, params: &RenderParams<'_>) {
    let Some(snapshot) = params.disk else { return };
    let filter = crate::domain::disk::DiskFilter::parse(&params.config.disk.disk_filter);
    for (row, info) in snapshot
        .info
        .disks
        .iter()
        .filter(|d| filter.matches(&d.name))
        .take(canvas.dim.height)
        .enumerate()
    {
        let text = if params.view.io_mode || params.config.disk.disk_io_mode {
            format!(
                "{} R {}/s W {}/s",
                info.name,
                bytes(info.read_bytes_per_sec, params),
                bytes(info.write_bytes_per_sec, params)
            )
        } else {
            format!(
                "{} {}% {}/{}",
                info.name,
                info.used_percent,
                bytes(info.used, params),
                bytes(info.total, params)
            )
        };
        canvas.line(row, &text, params.theme.color(tc::DISK_WIDGET));
    }
}

/// Compact rows reserve one column header and, when there is room, two
/// detail lines. This is shared with selection clamping before rendering.
pub(crate) fn process_row_count(height: usize, has_detail: bool) -> usize {
    height.saturating_sub(usize::from(height > 1) + detail_rows(height, has_detail))
}

fn detail_rows(height: usize, has_detail: bool) -> usize {
    if has_detail && height >= 6 { 2 } else { 0 }
}

struct Columns {
    name: usize,
    pid: usize,
    command: usize,
    cpu: usize,
    memory: usize,
    gpu: usize,
    net: usize,
    disk: usize,
}

impl Columns {
    fn configured(width: usize, gpu: bool, net: bool, disk: bool) -> Self {
        if !gpu && !net && !disk {
            return Self::new(width);
        }
        let mut cols = Self {
            name: 0,
            pid: 0,
            command: 0,
            cpu: 3,
            memory: 4,
            gpu: if gpu { 4 } else { 0 },
            net: if net { 5 } else { 0 },
            disk: if disk { 5 } else { 0 },
        };
        if !gpu && width >= 64 {
            cols.pid = 7;
        }
        let budget = width.saturating_sub(1);
        let used = |c: &Self| {
            [c.pid, c.command, c.cpu, c.memory, c.gpu, c.net, c.disk]
                .iter()
                .filter(|&&v| v > 0)
                .map(|v| v + 1)
                .sum::<usize>()
        };
        for drop in 0..7 {
            if used(&cols) + 3 <= budget {
                break;
            }
            match drop {
                0 => cols.pid = 0,
                1 => cols.disk = 0,
                2 => cols.net = 0,
                3 => cols.cpu = 0,
                4 => cols.memory = 0,
                5 => cols.gpu = 0,
                _ => {}
            }
        }
        cols.name = budget.saturating_sub(used(&cols));
        cols
    }
    fn new(width: usize) -> Self {
        let pid = if width >= 44 { 7 } else { 0 };
        let command = if width >= 72 { width / 4 } else { 0 };
        let cpu = if width >= 18 { 5 } else { 0 };
        let memory = if width >= 10 { 6 } else { 0 };
        let gaps = [pid, command, cpu, memory]
            .iter()
            .filter(|&&w| w > 0)
            .count();
        let name = width.saturating_sub(1 + pid + command + cpu + memory + gaps);
        Self {
            name,
            pid,
            command,
            cpu,
            memory,
            gpu: 0,
            net: 0,
            disk: 0,
        }
    }

    fn row(&self, fields: [&str; 8]) -> String {
        let mut text = tools::ljust(fields[0], self.name, true);
        for (field, width) in fields[1..].iter().zip([
            self.pid,
            self.command,
            self.cpu,
            self.memory,
            self.gpu,
            self.net,
            self.disk,
        ]) {
            if width > 0 {
                text.push(' ');
                text.push_str(&tools::rjust(field, width, true));
            }
        }
        text
    }
}

fn process_name(entry: &ProcDisplayEntry, proc: &ProcInfo, width: usize) -> String {
    if let Some(group) = &entry.group {
        let suffix = format!(" ({})", group.count);
        let prefix = if group.expanded { "v " } else { "> " };
        if width < 2 + tools::ulen(&suffix) {
            return format!("{}{}", prefix.trim_end(), suffix.trim_start());
        }
        let available = width.saturating_sub(2 + tools::ulen(&suffix));
        format!("{prefix}{}{suffix}", tools::uresize(&group.name, available))
    } else {
        format!("{}{}", entry.prefix, proc.name)
    }
}

fn process_cpu(raw: f64, params: &RenderParams<'_>) -> f64 {
    if !raw.is_finite() {
        return 0.0;
    }
    let cores = params.core_count.max(1) as f64;
    if params.view.proc_per_core {
        raw.clamp(0.0, cores * 100.0)
    } else {
        (raw / cores).clamp(0.0, 100.0)
    }
}

fn process_memory(raw: u64, params: &RenderParams<'_>) -> String {
    if params.config.proc.proc_mem_bytes {
        bytes(raw, params)
    } else {
        let percent = if params.total_mem > 0 {
            (raw as f64 * 100.0 / params.total_mem as f64).clamp(0.0, 100.0)
        } else {
            0.0
        };
        format!("{percent:.1}%")
    }
}

pub(crate) fn rate(value: Option<f64>) -> String {
    value
        .map(|v| tools::floating_humanizer(v.max(0.0) as u64, true, 0, false, false, false))
        .unwrap_or_else(|| "--".into())
}

fn processes(canvas: &mut Canvas<'_>, params: &RenderParams<'_>) {
    let columns = Columns::configured(
        canvas.dim.width,
        params.view.proc_gpu,
        params.view.proc_net,
        params.view.proc_disk,
    );
    let mut row = 0;
    if canvas.dim.height > 1 {
        let header = if let Some((name, force)) = params.armed_terminate {
            format!(
                "{}: {} to confirm",
                tools::uresize(name, canvas.dim.width.saturating_sub(18)),
                if force { "T" } else { "t" }
            )
        } else if params.is_filtering || !params.view.proc_filter.is_empty() {
            format!("filter: {}", params.view.proc_filter)
        } else {
            format!(
                " {}",
                columns.row([
                    if params.proc_paused {
                        "Paused"
                    } else {
                        "Program"
                    },
                    "PID",
                    "Command",
                    "CPU%",
                    "Memory",
                    "GPU%",
                    "Net/s",
                    "Disk/s"
                ])
            )
        };
        canvas.line(row, &header, params.theme.color(tc::PROC_WIDGET));
        row += 1;
    }
    if detail_rows(canvas.dim.height, params.detail_view.is_some()) > 0
        && let Some(detail) = params.detail_view
    {
        canvas.line(
            row,
            &format!(
                "PID {} {}{}",
                detail.proc.pid,
                detail.proc.name,
                if detail.dead { " [exited]" } else { "" }
            ),
            params.theme.color(tc::HI_FG),
        );
        canvas.line(row + 1, &detail.proc.cmd, params.theme.color(tc::TITLE));
        row += 2;
    }
    let count = process_row_count(canvas.dim.height, params.detail_view.is_some());
    for (offset, entry) in params
        .proc_entries
        .iter()
        .skip(params.proc_start)
        .take(count)
        .enumerate()
    {
        let Some(proc) = params.proc_source.get(entry.proc_index) else {
            continue;
        };
        let pid = if entry.group.is_some() {
            "-".into()
        } else {
            proc.pid.to_string()
        };
        let name = process_name(entry, proc, columns.name);
        let cpu = format!(
            "{:.1}",
            process_cpu(entry.cpu_override.unwrap_or(proc.cpu_p), params)
        );
        let memory = process_memory(entry.mem_override.unwrap_or(proc.mem), params);
        let selected = params.proc_start + offset == params.proc_selected;
        let dead = entry.group.is_none() && params.dead_pids.contains(&proc.pid);
        let command = entry
            .group
            .as_ref()
            .map(|group| format!("{} processes", group.count));
        let text = format!(
            "{}{}",
            if selected {
                ">"
            } else if dead {
                "×"
            } else {
                " "
            },
            columns.row([
                &name,
                &pid,
                command.as_deref().unwrap_or(&proc.cmd),
                &cpu,
                &memory,
                &entry
                    .gpu_override
                    .or(proc.gpu_p)
                    .map(|v| format!("{v:.1}"))
                    .unwrap_or_else(|| "--".into()),
                &rate(entry.net_override.or(proc.net_bps)),
                &rate(entry.disk_override.or(proc.disk_bps))
            ])
        );
        let color = if selected {
            tc::HI_FG
        } else if dead {
            tc::INACTIVE_FG
        } else {
            tc::MAIN_FG
        };
        canvas.line(row + offset, &text, params.theme.color(color));
    }
}

fn statusbar(canvas: &mut Canvas<'_>, params: &RenderParams<'_>) {
    let config = &params.config.statusbar;
    let mut text = String::new();
    let mut add = |token: &str| {
        let gap = usize::from(!text.is_empty());
        if tools::ulen(&text) + gap + tools::ulen(token) <= canvas.dim.width {
            if gap > 0 {
                text.push(' ');
            }
            text.push_str(token);
        } else if text.is_empty() {
            text = tools::uresize(token, canvas.dim.width);
        }
    };
    if config.statusbar_show_menu {
        add("[menu]");
    }
    if config.statusbar_show_preset {
        add(&format!("[P {} p]", params.config.active_preset().name()));
    }
    if config.statusbar_show_update_interval {
        add(&format!("[{}ms]", params.update_ms));
    }
    if config.statusbar_show_clock {
        add(&tools::format_clock(&config.statusbar_clock_format));
    }
    if config.statusbar_show_uptime
        && let Some(snapshot) = params.statusbar
    {
        add(&format!(
            "up {}",
            tools::sec_to_dhms(snapshot.info.uptime_seconds, false, true)
        ));
    }
    canvas.line(0, &text, params.theme.color(tc::STATUSBAR_FG));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn independently_enabled_metrics_fit_small_viewports_and_gpu_hides_pid() {
        for width in 1..=160 {
            for mask in 0..8 {
                let cols = Columns::configured(width, mask & 1 != 0, mask & 2 != 0, mask & 4 != 0);
                if mask & 1 != 0 {
                    assert_eq!(cols.pid, 0);
                }
                let row = format!(
                    " {}",
                    cols.row([
                        "ChatGPT (16)",
                        "12345",
                        "command",
                        "9.9",
                        "512M",
                        "2.0",
                        "1.2M",
                        "5.4M"
                    ])
                );
                assert!(
                    tools::ulen(&row) <= width,
                    "width={width} mask={mask} row={row:?}"
                );
            }
        }
        assert!(Columns::configured(20, true, false, false).gpu > 0);
        assert!(Columns::configured(40, true, true, true).net > 0);
        assert!(Columns::configured(40, true, true, true).disk > 0);
    }

    #[test]
    fn compact_columns_fit_display_cells_and_drop_lower_priority_columns() {
        for width in 1..=160 {
            let columns = Columns::new(width);
            let text = format!(
                " {}",
                columns.row([
                    "程序çalışma",
                    "123456",
                    "long command",
                    "10.5",
                    "8.0G",
                    "1.0",
                    "2K",
                    "3K"
                ])
            );
            assert!(tools::ulen(&text) <= width, "width={width}: {text:?}");
        }
        assert_eq!(Columns::new(20).pid, 0);
        assert_eq!(Columns::new(20).command, 0);
        assert!(Columns::new(20).cpu > 0 && Columns::new(20).memory > 0);
        assert_eq!(Columns::new(10).cpu, 0);
    }

    #[test]
    fn compact_selection_row_budget_always_leaves_a_row_when_possible() {
        assert_eq!(process_row_count(0, false), 0);
        assert_eq!(process_row_count(1, true), 1);
        assert_eq!(process_row_count(2, true), 1);
        assert_eq!(process_row_count(6, true), 3);
        assert_eq!(process_row_count(8, false), 7);
    }
}
