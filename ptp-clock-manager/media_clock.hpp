#pragma once
#include <cstdint>
#include <deque>
#include <string>
#include <utility>

/*
 * Publisher of MXL's media clock record (mxl/mediaclock.h in GBA-TAB/mxl, MXL_MEDIA_CLOCK): MXL
 * time from PTP instead of the NTP-steered system clock. The layout below must match that header
 * (magic + version are checked by readers).
 */
#define MXL_MEDIA_CLOCK_MAGIC   0x434D584DU
#define MXL_MEDIA_CLOCK_VERSION 1U
#define MXL_MEDIA_CLOCK_LOCKED  0x1U

extern "C" {
struct mxlMediaClockRecord {
    uint32_t magic;
    uint32_t version;
    uint32_t seq;          /* seqlock: odd while writing */
    uint32_t flags;
    int64_t  refRawNs;     /* CLOCK_MONOTONIC_RAW at the reference point */
    int64_t  refMediaNs;   /* media (PTP) time at the reference point */
    double   rate;         /* media ns per CLOCK_MONOTONIC_RAW ns */
    int64_t  updatedRawNs;
    uint8_t  gmIdentity[8];
    uint8_t  reserved[16];
};
}
static_assert(sizeof(mxlMediaClockRecord) == 72, "must match mxl/mediaclock.h");

/*
 * Turns (CLOCK_MONOTONIC_RAW, PHC) samples into the record's linear mapping, keeping MXL time
 * continuous and monotonic: each update re-anchors at the current mapped time (no jump) with the
 * measured rate plus a correction that removes the phase error over `phase_time_constant_s`.
 * A phase error beyond `step_threshold_ns` (ptp4l stepped the PHC, or the first samples) re-anchors
 * on the sample itself - a visible jump, logged.
 */
class MediaClockServo {
public:
    struct Mapping { int64_t ref_raw; int64_t ref_media; double rate; };

    /* Feeds one sample; returns true once there is a mapping to publish. */
    bool feed(int64_t raw_ns, int64_t phc_ns);
    const Mapping& mapping() const { return m_; }
    int64_t last_phase_error_ns() const { return last_err_; }
    unsigned steps() const { return steps_; }

    double rate_window_s{8.0};
    double phase_time_constant_s{2.0};
    int64_t step_threshold_ns{1'000'000};

private:
    int64_t at(int64_t raw) const;
    std::deque<std::pair<int64_t, int64_t>> window_;
    Mapping m_{0, 0, 1.0};
    bool have_{false};
    int64_t last_err_{0};
    unsigned steps_{0};
};

class MediaClockPublisher {
public:
    explicit MediaClockPublisher(std::string path) : path_(std::move(path)) {}
    ~MediaClockPublisher();
    bool open();
    void publish(const MediaClockServo::Mapping& m, int64_t updated_raw, bool locked, uint64_t gmid);
    const std::string& path() const { return path_; }

private:
    std::string path_;
    mxlMediaClockRecord* rec_{nullptr};
};
