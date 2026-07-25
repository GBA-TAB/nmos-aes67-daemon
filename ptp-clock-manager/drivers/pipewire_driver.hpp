#pragma once
#include "audio_clock_driver.hpp"

struct PipeWireDriverConfig {
    const char* clock_node_name{nullptr};  /* nullptr = graph master clock */
    double max_rate_change_ppb{50.0};
};

/*
 * PipeWire native AES67 driver.
 *
 * PipeWire ≥ 0.3.51 can reference CLOCK_TAI for AES67 media timestamping
 * via the 'clock.ptp' property in pipewire.conf.  When that is configured,
 * disciplining CLOCK_REALTIME/CLOCK_TAI (ClockTaiDriver) is sufficient — no
 * additional PipeWire interaction is needed for basic sync.
 *
 * This driver provides finer integration: it connects to PipeWire via the
 * native protocol and trims the graph clock rate directly using freq_ppb.
 * Useful when sub-sample accuracy is required without clock_settime steps.
 *
 * Build guard: only compiled when -DWITH_PIPEWIRE=ON.
 */
class PipeWireDriver : public AudioClockDriver {
public:
    using Config = PipeWireDriverConfig;

    PipeWireDriver();
    explicit PipeWireDriver(Config cfg);
    ~PipeWireDriver() override;

    bool start()  override;
    void stop()   override;
    int64_t on_ptp_update(int64_t offset_ns, int64_t freq_ppb, bool locked) override;
    std::string name() const override { return "pipewire_aes67"; }

private:
    Config cfg_;
    void* pw_loop_{nullptr};
    void* pw_context_{nullptr};
    void* pw_core_{nullptr};
};
