#pragma once
#include "audio_clock_driver.hpp"
#include <atomic>
#include <string>
#include <thread>

/*
 * ALSA SRC driver — bridges a RAVENNA ALSA loopback capture device (correctly
 * PTP-timed) to any ALSA playback device that cannot be disciplined directly
 * (USB audio, I2S, etc.) via a continuously rate-adjusted soxr resampler.
 *
 * The sink's playback crystal is an independent oscillator from the system
 * clock, so it has no fixed relationship to the freq_ppb correction
 * ClockTaiDriver applies to CLOCK_REALTIME — that number describes the
 * system oscillator's error against PTP, not the sink codec's. Instead, the
 * resample ratio is servoed directly off the sink's own playback buffer
 * occupancy (via snd_pcm_delay): if the buffer trends toward empty, the sink
 * is draining faster than nominal and more output samples are fed per input
 * sample (and vice versa). This runs entirely inside the driver's own
 * capture/playback thread, independent of the ~250 ms main poll tick.
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

        /* Sink buffer-occupancy servo (PI on snd_pcm_delay() vs. target level) */
        double      drift_kp{0.05};       /* ppb correction per frame of level error */
        double      drift_ki{0.0008};     /* ppb correction per frame-tick of accumulated error */
        double      max_drift_ppb{2000.0};
    };

    explicit AlsaSrcDriver(Config cfg);
    ~AlsaSrcDriver() override;

    bool    start()  override;
    void    stop()   override;
    int64_t on_ptp_update(int64_t offset_ns, int64_t freq_ppb, bool locked) override;
    std::string name() const override { return "alsa_src:" + cfg_.sink_device; }

private:
    Config              cfg_;
    std::atomic<double> ratio_{1.0};    /* consumed by run() */
    std::atomic<bool>   locked_{false}; /* set from on_ptp_update, read by run() */
    std::atomic<bool>   running_{false};
    std::thread         thread_;

    void* src_pcm_{nullptr};           /* snd_pcm_t* — source capture */
    void* snk_pcm_{nullptr};           /* snd_pcm_t* — sink playback */
    void* soxr_{nullptr};              /* soxr_t */

    void run();
};
