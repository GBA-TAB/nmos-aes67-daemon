//
//  ptp_clock_shm.hpp
//
//  Read-only access to ptp-clock-manager's PtpClockShm segment (see
//  ../ptp-clock-manager/shm_clock.h). ptp-clock-manager is an independent,
//  optional process; these helpers must degrade gracefully (return false)
//  when it isn't running.
//

#pragma once

#include "../ptp-clock-manager/shm_clock.h"

// Best-effort read of ptp-clock-manager's PtpClockShm segment. Returns false
// if ptp-clock-manager isn't running (segment doesn't exist), the
// magic/version don't match, or a consistent snapshot couldn't be taken
// within a few retries against the writer's update_count generation counter.
bool read_ptp_clock_shm(PtpClockShm& out);

// Same as read_ptp_clock_shm, but also rejects a snapshot whose
// monotonic_ns timestamp is older than max_age_ns — guards against a
// crashed ptp-clock-manager leaving a stale-but-well-formed segment behind.
bool read_ptp_clock_shm_fresh(PtpClockShm& out, int64_t max_age_ns = 3'000'000'000LL);
