#pragma once
#include <cstdint>

struct ClockDisciplineConfig {
    double kp{0.7};
    double ki{0.3 / 30.0};        /* tau_i ≈ 30 s */
    double max_freq_ppb{500.0};
    double step_threshold_ns{1'000.0};
};

/*
 * PI servo for disciplining CLOCK_REALTIME from a PTP offset measurement.
 * CLOCK_TAI tracks CLOCK_REALTIME automatically (TAI = REALTIME + tai_offset).
 */
class ClockDiscipline {
public:
    using Config = ClockDisciplineConfig;

    ClockDiscipline();
    explicit ClockDiscipline(Config cfg);

    /* Feed a new offset sample (ns); returns freq correction applied (ppb).
     * Calls clock_adjtime(CLOCK_REALTIME, ...) internally. */
    double feed(int64_t offset_ns);

    /* Step-set CLOCK_REALTIME to remove a large initial offset. Resets integrator. */
    void step(int64_t offset_ns);

    void reset();

private:
    Config   cfg_;
    double   integral_{0.0};
    bool     first_{true};
    int64_t  tai_offset_s_{37};

    void set_tai_offset();
    void apply_freq(double freq_ppb);
};
