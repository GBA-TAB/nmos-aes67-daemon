/// Nanoseconds since the TAI epoch (1970-01-01T00:00:00 TAI), which is the same epoch MXL's
/// ring-buffer indexing uses (SMPTE ST 2059-1). Disciplined system-wide by
/// ptp-clock-manager/drivers/clock_tai_driver.cpp — no PTP client needed here, we just read the
/// OS clock.
pub fn tai_now_ns() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: `ts` is a valid, correctly-sized out-param for clock_gettime.
    let rc = unsafe { libc::clock_gettime(libc::CLOCK_TAI, &mut ts) };
    if rc != 0 {
        panic!(
            "clock_gettime(CLOCK_TAI) failed: {}",
            std::io::Error::last_os_error()
        );
    }
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}
