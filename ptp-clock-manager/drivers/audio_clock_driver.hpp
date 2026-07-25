#pragma once
#include <cstdint>
#include <string>

/*
 * Abstract sink interface for distributing PTP-derived clock state to any
 * audio subsystem.  Each driver is called once per poll tick when locked.
 *
 * Implementations must be thread-safe (called from the main loop thread).
 */
class AudioClockDriver {
public:
    virtual ~AudioClockDriver() = default;

    virtual bool start() { return true; }
    virtual void stop()  {}

    /*
     * Called on every PTP poll tick regardless of lock state.
     *  offset_ns  — measured offset from PTP master (negative = behind)
     *  freq_ppb   — frequency correction currently applied to CLOCK_REALTIME
     *  locked     — true when RAVENNA reports PTPLS_LOCKED
     */
    /* Returns the freq_ppb applied to the kernel clock, or 0 if not applicable.
     * Callers propagate the first non-zero return to subsequent drivers. */
    virtual int64_t on_ptp_update(int64_t offset_ns, int64_t freq_ppb, bool locked) = 0;

    virtual std::string name() const = 0;
};
