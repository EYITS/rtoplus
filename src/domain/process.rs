use std::fmt;
use std::sync::{Arc, OnceLock};

/// Share the unavailable-path sentinel across snapshots instead of allocating
/// a separate empty Arc for every protected process on every collection.
pub(crate) fn unknown_executable_path() -> Arc<str> {
    static EMPTY: OnceLock<Arc<str>> = OnceLock::new();
    Arc::clone(EMPTY.get_or_init(|| Arc::from("")))
}

/// Information about a single process.
#[derive(Debug, Clone)]
pub struct ProcInfo {
    /// Process ID.
    pub pid: u32,
    /// Process name (e.g. "chrome.exe").
    pub name: String,
    /// Full command line.
    pub cmd: String,
    /// Normalized executable image path, cached for this process lifetime.
    /// Empty when Windows denies access; never inferred from the display name.
    pub exe_path: Arc<str>,
    /// Process creation FILETIME, or zero when unavailable.
    pub started_at: u64,
    /// Thread count.
    pub threads: usize,
    /// Username of the process owner.
    pub user: String,
    /// Memory usage in bytes (working set).
    pub mem: u64,
    /// Current CPU usage percentage (may exceed 100% if per-core).
    pub cpu_p: f64,
    /// Current process state.
    pub state: ProcState,
    /// Process priority class.
    pub priority: PriorityClass,
    /// Parent process ID.
    pub ppid: u32,
    /// Total CPU time consumed (100ns intervals).
    pub cpu_time: u64,
    /// Total IO bytes read.
    pub io_read: u64,
    /// Total IO bytes written.
    pub io_write: u64,
}

impl Default for ProcInfo {
    fn default() -> Self {
        Self {
            pid: 0,
            name: String::new(),
            cmd: String::new(),
            exe_path: unknown_executable_path(),
            started_at: 0,
            threads: 0,
            user: String::new(),
            mem: 0,
            cpu_p: 0.0,
            state: ProcState::Unknown,
            priority: PriorityClass::Normal,
            ppid: 0,
            cpu_time: 0,
            io_read: 0,
            io_write: 0,
        }
    }
}

/// Stable identity of an application group, independent of its representative PID.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum AppGroupKey {
    Executable(Arc<str>),
    /// Only used for a verified same-name parent/child chain without an image path.
    Lineage {
        pid: u32,
        started_at: u64,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProcGroupSummary {
    pub key: AppGroupKey,
    pub count: usize,
    pub name: String,
    pub expanded: bool,
}

/// Display-only metadata for one visible process row.
#[derive(Debug, Clone, PartialEq)]
pub struct ProcDisplayEntry {
    /// Index into the collector's raw `procs` vector.
    pub proc_index: usize,
    /// Tree view prefix string (e.g. "├─ ", "│  └─ ").
    pub prefix: String,
    /// Depth in process tree (0 = root).
    pub depth: usize,
    /// Aggregated CPU% override (used when `proc_aggregate` is on in tree mode).
    pub cpu_override: Option<f64>,
    /// Aggregated memory override (used when `proc_aggregate` is on in tree mode).
    pub mem_override: Option<u64>,
    /// Present only for a synthetic application row. Such a row has no actionable PID.
    pub group: Option<ProcGroupSummary>,
}

impl ProcDisplayEntry {
    pub fn flat(proc_index: usize) -> Self {
        Self {
            proc_index,
            prefix: String::new(),
            depth: 0,
            cpu_override: None,
            mem_override: None,
            group: None,
        }
    }

    pub fn tree(proc_index: usize, prefix: String, depth: usize) -> Self {
        Self {
            proc_index,
            prefix,
            depth,
            cpu_override: None,
            mem_override: None,
            group: None,
        }
    }

    /// Resolve only an explicitly selected individual process. Display code may
    /// read `proc_index` for a group's label, but actions must use this method.
    pub fn process<'a>(&self, procs: &'a [ProcInfo]) -> Option<&'a ProcInfo> {
        if self.group.is_some() {
            None
        } else {
            procs.get(self.proc_index)
        }
    }
}

/// Process state on Windows.
///
/// Unlike Linux (R/S/D/Z/T/t/X/K/W/P), Windows has fewer observable states.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcState {
    Running,
    /// Every thread is parked in a suspended wait. Modern Windows
    /// suspends UWP and Store apps when they lose focus, and a
    /// debugger freeze looks the same, so this distinguishes "idle by
    /// choice" from "stopped by the system" for a process sitting at
    /// 0% CPU.
    Suspended,
    Unknown,
}

impl fmt::Display for ProcState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Running => write!(f, "Running"),
            Self::Suspended => write!(f, "Suspended"),
            Self::Unknown => write!(f, "Unknown"),
        }
    }
}

/// Windows process priority class.
///
/// Replaces Unix nice values (-20..19) with 6 discrete classes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PriorityClass {
    Idle = 0,
    BelowNormal = 1,
    Normal = 2,
    AboveNormal = 3,
    High = 4,
    Realtime = 5,
}

impl fmt::Display for PriorityClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Idle => write!(f, "Idle"),
            Self::BelowNormal => write!(f, "Below Normal"),
            Self::Normal => write!(f, "Normal"),
            Self::AboveNormal => write!(f, "Above Normal"),
            Self::High => write!(f, "High"),
            Self::Realtime => write!(f, "Realtime"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synthetic_group_cannot_resolve_an_actionable_process() {
        let procs = [ProcInfo {
            pid: 42,
            ..Default::default()
        }];
        let mut entry = ProcDisplayEntry::flat(0);
        assert_eq!(entry.process(&procs).unwrap().pid, 42);
        entry.group = Some(ProcGroupSummary {
            key: AppGroupKey::Executable(Arc::from("c:\\app.exe")),
            count: 2,
            name: "app".into(),
            expanded: false,
        });
        assert!(entry.process(&procs).is_none());
    }

    #[test]
    fn proc_state_display_names() {
        assert_eq!(ProcState::Running.to_string(), "Running");
        assert_eq!(ProcState::Unknown.to_string(), "Unknown");
    }

    #[test]
    fn priority_class_ordering() {
        assert!(PriorityClass::Idle < PriorityClass::BelowNormal);
        assert!(PriorityClass::BelowNormal < PriorityClass::Normal);
        assert!(PriorityClass::Normal < PriorityClass::AboveNormal);
        assert!(PriorityClass::AboveNormal < PriorityClass::High);
        assert!(PriorityClass::High < PriorityClass::Realtime);
    }

    #[test]
    fn priority_class_display() {
        assert_eq!(PriorityClass::Normal.to_string(), "Normal");
        assert_eq!(PriorityClass::Realtime.to_string(), "Realtime");
        assert_eq!(PriorityClass::Idle.to_string(), "Idle");
    }
}
