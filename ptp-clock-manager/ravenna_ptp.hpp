#pragma once
#include <cstdint>
#include <optional>

/* Subset of TPTPStatus fields we care about for clock discipline */
struct RavennaPtpStatus {
    int  lock_state;     /* 0=UNLOCKED 1=LOCKING 2=LOCKED */
    uint64_t gmid[2];    /* grandmaster ID as two uint64 (see TPTPStatus.ui64GMID) */
    int32_t  network_jitter;
    int32_t  clock_jitter;  /* peak jitter magnitude over the poll interval (ns); NOT a signed offset */
    int64_t  ptp_offset_ns;  /* local RTX clock minus PTP master time, ns (negative = local behind master) */
};

/*
 * Lightweight raw-netlink client for the RAVENNA ALSA LKM.
 * Uses a single U2K socket (protocol 31) for synchronous commands only.
 * Does NOT register on K2U (29) — we poll instead of using events.
 *
 * Requires CAP_NET_ADMIN (PF_NETLINK/SOCK_RAW).
 */
class RavennaPtp {
public:
    RavennaPtp();
    ~RavennaPtp();

    RavennaPtp(const RavennaPtp&) = delete;
    RavennaPtp& operator=(const RavennaPtp&) = delete;

    /* Open U2K socket; returns false on failure */
    bool open();
    void close();

    /* Query TPTPStatus from kernel module; returns nullopt on timeout/error */
    std::optional<RavennaPtpStatus> get_status();

    /* External PTP mode (driver module option ptp_source=1): one (PTP time, CLOCK_MONOTONIC) pair.
     * Returns the driver's error code (0 = taken, -401 = the driver runs its own PTP), or -1 on a
     * transport error. */
    int send_external_sample(uint64_t ptp_ns, uint64_t mono_ns, uint64_t gmid, bool locked);

private:
    int fd_{-1};
};
