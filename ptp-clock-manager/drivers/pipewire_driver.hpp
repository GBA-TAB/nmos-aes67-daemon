#pragma once
#include "audio_clock_driver.hpp"
#include <atomic>
#include <string>
#include <thread>

struct PipeWireDriverConfig {
    /* Only treat a driver node as TAI-synced if its node.name matches this
     * exactly. nullptr = accept any node whose clock.id property is "tai". */
    const char* driver_node_name{nullptr};
};

/*
 * PipeWire verification driver.
 *
 * PipeWire has no public API for trimming the graph clock by an external ppb
 * correction — there is no "rate-match" metadata key (checked against the
 * PipeWire 1.4.8 source: the only externally-settable clock key is
 * clock.force-rate, which forces an integer nominal sample rate, not a
 * continuous frequency trim). Sub-ppm sync is instead achieved by pointing a
 * `support.node.driver` object at CLOCK_TAI (see pipewire-aes67.conf,
 * `clock.id = tai`) and disciplining CLOCK_TAI directly — which is exactly
 * what ClockTaiDriver already does. No further code-side action is needed
 * for PipeWire to pick up the disciplined clock.
 *
 * This driver connects to PipeWire and watches the registry for a node
 * advertising clock.id=tai (i.e. a PTP-clocked graph driver), so that a
 * pipewire.conf misconfiguration (graph still running off its own
 * free-running clock) shows up in ptp-clock-manager's own logs instead of
 * failing silently.
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

    /* Invoked from the registry callback when a matching TAI-clocked driver
     * node appears. Not for external use. */
    void note_tai_driver_node(const std::string& node_name);

private:
    Config cfg_;
    void* pw_loop_{nullptr};
    void* pw_context_{nullptr};
    void* pw_core_{nullptr};
    void* pw_registry_{nullptr};
    void* registry_ctx_{nullptr};
    std::thread loop_thread_;

    std::atomic<bool> tai_driver_found_{false};
    bool was_locked_{false};
};
