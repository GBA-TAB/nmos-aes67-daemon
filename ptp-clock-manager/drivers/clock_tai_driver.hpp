#pragma once
#include "audio_clock_driver.hpp"
#include "../clock_discipline.hpp"

/*
 * Disciplines CLOCK_REALTIME (and therefore CLOCK_TAI) using the PI servo.
 * This is the primary driver; once active, any consumer of CLOCK_TAI (PipeWire
 * native AES67, ptp4l media-clock consumers, etc.) gets a disciplined clock
 * without any additional integration.
 *
 * Requires CAP_SYS_TIME.
 */
class ClockTaiDriver : public AudioClockDriver {
public:
    ClockTaiDriver();
    explicit ClockTaiDriver(ClockDiscipline::Config servo_cfg);

    int64_t on_ptp_update(int64_t offset_ns, int64_t freq_ppb, bool locked) override;
    std::string name() const override { return "clock_tai"; }

private:
    ClockDiscipline servo_;
    bool            was_locked_{false};
};
