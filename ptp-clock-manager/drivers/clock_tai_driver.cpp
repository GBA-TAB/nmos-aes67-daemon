#include "clock_tai_driver.hpp"
#include <cstdio>

ClockTaiDriver::ClockTaiDriver() = default;
ClockTaiDriver::ClockTaiDriver(ClockDiscipline::Config cfg) : servo_(cfg) {}

int64_t ClockTaiDriver::on_ptp_update(int64_t offset_ns, int64_t /*freq_ppb*/, bool locked) {
    if (!locked) {
        if (was_locked_) {
            servo_.reset();
            std::puts("ClockTaiDriver: PTP lock lost — servo reset");
        }
        was_locked_ = false;
        return 0;
    }
    if (!was_locked_)
        std::puts("ClockTaiDriver: PTP locked — disciplining CLOCK_REALTIME");

    was_locked_ = true;
    return static_cast<int64_t>(servo_.feed(offset_ns));
}
