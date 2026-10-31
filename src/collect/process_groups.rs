//! Application grouping built solely from the collector's process snapshot.
//!
//! Executable identities are cached by the collector. Missing identities only
//! inherit from a verifiable, same-name parent chain; unrelated processes with
//! the same display name remain separate.

use super::process_display::{ParsedFilter, ProcSort, compare_procs, sort_proc_indices};
use crate::domain::process::{AppGroupKey, ProcDisplayEntry, ProcGroupSummary, ProcInfo};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// Normalize a Windows executable path without filesystem access. Called when
/// the collector first observes a process, rather than on every UI refresh.
pub fn normalize_executable_path(path: &str) -> Arc<str> {
    let normalized = path.replace('/', "\\").to_lowercase();
    if let Some(unc) = normalized.strip_prefix("\\\\?\\unc\\") {
        Arc::from(format!("\\\\{unc}"))
    } else {
        Arc::from(
            normalized
                .strip_prefix("\\\\?\\")
                .or_else(|| normalized.strip_prefix("\\??\\"))
                .unwrap_or(&normalized),
        )
    }
}

struct Group {
    first: usize,
    representative: usize,
    // Singletons need no member allocation. The first member is stored above.
    others: Vec<usize>,
    cpu: f64,
    memory: u64,
    threads: usize,
    gpu: HashMap<String, f64>,
    gpu_available: bool,
    net: Option<f64>,
    disk: Option<f64>,
    matches: bool,
    followed: bool,
}

fn gpu_total(group: &Group) -> f64 {
    if group.gpu_available {
        group
            .gpu
            .values()
            .copied()
            .fold(0.0, f64::max)
            .clamp(0.0, 100.0)
    } else {
        -1.0
    }
}

impl Group {
    fn new(index: usize, proc: &ProcInfo, matches: bool, followed_pid: u32) -> Self {
        Self {
            first: index,
            representative: index,
            others: Vec::new(),
            cpu: proc.cpu_p,
            memory: proc.mem,
            threads: proc.threads,
            gpu: proc.gpu_engines.iter().cloned().collect(),
            gpu_available: proc.gpu_p.is_some(),
            net: proc.net_bps,
            disk: proc.disk_bps,
            matches,
            followed: followed_pid != 0 && proc.pid == followed_pid,
        }
    }

    fn count(&self) -> usize {
        1 + self.others.len()
    }

    fn add(&mut self, index: usize, procs: &[ProcInfo], matches: bool, followed_pid: u32) {
        let proc = &procs[index];
        self.others.push(index);
        self.cpu += proc.cpu_p;
        self.net = self.net.zip(proc.net_bps).map(|(a, b)| a + b);
        self.disk = self.disk.zip(proc.disk_bps).map(|(a, b)| a + b);
        self.gpu_available |= proc.gpu_p.is_some();
        for (engine, value) in &proc.gpu_engines {
            *self.gpu.entry(engine.clone()).or_default() += value;
        }
        self.memory = self.memory.saturating_add(proc.mem);
        self.threads = self.threads.saturating_add(proc.threads);
        self.matches |= matches;
        self.followed |= followed_pid != 0 && proc.pid == followed_pid;
        if proc.pid < procs[self.representative].pid {
            self.representative = index;
        }
    }
}

/// Build group headers and optional individual PID rows. Filtering retains the
/// complete application when any member matches, so the header always reports
/// the application's full process count and resource totals.
pub fn build_grouped_proc_display_entries(
    procs: &[ProcInfo],
    sort_by: ProcSort,
    reversed: bool,
    filter: &str,
    expanded: &mut HashSet<AppGroupKey>,
    followed_pid: u32,
) -> Vec<ProcDisplayEntry> {
    let pid_indices: HashMap<u32, usize> = procs
        .iter()
        .enumerate()
        .map(|(index, proc)| (proc.pid, index))
        .collect();
    let parsed = ParsedFilter::parse(filter);
    let mut groups: HashMap<AppGroupKey, Group> = HashMap::with_capacity(procs.len());
    for (index, proc) in procs.iter().enumerate() {
        let key = application_key(index, procs, &pid_indices);
        let matches = filter.is_empty() || parsed.matches(proc);
        groups
            .entry(key)
            .and_modify(|group| group.add(index, procs, matches, followed_pid))
            .or_insert_with(|| Group::new(index, proc, matches, followed_pid));
    }

    // Filtered-out applications retain their expansion state. Exited
    // applications release it, bounding this runtime-only state to the snapshot.
    expanded.retain(|key| groups.contains_key(key));
    let mut groups: Vec<(AppGroupKey, Group)> = groups
        .into_iter()
        .filter(|(_, group)| group.matches)
        .collect();
    groups.sort_unstable_by(|(_, a), (_, b)| {
        let cmp = match sort_by {
            ProcSort::Gpu => gpu_total(a).total_cmp(&gpu_total(b)),
            ProcSort::Net => a.net.unwrap_or(-1.0).total_cmp(&b.net.unwrap_or(-1.0)),
            ProcSort::Disk => a.disk.unwrap_or(-1.0).total_cmp(&b.disk.unwrap_or(-1.0)),
            ProcSort::Cpu => a
                .cpu
                .partial_cmp(&b.cpu)
                .unwrap_or(std::cmp::Ordering::Equal),
            ProcSort::Memory => a.memory.cmp(&b.memory),
            ProcSort::Threads => a.threads.cmp(&b.threads),
            _ => compare_procs(&procs[a.representative], &procs[b.representative], sort_by),
        }
        .then_with(|| {
            procs[a.representative]
                .pid
                .cmp(&procs[b.representative].pid)
        });
        if reversed { cmp.reverse() } else { cmp }
    });

    let mut entries = Vec::with_capacity(procs.len());
    for (key, mut group) in groups {
        if group.count() == 1 {
            entries.push(ProcDisplayEntry::flat(group.first));
            continue;
        }
        if group.followed {
            expanded.insert(key.clone());
        }
        let is_expanded = expanded.contains(&key);
        let representative = &procs[group.representative];
        entries.push(ProcDisplayEntry {
            proc_index: group.representative,
            prefix: if is_expanded { "v " } else { "> " }.to_string(),
            depth: 0,
            cpu_override: Some(group.cpu),
            mem_override: Some(group.memory),
            net_override: group.net,
            disk_override: group.disk,
            gpu_override: group.gpu_available.then(|| {
                group
                    .gpu
                    .values()
                    .copied()
                    .fold(0.0_f64, f64::max)
                    .clamp(0.0, 100.0)
            }),
            group: Some(ProcGroupSummary {
                key,
                count: group.count(),
                name: application_name(&representative.name),
                expanded: is_expanded,
            }),
        });
        if is_expanded {
            group.others.push(group.first);
            sort_proc_indices(&mut group.others, procs, sort_by, reversed);
            for index in group.others {
                entries.push(ProcDisplayEntry::tree(index, "  ".to_string(), 1));
            }
        }
    }
    entries
}

pub(crate) fn members(procs: &[ProcInfo], key: &AppGroupKey) -> Vec<(u32, u64)> {
    let indices = procs.iter().enumerate().map(|(i, p)| (p.pid, i)).collect();
    procs
        .iter()
        .enumerate()
        .filter(|(i, _)| application_key(*i, procs, &indices) == *key)
        .map(|(_, p)| (p.pid, p.started_at))
        .collect()
}

fn application_name(name: &str) -> String {
    match name.rsplit_once('.') {
        Some((base, extension)) if !base.is_empty() && extension.eq_ignore_ascii_case("exe") => {
            base.to_string()
        }
        _ => name.to_string(),
    }
}

fn lineage_key(proc: &ProcInfo) -> AppGroupKey {
    AppGroupKey::Lineage {
        pid: proc.pid,
        started_at: proc.started_at,
    }
}

pub(super) fn application_key(
    origin: usize,
    procs: &[ProcInfo],
    pid_indices: &HashMap<u32, usize>,
) -> AppGroupKey {
    let mut index = origin;
    // The bounded walk handles corrupt/cyclic ancestry without recursion or
    // per-process visited allocations. On exhaustion no ancestry is trusted.
    for _ in 0..64 {
        let proc = &procs[index];
        if !proc.exe_path.is_empty() {
            return AppGroupKey::Executable(Arc::clone(&proc.exe_path));
        }
        let Some(&parent_index) = pid_indices.get(&proc.ppid) else {
            return lineage_key(proc);
        };
        let parent = &procs[parent_index];
        if parent_index == index
            || proc.started_at == 0
            || parent.started_at == 0
            || parent.started_at > proc.started_at
            || !parent.name.eq_ignore_ascii_case(&proc.name)
        {
            return lineage_key(proc);
        }
        index = parent_index;
    }
    lineage_key(&procs[origin])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn optional_metrics_aggregate_and_sort_by_complete_application() {
        let mut a = proc(1, "C:/a.exe", "a.exe", 1.0, 1);
        a.gpu_p = Some(20.0);
        a.gpu_engines = vec![("engine0".into(), 20.0)];
        a.net_bps = Some(10.0);
        a.disk_bps = Some(30.0);
        let mut b = a.clone();
        b.pid = 2;
        b.started_at = 2;
        b.gpu_engines = vec![("engine0".into(), 25.0), ("engine1".into(), 5.0)];
        b.net_bps = Some(20.0);
        let mut c = proc(3, "C:/c.exe", "c.exe", 2.0, 2);
        c.gpu_p = Some(35.0);
        c.gpu_engines = vec![("engine0".into(), 35.0)];
        c.net_bps = Some(25.0);
        c.disk_bps = Some(50.0);
        let procs = vec![a, b, c];
        for sorting in [ProcSort::Gpu, ProcSort::Net, ProcSort::Disk] {
            let entries = build_grouped_proc_display_entries(
                &procs,
                sorting,
                true,
                "",
                &mut HashSet::new(),
                0,
            );
            assert_eq!(entries[0].group.as_ref().unwrap().name, "a");
            assert_eq!(entries[0].gpu_override, Some(45.0));
            assert_eq!(entries[0].net_override, Some(30.0));
            assert_eq!(entries[0].disk_override, Some(60.0));
        }
        let key = AppGroupKey::Executable(normalize_executable_path("C:/a.exe"));
        assert_eq!(members(&procs, &key), vec![(1, 1), (2, 2)]);
    }

    fn proc(pid: u32, path: &str, name: &str, cpu: f64, mem: u64) -> ProcInfo {
        ProcInfo {
            pid,
            ppid: 9999,
            name: name.to_string(),
            cmd: format!("{name} --pid {pid}"),
            exe_path: normalize_executable_path(path),
            started_at: u64::from(pid),
            cpu_p: cpu,
            mem,
            ..Default::default()
        }
    }

    fn build(procs: &[ProcInfo]) -> Vec<ProcDisplayEntry> {
        build_grouped_proc_display_entries(procs, ProcSort::Cpu, true, "", &mut HashSet::new(), 0)
    }

    #[test]
    fn path_normalization_handles_case_separators_and_extended_paths() {
        assert_eq!(
            normalize_executable_path("C:/Apps/ChatGPT.EXE").as_ref(),
            "c:\\apps\\chatgpt.exe"
        );
        assert_eq!(
            normalize_executable_path("\\\\?\\C:\\Apps\\ChatGPT.exe").as_ref(),
            "c:\\apps\\chatgpt.exe"
        );
        assert_eq!(
            normalize_executable_path("\\\\?\\UNC\\Server\\Share\\APP.exe").as_ref(),
            "\\\\server\\share\\app.exe"
        );
        assert_eq!(
            normalize_executable_path("\\??\\C:\\APP.EXE").as_ref(),
            "c:\\app.exe"
        );
        assert_eq!(normalize_executable_path("").as_ref(), "");
    }

    #[test]
    fn same_name_different_installations_remain_separate() {
        let procs = vec![
            proc(1, "C:/AppA/app.exe", "app.exe", 1.0, 10),
            proc(2, "C:/AppB/app.exe", "app.exe", 2.0, 20),
            proc(3, "C:/APPA/APP.EXE", "app.exe", 3.0, 30),
        ];
        let entries = build(&procs);
        assert_eq!(entries.len(), 2);
        let group = entries[0].group.as_ref().unwrap();
        assert_eq!(group.count, 2);
        assert_eq!(group.name, "app");
        assert_eq!(entries[0].cpu_override, Some(4.0));
        assert_eq!(entries[0].mem_override, Some(40));
        assert_eq!(procs[entries[1].proc_index].pid, 2);
        assert!(entries[1].group.is_none());
    }

    #[test]
    fn unknown_unrelated_same_names_are_not_merged() {
        let entries = build(&[
            proc(1, "", "host.exe", 1.0, 10),
            proc(2, "", "host.exe", 2.0, 20),
        ]);
        assert_eq!(entries.len(), 2);
        assert!(entries.iter().all(|entry| entry.group.is_none()));
    }

    #[test]
    fn unknown_path_uses_verified_same_name_parent_identity() {
        let mut child = proc(2, "", "APP.EXE", 2.0, 20);
        child.ppid = 1;
        let procs = vec![proc(1, "C:/A/app.exe", "app.exe", 1.0, 10), child];
        let entries = build(&procs);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].group.as_ref().unwrap().count, 2);
        assert_eq!(entries[0].mem_override, Some(30));
    }

    #[test]
    fn unknown_same_name_lineage_groups_without_executable_path() {
        let mut child = proc(2, "", "app.exe", 2.0, 20);
        child.ppid = 1;
        let entries = build(&[proc(1, "", "app.exe", 1.0, 10), child]);
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0].group.as_ref().unwrap().key,
            AppGroupKey::Lineage {
                pid: 1,
                started_at: 1
            }
        );
    }

    #[test]
    fn parent_relationship_never_overrides_a_known_different_path() {
        let mut child = proc(2, "C:/B/app.exe", "app.exe", 2.0, 20);
        child.ppid = 1;
        let entries = build(&[proc(1, "C:/A/app.exe", "app.exe", 1.0, 10), child]);
        assert_eq!(entries.len(), 2);
        assert!(entries.iter().all(|entry| entry.group.is_none()));
    }

    #[test]
    fn newer_or_unverifiable_parent_cannot_supply_identity() {
        for parent_start in [0, 3] {
            let mut parent = proc(1, "C:/A/app.exe", "app.exe", 1.0, 10);
            parent.started_at = parent_start;
            let mut child = proc(2, "", "app.exe", 2.0, 20);
            child.ppid = 1;
            assert_eq!(build(&[parent, child]).len(), 2);
        }
    }

    #[test]
    fn differently_named_children_and_cycles_remain_individual() {
        let mut child = proc(2, "", "other.exe", 2.0, 20);
        child.ppid = 1;
        assert_eq!(
            build(&[proc(1, "C:/A/app.exe", "app.exe", 1.0, 10), child]).len(),
            2
        );
        let mut a = proc(1, "", "app.exe", 1.0, 10);
        let mut b = proc(2, "", "app.exe", 2.0, 20);
        a.ppid = 2;
        b.ppid = 1;
        a.started_at = 1;
        b.started_at = 1;
        assert_eq!(build(&[a, b]).len(), 2);
    }

    #[test]
    fn groups_sort_on_total_cpu_memory_and_threads_in_both_directions() {
        let mut procs = vec![
            proc(1, "C:/A/a.exe", "a.exe", 4.0, 40),
            proc(2, "C:/A/a.exe", "a.exe", 4.0, 40),
            proc(3, "C:/B/b.exe", "b.exe", 7.0, 70),
        ];
        procs[0].threads = 4;
        procs[1].threads = 4;
        procs[2].threads = 7;
        for sort in [ProcSort::Cpu, ProcSort::Memory, ProcSort::Threads] {
            for reversed in [false, true] {
                let entries = build_grouped_proc_display_entries(
                    &procs,
                    sort,
                    reversed,
                    "",
                    &mut HashSet::new(),
                    0,
                );
                assert_eq!(
                    entries[0].group.is_some(),
                    reversed,
                    "{sort:?} reverse={reversed}"
                );
            }
        }
    }

    #[test]
    fn expansion_exposes_individual_pids_and_filter_keeps_full_totals() {
        let procs = vec![
            proc(1, "C:/A/a.exe", "a.exe", 4.0, 40),
            proc(2, "C:/A/a.exe", "a.exe", 6.0, 60),
            proc(3, "C:/B/b.exe", "b.exe", 7.0, 70),
        ];
        let key = AppGroupKey::Executable(normalize_executable_path("C:/A/a.exe"));
        let mut expanded = HashSet::from([key]);
        let entries = build_grouped_proc_display_entries(
            &procs,
            ProcSort::Cpu,
            true,
            "--pid 2",
            &mut expanded,
            0,
        );
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].group.as_ref().unwrap().count, 2);
        assert_eq!(entries[0].cpu_override, Some(10.0));
        assert_eq!(entries[0].mem_override, Some(100));
        assert_eq!(entries[0].prefix, "v ");
        assert!(
            entries[1..]
                .iter()
                .all(|entry| entry.group.is_none() && entry.prefix == "  ")
        );
        assert_eq!(procs[entries[1].proc_index].pid, 2);
        assert_eq!(procs[entries[2].proc_index].pid, 1);
    }

    #[test]
    fn filtering_preserves_hidden_expansion_but_exit_clears_it() {
        let procs = vec![
            proc(1, "C:/A/a.exe", "a.exe", 4.0, 40),
            proc(2, "C:/A/a.exe", "a.exe", 6.0, 60),
        ];
        let key = AppGroupKey::Executable(normalize_executable_path("C:/A/a.exe"));
        let mut expanded = HashSet::from([key.clone()]);
        assert!(
            build_grouped_proc_display_entries(
                &procs,
                ProcSort::Cpu,
                false,
                "missing",
                &mut expanded,
                0
            )
            .is_empty()
        );
        assert!(expanded.contains(&key));
        assert!(
            build_grouped_proc_display_entries(&[], ProcSort::Cpu, false, "", &mut expanded, 0)
                .is_empty()
        );
        assert!(expanded.is_empty());
    }

    #[test]
    fn following_opens_containing_group_and_dynamic_snapshot_recounts() {
        let mut procs = vec![
            proc(1, "C:/A/a.exe", "a.exe", 4.0, 40),
            proc(2, "C:/A/a.exe", "a.exe", 6.0, 60),
        ];
        let mut expanded = HashSet::new();
        let entries =
            build_grouped_proc_display_entries(&procs, ProcSort::Cpu, true, "", &mut expanded, 2);
        assert_eq!(entries.len(), 3);
        assert!(entries[0].group.as_ref().unwrap().expanded);
        assert!(
            entries[1..]
                .iter()
                .any(|entry| procs[entry.proc_index].pid == 2)
        );
        procs.push(proc(3, "C:/A/a.exe", "a.exe", 1.0, 10));
        let entries =
            build_grouped_proc_display_entries(&procs, ProcSort::Cpu, true, "", &mut expanded, 0);
        assert_eq!(entries[0].group.as_ref().unwrap().count, 3);
        assert_eq!(entries[0].cpu_override, Some(11.0));
        procs.truncate(1);
        let entries =
            build_grouped_proc_display_entries(&procs, ProcSort::Cpu, true, "", &mut expanded, 0);
        assert_eq!(entries.len(), 1);
        assert!(entries[0].group.is_none());
    }

    #[test]
    fn totals_saturate_and_equal_sort_values_have_deterministic_order() {
        let procs = vec![
            proc(3, "C:/A/a.exe", "a.exe", 2.0, u64::MAX),
            proc(2, "C:/A/a.exe", "a.exe", 2.0, 10),
            proc(1, "C:/B/b.exe", "b.exe", 4.0, 10),
        ];
        let entries = build(&procs);
        assert_eq!(procs[entries[0].proc_index].pid, 2);
        assert_eq!(entries[0].mem_override, Some(u64::MAX));
        assert_eq!(procs[entries[1].proc_index].pid, 1);
        assert_eq!(build(&procs), entries);
    }
}
