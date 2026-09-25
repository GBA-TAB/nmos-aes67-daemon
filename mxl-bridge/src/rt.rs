//! Real-time scheduling for the audio period threads.
//!
//! On a host partitioned for real time (mxl-orchestrator `exclusive` CPU mode), the app owns its
//! cores - but a normal (SCHED_OTHER) thread can still be preempted there by the app's own other
//! threads (HTTP, registration, logging). SCHED_FIFO makes the period thread win every time.
//! Needs CAP_SYS_NICE (the orchestrator grants it to real-time kinds). Without it this logs a
//! warning and the thread simply keeps normal priority - never fatal. The kernel's RT throttling
//! (sched_rt_runtime_us, 95 % by default) still bounds a runaway thread.

/// Promotes the *calling* thread to SCHED_FIFO at `priority` (1-99; 0 = leave it alone).
pub fn promote_current_thread(label: &str, priority: u8) {
    if priority == 0 {
        return;
    }
    let param = libc::sched_param { sched_priority: priority.min(98) as libc::c_int };
    // pid 0 = the calling thread (Linux applies the policy per thread).
    let rc = unsafe { libc::sched_setscheduler(0, libc::SCHED_FIFO, &param) };
    if rc == 0 {
        tracing::info!(thread = label, priority, "real-time scheduling (SCHED_FIFO) enabled");
    } else {
        let err = std::io::Error::last_os_error();
        tracing::warn!(thread = label, priority, error = %err, "could not enable real-time scheduling, keeping normal priority (the container needs CAP_SYS_NICE)");
    }
}
