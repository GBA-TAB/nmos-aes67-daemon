#pragma once
#include "audio_clock_driver.hpp"
#include <atomic>
#include <string>
#include <thread>

/*
 * ALSA SRC driver — bridges a RAVENNA ALSA loopback capture device to any
 * ALSA playback device that cannot be disciplined directly (USB audio, I2S, etc.)
 * via a continuously rate-adjusted soxr resampler.
 *
 * The SRC ratio is trimmed each poll tick from the freq_ppb produced by
 * ClockTaiDriver, keeping the sink in sync with the PTP master.
 *
 * Requires: libasound2, libsoxr.  Compile with -DWITH_ALSA_SRC.
 */
class AlsaSrcDriver : public AudioClockDriver {
public:
    struct Config {
        std::string source_device{"hw:Loopback,0,0"};
        std::string sink_device;          /* e.g. "hw:2" — must not be empty */
        unsigned    sample_rate{48000};
        unsigned    channels{2};
        unsigned    period_frames{256};   /* ALSA period and soxr block size */
    };

    explicit AlsaSrcDriver(Config cfg);
    ~AlsaSrcDriver() override;

    bool    start()  override;
    void    stop()   override;
    int64_t on_ptp_update(int64_t offset_ns, int64_t freq_ppb, bool locked) override;
    std::string name() const override { return "alsa_src:" + cfg_.sink_device; }

private:
    Config              cfg_;
    std::atomic<double> ratio_{1.0};   /* updated from main loop, consumed by run() */
    std::atomic<bool>   running_{false};
    std::thread         thread_;

    void* src_pcm_{nullptr};           /* snd_pcm_t* — source capture */
    void* snk_pcm_{nullptr};           /* snd_pcm_t* — sink playback */
    void* soxr_{nullptr};              /* soxr_t */

    void run();
};
