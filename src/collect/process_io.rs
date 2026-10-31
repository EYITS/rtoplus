//! Optional session-scoped kernel network/disk tracing. Never installs a service.
use crate::domain::process::ProcInfo;
use std::{
    collections::HashMap,
    mem::size_of,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU8, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};
use windows::{
    Win32::System::Diagnostics::Etw::*,
    core::{GUID, PCWSTR, PWSTR},
};
static ENABLED: AtomicU8 = AtomicU8::new(0);
pub(crate) fn set_enabled(net: bool, disk: bool) {
    ENABLED.store(u8::from(net) | (u8::from(disk) << 1), Ordering::Relaxed);
}
const TCP: GUID = GUID::from_u128(0x9a280ac0_c8e0_11d1_84e2_00c04fb998a2);
const UDP: GUID = GUID::from_u128(0xbf3a50c5_a9c9_4988_a005_2df0b7c80f80);
const DISK: GUID = GUID::from_u128(0x3d6fa8d4_fe05_11d0_9dda_00c04fd7ba7c);
#[derive(Default)]
struct Counts {
    net: HashMap<u32, u64>,
    disk: HashMap<u32, u64>,
}
struct Session {
    control: CONTROLTRACE_HANDLE,
    trace: PROCESSTRACE_HANDLE,
    worker: Option<JoinHandle<()>>,
    counts: Arc<Mutex<Counts>>,
    props: Vec<u64>,
    name: Vec<u16>,
    mask: u8,
}
impl Drop for Session {
    fn drop(&mut self) {
        // SAFETY: stop only our uniquely named session, then unblock/join its consumer.
        unsafe {
            let _ = ControlTraceW(
                self.control,
                PCWSTR(self.name.as_ptr()),
                self.props.as_mut_ptr().cast(),
                EVENT_TRACE_CONTROL_STOP,
            );
            let _ = CloseTrace(self.trace);
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
impl Session {
    fn start(mask: u8) -> Option<Self> {
        use windows::Win32::Foundation::{HANDLE, LUID};
        use windows::Win32::{Security::*, System::Threading::*};
        // SAFETY: changes only privileges already granted to this process token.
        unsafe {
            let mut raw = HANDLE::default();
            if OpenProcessToken(
                GetCurrentProcess(),
                TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
                &mut raw,
            )
            .is_ok()
                && let Some(token) = crate::collect::win::OwnedHandle::new(raw)
            {
                let mut luid = LUID::default();
                if LookupPrivilegeValueW(
                    None,
                    windows::core::w!("SeSystemProfilePrivilege"),
                    &mut luid,
                )
                .is_ok()
                {
                    let privileges = TOKEN_PRIVILEGES {
                        PrivilegeCount: 1,
                        Privileges: [LUID_AND_ATTRIBUTES {
                            Luid: luid,
                            Attributes: SE_PRIVILEGE_ENABLED,
                        }],
                    };
                    let _ =
                        AdjustTokenPrivileges(token.get(), false, Some(&privileges), 0, None, None);
                }
            }
        }

        let name: Vec<u16> = format!("rtop-process-io-{}\0", std::process::id())
            .encode_utf16()
            .collect();
        let bytes = size_of::<EVENT_TRACE_PROPERTIES>() + name.len() * 2;
        let mut props = vec![0u64; bytes.div_ceil(8)];
        let mut control = CONTROLTRACE_HANDLE::default();
        // SAFETY: aligned allocation includes the properties and trailing logger name.
        unsafe {
            let p = &mut *props.as_mut_ptr().cast::<EVENT_TRACE_PROPERTIES>();
            p.Wnode.BufferSize = bytes as u32;
            p.Wnode.Guid = SystemTraceControlGuid;
            p.Wnode.Flags = 0x20000;
            p.Wnode.ClientContext = 1;
            p.BufferSize = 64;
            p.MinimumBuffers = 2;
            p.MaximumBuffers = 8;
            p.FlushTimer = 1;
            p.LogFileMode = EVENT_TRACE_REAL_TIME_MODE | EVENT_TRACE_SYSTEM_LOGGER_MODE;
            p.EnableFlags = EVENT_TRACE_FLAG(
                if mask & 1 != 0 { 0x10000 } else { 0 } | if mask & 2 != 0 { 0x100 } else { 0 },
            );
            p.LoggerNameOffset = size_of::<EVENT_TRACE_PROPERTIES>() as u32;
            std::ptr::copy_nonoverlapping(
                name.as_ptr(),
                props
                    .as_mut_ptr()
                    .cast::<u8>()
                    .add(size_of::<EVENT_TRACE_PROPERTIES>())
                    .cast::<u16>(),
                name.len(),
            );
            let error = StartTraceW(
                &mut control,
                PCWSTR(name.as_ptr()),
                props.as_mut_ptr().cast(),
            );
            if error.0 != 0 {
                tracing::debug!(
                    code = error.0,
                    "Process I/O tracing unavailable; run elevated"
                );
                return None;
            }
        }
        let counts = Arc::new(Mutex::new(Counts::default()));
        let mut logfile = EVENT_TRACE_LOGFILEW {
            LoggerName: PWSTR(name.as_ptr().cast_mut()),
            Context: Arc::as_ptr(&counts).cast_mut().cast(),
            ..Default::default()
        };
        logfile.Anonymous1.ProcessTraceMode =
            PROCESS_TRACE_MODE_REAL_TIME | PROCESS_TRACE_MODE_EVENT_RECORD;
        logfile.Anonymous2.EventRecordCallback = Some(event);
        // SAFETY: callbacks reference counts, held by Session and the worker until join.
        let trace = unsafe { OpenTraceW(&mut logfile) };
        if trace.Value == u64::MAX {
            // SAFETY: this session was started above and belongs exclusively to us.
            unsafe {
                let _ = ControlTraceW(
                    control,
                    PCWSTR(name.as_ptr()),
                    props.as_mut_ptr().cast(),
                    EVENT_TRACE_CONTROL_STOP,
                );
            }
            return None;
        }
        let keep = Arc::clone(&counts);
        let worker = std::thread::spawn(move || {
            // SAFETY: trace stays open until Session stops it and joins this thread.
            unsafe {
                let _ = ProcessTrace(&[trace], None, None);
            }
            drop(keep);
        });
        Some(Self {
            control,
            trace,
            worker: Some(worker),
            counts,
            props,
            name,
            mask,
        })
    }
}
fn property(record: *const EVENT_RECORD, name: PCWSTR) -> Option<u32> {
    let descriptor = PROPERTY_DATA_DESCRIPTOR {
        PropertyName: name.0 as u64,
        ArrayIndex: u32::MAX,
        ..Default::default()
    };
    let mut value = [0u8; 4];
    // SAFETY: callback record and UTF-16 property name remain live; output is 4 bytes.
    (unsafe { TdhGetProperty(record, None, &[descriptor], &mut value) } == 0)
        .then(|| u32::from_ne_bytes(value))
}
unsafe extern "system" fn event(record: *mut EVENT_RECORD) {
    // SAFETY: ETW invokes this callback with a live record and our stable context.
    let record = unsafe { &*record };
    let provider = record.EventHeader.ProviderId;
    let opcode = record.EventHeader.EventDescriptor.Opcode;
    let data = if record.UserData.is_null() {
        &[][..]
    } else {
        // SAFETY: ETW supplies exactly UserDataLength readable payload bytes.
        unsafe {
            std::slice::from_raw_parts(record.UserData.cast::<u8>(), record.UserDataLength as usize)
        }
    };
    let sample = if (provider == TCP || provider == UDP)
        && matches!(opcode, 10 | 11 | 26 | 27)
        && data.len() >= 8
    {
        Some((
            true,
            u32::from_ne_bytes(data[..4].try_into().unwrap()),
            u32::from_ne_bytes(data[4..8].try_into().unwrap()),
        ))
    } else if provider == DISK && matches!(opcode, 10 | 11) {
        use windows::Win32::System::Threading::*;
        let Some(tid) = property(record, windows::core::w!("IssuingThreadId")) else {
            return;
        };
        let Some(bytes) = property(record, windows::core::w!("TransferSize")) else {
            return;
        };
        // SAFETY: query-only thread handle is owned and closed on every path.
        let pid = unsafe {
            OpenThread(THREAD_QUERY_LIMITED_INFORMATION, false, tid)
                .ok()
                .and_then(crate::collect::win::OwnedHandle::new)
                .map(|h| GetProcessIdOfThread(h.get()))
        };
        pid.map(|pid| (false, pid, bytes))
    } else {
        None
    };
    if let Some((net, pid, bytes)) = sample
        && pid > 0
    {
        // SAFETY: Session and worker hold the Arc backing this pointer until callbacks stop.
        let counts = unsafe { &*record.UserContext.cast::<Mutex<Counts>>() };
        if let Ok(mut counts) = counts.lock() {
            let map = if net {
                &mut counts.net
            } else {
                &mut counts.disk
            };
            let total = map.entry(pid).or_default();
            *total = total.saturating_add(u64::from(bytes));
        }
    }
}
#[derive(Default)]
pub(crate) struct ProcessIo {
    session: Option<Session>,
    attempted: Option<Instant>,
    last: Option<Instant>,
}

impl ProcessIo {
    pub(crate) fn collect(&mut self, procs: &mut [ProcInfo]) {
        let mask = ENABLED.load(Ordering::Relaxed);
        if self.session.as_ref().is_some_and(|s| s.mask != mask) || mask == 0 {
            self.session = None;
            self.attempted = None;
            self.last = None;
        }
        if mask == 0 {
            return;
        }
        if self.session.is_none() {
            if self
                .attempted
                .is_some_and(|t| t.elapsed() < Duration::from_secs(30))
            {
                return;
            }
            self.attempted = Some(Instant::now());
            self.session = Session::start(mask);
            self.last = Some(Instant::now());
            return;
        }
        let session = self.session.as_mut().unwrap();
        let now = Instant::now();
        let elapsed = now
            .duration_since(self.last.replace(now).unwrap())
            .as_secs_f64()
            .max(0.001);
        // SAFETY: querying our session writes within its owned properties buffer.
        let valid = unsafe {
            ControlTraceW(
                session.control,
                PCWSTR(session.name.as_ptr()),
                session.props.as_mut_ptr().cast(),
                EVENT_TRACE_CONTROL_QUERY,
            )
            .0 == 0
                && (*session.props.as_ptr().cast::<EVENT_TRACE_PROPERTIES>()).EventsLost == 0
        };
        if let Ok(mut counts) = session.counts.lock() {
            if valid {
                for proc in procs {
                    if mask & 1 != 0 {
                        proc.net_bps =
                            Some(counts.net.get(&proc.pid).copied().unwrap_or(0) as f64 / elapsed);
                    }
                    if mask & 2 != 0 {
                        proc.disk_bps =
                            Some(counts.disk.get(&proc.pid).copied().unwrap_or(0) as f64 / elapsed);
                    }
                }
            }
            counts.net.clear();
            counts.disk.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn network_callback_counts_bytes_for_payload_pid_and_ignores_other_events() {
        let counts = Mutex::new(Counts::default());
        let mut payload = [0u8; 8];
        payload[..4].copy_from_slice(&42u32.to_ne_bytes());
        payload[4..].copy_from_slice(&512u32.to_ne_bytes());
        let mut record = EVENT_RECORD {
            UserContext: std::ptr::from_ref(&counts).cast_mut().cast(),
            UserData: payload.as_mut_ptr().cast(),
            UserDataLength: 8,
            ..Default::default()
        };
        record.EventHeader.ProviderId = UDP;
        record.EventHeader.ProcessId = 999;
        record.EventHeader.EventDescriptor.Opcode = 10;
        // SAFETY: test record, context, and payload are live across synchronous callbacks.
        unsafe {
            event(&mut record);
        }
        assert_eq!(counts.lock().unwrap().net.get(&42), Some(&512));
        assert!(counts.lock().unwrap().disk.is_empty());
        record.UserDataLength = 4;
        // SAFETY: shorter payload remains valid but must be ignored.
        unsafe {
            event(&mut record);
        }
        assert_eq!(counts.lock().unwrap().net.get(&42), Some(&512));
    }
}
