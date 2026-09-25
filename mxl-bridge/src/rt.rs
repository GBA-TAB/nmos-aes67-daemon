//! Real-time scheduling for the audio period threads.
//!
//! On a host partitioned for real time (mxl-orchestrator `exclusive` CPU mode), the app owns its
//! cores - but a normal (SCHED_OTHER) thread can still be preempted there by the app's own other
//! threads (HTTP, registration, logging). SCHED_FIFO makes the period thread win every time.
//! Needs CAP_SYS_NICE (the orchestrator grants it to real-time kinds). Without it this logs a
//! warning and the thread simply keeps normal priority - never fatal. The kernel's RT throttling
//! (sched_rt_runtime_us, 95 % by default) still bounds a runaway thread.

const CAP_SYS_NICE: u32 = 23;
const LINUX_CAPABILITY_VERSION_3: u32 = 0x2008_0522;

#[repr(C)]
struct CapHeader {
    version: u32,
    pid: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CapData {
    effective: u32,
    permitted: u32,
    inheritable: u32,
}

/// The app runs as a non-root user, for which a capability is never effective on its own: the
/// image gives the binary `cap_sys_nice+p` (permitted, *not* effective - an effective file
/// capability would make the binary fail to start wherever SYS_NICE is not allowed, e.g. a plain
/// `docker run`). Raise it to effective here, for the calling thread, when it is permitted.
fn raise_sys_nice() -> bool {
    let mut header = CapHeader { version: LINUX_CAPABILITY_VERSION_3, pid: 0 };
    let mut data = [CapData::default(); 2];
    unsafe {
        if libc::syscall(libc::SYS_capget, &mut header as *mut CapHeader, data.as_mut_ptr()) != 0 {
            return false;
        }
        if data[0].permitted & (1 << CAP_SYS_NICE) == 0 {
            return false;
        }
        data[0].effective |= 1 << CAP_SYS_NICE;
        libc::syscall(libc::SYS_capset, &mut header as *mut CapHeader, data.as_ptr()) == 0
    }
}

/// Promotes the *calling* thread to SCHED_FIFO at `priority` (1-99; 0 = leave it alone).
pub fn promote_current_thread(label: &str, priority: u8) {
    if priority == 0 {
        return;
    }
    let raised = raise_sys_nice();
    let param = libc::sched_param { sched_priority: priority.min(98) as libc::c_int };
    // pid 0 = the calling thread (Linux applies the policy per thread).
    let rc = unsafe { libc::sched_setscheduler(0, libc::SCHED_FIFO, &param) };
    if rc == 0 {
        tracing::info!(thread = label, priority, "real-time scheduling (SCHED_FIFO) enabled");
    } else {
        let err = std::io::Error::last_os_error();
        tracing::warn!(thread = label, priority, error = %err, cap_sys_nice = raised, "could not enable real-time scheduling, keeping normal priority (needs CAP_SYS_NICE: allowed by the container and set on the binary)");
    }
}
