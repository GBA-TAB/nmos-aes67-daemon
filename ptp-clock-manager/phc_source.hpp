#pragma once
#include <cstdint>
#include <optional>
#include <string>

/*
 * PTP time from a NIC's hardware clock (PHC) that ptp4l disciplines with hardware timestamps,
 * for the RAVENNA driver's external PTP mode (module option ptp_source=1).
 *
 * sample() reads the PHC against CLOCK_MONOTONIC - the driver's own clock (ktime_get) - with the
 * kernel's cross-timestamp ioctl (PTP_SYS_OFFSET_EXTENDED, best of 5 reads). ptp4l_state() asks
 * ptp4l (via pmc on its read-only management socket) whether it is locked and to which grandmaster.
 *
 * The system clock is not touched: the grandmaster's timescale need not be TAI.
 */
struct PhcSample {
    uint64_t ptp_ns;     /* PHC time (PTP timescale) */
    uint64_t mono_ns;    /* CLOCK_MONOTONIC at the same instant */
    uint32_t window_ns;  /* width of the best read's system-time bracket (its uncertainty) */
};

struct Ptp4lState {
    bool     locked{false};
    uint64_t gmid{0};          /* clock identity bytes in wire order */
    int64_t  master_offset{0}; /* ns, ptp4l's last offset to the grandmaster */
};

class PhcSource {
public:
    PhcSource(std::string device, std::string ptp4l_uds);
    ~PhcSource();

    bool open();
    std::optional<PhcSample> sample();
    /* Queries ptp4l; cheap enough for once a second (spawns pmc). */
    Ptp4lState ptp4l_state();

    const std::string& device() const { return device_; }

private:
    std::string device_;
    std::string uds_;
    int fd_{-1};
    bool monotonic_ioctl_{true}; /* the kernel takes clockid = CLOCK_MONOTONIC; else convert from REALTIME */
};
