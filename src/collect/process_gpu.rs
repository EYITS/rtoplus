//! On-demand WDDM process GPU counters. No query is collected while hidden.
use crate::domain::process::ProcInfo;
use std::{
    collections::HashMap,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};
static ENABLED: AtomicBool = AtomicBool::new(false);
pub(crate) fn set_enabled(value: bool) {
    ENABLED.store(value, Ordering::Relaxed);
}
#[repr(C)]
struct Value {
    status: u32,
    value: f64,
}
#[repr(C)]
struct Item {
    name: *const u16,
    value: Value,
}
#[link(name = "pdh")]
unsafe extern "system" {
    fn PdhOpenQueryW(source: *const u16, data: usize, query: *mut isize) -> i32;
    fn PdhAddEnglishCounterW(
        query: isize,
        path: *const u16,
        data: usize,
        counter: *mut isize,
    ) -> i32;
    fn PdhCollectQueryData(query: isize) -> i32;
    fn PdhGetFormattedCounterArrayW(
        counter: isize,
        format: u32,
        size: *mut u32,
        count: *mut u32,
        items: *mut Item,
    ) -> i32;
    fn PdhCloseQuery(query: isize) -> i32;
}
struct Query {
    handle: isize,
    counter: isize,
}
impl Drop for Query {
    fn drop(&mut self) {
        // SAFETY: this object exclusively owns the successful PDH query handle.
        unsafe {
            PdhCloseQuery(self.handle);
        }
    }
}
#[derive(Default)]
pub(crate) struct ProcessGpu {
    query: Option<Query>,
    attempted: Option<Instant>,
    buffer: Vec<u64>,
}
impl ProcessGpu {
    pub(crate) fn collect(&mut self, procs: &mut [ProcInfo]) {
        if !ENABLED.load(Ordering::Relaxed) {
            self.query = None;
            self.attempted = None;
            self.buffer = Vec::new();
            return;
        }
        if self.query.is_none() {
            if self
                .attempted
                .is_some_and(|t| t.elapsed() < Duration::from_secs(30))
            {
                return;
            }
            self.attempted = Some(Instant::now());
            let mut handle = 0;
            let path: Vec<u16> = "\\GPU Engine(*)\\Utilization Percentage\0"
                .encode_utf16()
                .collect();
            // SAFETY: out parameters and NUL-terminated UTF-16 path remain live.
            unsafe {
                if PdhOpenQueryW(std::ptr::null(), 0, &mut handle) != 0 {
                    return;
                }
                let mut query = Query { handle, counter: 0 };
                if PdhAddEnglishCounterW(handle, path.as_ptr(), 0, &mut query.counter) != 0 {
                    return;
                }
                PdhCollectQueryData(handle);
                self.query = Some(query);
            }
            return; // Utilization requires two samples separated by an interval.
        }
        let query = self.query.as_ref().unwrap();
        let (mut size, mut count) = (0, 0);
        // SAFETY: PDH owns valid handles; the first call only returns buffer size.
        unsafe {
            if PdhCollectQueryData(query.handle) != 0 {
                return;
            }
            if PdhGetFormattedCounterArrayW(
                query.counter,
                0x200,
                &mut size,
                &mut count,
                std::ptr::null_mut(),
            ) != 0x800007d2u32 as i32
            {
                return;
            }
            if size == 0 || size > 32 * 1024 * 1024 {
                return;
            }
            self.buffer.resize((size as usize).div_ceil(8), 0);
            if PdhGetFormattedCounterArrayW(
                query.counter,
                0x200,
                &mut size,
                &mut count,
                self.buffer.as_mut_ptr().cast(),
            ) != 0
            {
                return;
            }
            if count as usize > self.buffer.len() * 8 / std::mem::size_of::<Item>() {
                return;
            }
            let items =
                std::slice::from_raw_parts(self.buffer.as_ptr().cast::<Item>(), count as usize);
            let mut usage: HashMap<u32, HashMap<String, f64>> = HashMap::new();
            for item in items {
                if item.value.status > 1 || !item.value.value.is_finite() {
                    continue;
                }
                let start = item.name as usize;
                let base = self.buffer.as_ptr() as usize;
                let end = base + self.buffer.len() * 8;
                if start < base || start >= end || !start.is_multiple_of(2) {
                    continue;
                }
                let units = std::slice::from_raw_parts(item.name, (end - start) / 2);
                let length = units.iter().position(|&c| c == 0).unwrap_or(units.len());
                let name = String::from_utf16_lossy(&units[..length]);
                if let Some((pid, engine)) = instance(&name) {
                    *usage
                        .entry(pid)
                        .or_default()
                        .entry(engine.to_string())
                        .or_default() += item.value.value.max(0.0);
                }
            }
            for proc in procs {
                proc.gpu_engines = usage
                    .remove(&proc.pid)
                    .unwrap_or_default()
                    .into_iter()
                    .collect();
                proc.gpu_p = Some(
                    proc.gpu_engines
                        .iter()
                        .map(|(_, v)| *v)
                        .fold(0.0_f64, f64::max)
                        .clamp(0.0, 100.0),
                );
            }
        }
    }
}
fn instance(name: &str) -> Option<(u32, &str)> {
    let rest = name.strip_prefix("pid_")?;
    let (pid, engine) = rest.split_once('_')?;
    Some((pid.parse().ok()?, engine.split('#').next()?))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn instance_keys_share_engines_across_pids() {
        assert_eq!(
            instance("pid_12_luid_A_phys_0_eng_1_engtype_3D#1"),
            Some((12, "luid_A_phys_0_eng_1_engtype_3D"))
        );
        assert_eq!(instance("_Total"), None);
        assert_eq!(instance("pid_bad_engine"), None);
    }
}
