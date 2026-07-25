/* ptp-clock-manager shared memory layout — C-compatible, consumed by any process */
#pragma once
#include <stdint.h>

#define PTP_CLOCK_SHM_NAME    "/ptp_clock_mgr"
#define PTP_CLOCK_SHM_VERSION 1
#define PTP_CLOCK_SHM_MAGIC   0x50544343u  /* 'PTCC' */

typedef enum {
    PTP_LOCK_UNLOCKED = 0,
    PTP_LOCK_LOCKING  = 1,
    PTP_LOCK_LOCKED   = 2
} PtpLockState;

/*
 * Written by ptp-clock-manager, read by consumers (PipeWire, ALSA plugins, etc.).
 * update_count is a generation counter: readers should snapshot it before and after
 * copying the struct; if it changed, retry.  Odd count means a write is in progress.
 */
typedef struct {
    uint32_t   magic;           /* PTP_CLOCK_SHM_MAGIC                          */
    uint32_t   version;         /* PTP_CLOCK_SHM_VERSION                        */
    uint32_t   update_count;    /* incremented on each write; odd = write active */
    uint8_t    lock_state;      /* PtpLockState                                 */
    uint8_t    _pad[3];
    uint8_t    gmid[8];         /* grandmaster clock identity (EUI-64)           */
    int64_t    offset_ns;       /* measured offset from PTP master (ns)          */
    int64_t    freq_ppb;        /* current frequency correction applied (ppb)    */
    int64_t    tai_ns;          /* CLOCK_TAI snapshot at last update             */
    uint64_t   monotonic_ns;    /* CLOCK_MONOTONIC at last update                */
    int32_t    network_jitter;  /* RAVENNA TPTPStatus.i32NetworkJitter           */
    int32_t    clock_jitter;    /* RAVENNA TPTPStatus.i32ClockJitter             */
    uint32_t   _pad2;
} PtpClockShm;
