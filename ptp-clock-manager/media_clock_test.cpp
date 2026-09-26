// Servo test: a simulated PHC at +27 ppm vs the raw clock, with read noise and one ptp4l step.
// MXL time must never go backwards, must follow the PHC to well under a microsecond once settled,
// and a step must be followed (one re-anchor), not smeared.
#include "media_clock.hpp"
#include <cassert>
#include <cstdio>
#include <cstdlib>
#include <random>

int main() {
    MediaClockServo s;
    std::mt19937 rng(1);
    std::normal_distribution<double> noise(0.0, 30.0);  // 30 ns rms read noise
    const double ppm = 27.0;
    int64_t last_mxl = 0;
    double worst_settled = 0;
    int64_t step = 0;
    for (int i = 0; i < 8 * 120; ++i) {  // 2 minutes at 8 Hz
        const int64_t raw = 1'000'000'000LL + i * 125'000'000LL;
        if (i == 8 * 60) step = 5'000'000;  // ptp4l steps the PHC by 5 ms at t = 60 s
        const double phc_true = 1.79e18 + (raw - 1e9) * (1 + ppm * 1e-6) + step;
        s.feed(raw, static_cast<int64_t>(phc_true + noise(rng)));
        // MXL reads between updates: monotonic across updates
        for (int k = 0; k < 125; k += 25) {
            const auto& m = s.mapping();
            const int64_t r = raw + k * 1'000'000LL;
            const int64_t mxl = m.ref_media + std::llround((r - m.ref_raw) * m.rate);
            assert(mxl > last_mxl && "MXL time went backwards");
            last_mxl = mxl;
        }
        const double err = std::abs(static_cast<double>(s.last_phase_error_ns()));
        if ((i > 8 * 20 && i < 8 * 60) || i > 8 * 80) worst_settled = std::max(worst_settled, err);
    }
    std::printf("steps %u, worst settled phase error %.0f ns, rate %+.3f ppm\n", s.steps(), worst_settled, (s.mapping().rate - 1) * 1e6);
    assert(s.steps() == 1);
    assert(worst_settled < 500);
    assert(std::abs((s.mapping().rate - 1) * 1e6 - ppm) < 0.5);
    std::puts("PASS");
    return 0;
}
