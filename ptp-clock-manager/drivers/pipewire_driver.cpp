#include "pipewire_driver.hpp"
#include <cstdio>
#include <algorithm>

#ifdef WITH_PIPEWIRE
#include <pipewire/pipewire.h>
#include <pipewire/loop.h>
#endif

PipeWireDriver::PipeWireDriver() = default;
PipeWireDriver::PipeWireDriver(Config cfg) : cfg_(cfg) {}

PipeWireDriver::~PipeWireDriver() {
    stop();
}

bool PipeWireDriver::start() {
#ifndef WITH_PIPEWIRE
    std::fputs("PipeWireDriver: not compiled in (rebuild with -DWITH_PIPEWIRE=ON)\n", stderr);
    return false;
#else
    pw_init(nullptr, nullptr);

    pw_loop_ = pw_main_loop_new(nullptr);
    if (!pw_loop_) return false;

    pw_context_ = pw_context_new(
        pw_main_loop_get_loop(static_cast<struct pw_main_loop*>(pw_loop_)),
        nullptr, 0);
    if (!pw_context_) return false;

    pw_core_ = pw_context_connect(
        static_cast<struct pw_context*>(pw_context_), nullptr, 0);
    if (!pw_core_) return false;

    std::puts("PipeWireDriver: connected to PipeWire");
    return true;
#endif
}

void PipeWireDriver::stop() {
#ifdef WITH_PIPEWIRE
    if (pw_core_)   { pw_core_disconnect(static_cast<struct pw_core*>(pw_core_)); pw_core_ = nullptr; }
    if (pw_context_){ pw_context_destroy(static_cast<struct pw_context*>(pw_context_)); pw_context_ = nullptr; }
    if (pw_loop_)   { pw_main_loop_destroy(static_cast<struct pw_main_loop*>(pw_loop_)); pw_loop_ = nullptr; }
#endif
}

int64_t PipeWireDriver::on_ptp_update(int64_t /*offset_ns*/, int64_t freq_ppb, bool locked) {
#ifndef WITH_PIPEWIRE
    (void)freq_ppb; (void)locked;
    return 0;
#else
    if (!locked || !pw_core_) return 0;

    /*
     * Rate matching via pw_metadata "default.clock.rate-match".
     * Clamp the change to avoid disturbing the PipeWire graph with large steps.
     *
     * A more direct path: if clock.ptp=tai is configured in pipewire.conf,
     * PipeWire reads CLOCK_TAI natively — ClockTaiDriver is then the only
     * driver needed and this on_ptp_update is a no-op.
     *
     * The full implementation (pw_metadata_set) is left as TODO once the
     * PipeWire API for rate-matching is confirmed for the running version.
     */
    double clamped_ppb = std::clamp(static_cast<double>(freq_ppb),
                                    -cfg_.max_rate_change_ppb,
                                     cfg_.max_rate_change_ppb);
    /* TODO: pw_metadata_set(metadata, 0, "default.clock.rate-match", "Fraction",
                             format_fraction(1'000'000'000LL + clamped_ppb, 1'000'000'000LL)); */
    (void)clamped_ppb;
    return 0;
#endif
}
