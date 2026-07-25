#include "alsa_src_driver.hpp"
#include <cstdio>
#include <cstring>

#ifdef WITH_ALSA_SRC
#include <alsa/asoundlib.h>
#include <soxr.h>
#include <vector>

/* Cast helpers — keep ALSA/soxr types out of the header */
static snd_pcm_t* pcm(void* p)  { return static_cast<snd_pcm_t*>(p); }
static soxr_t     sxr(void* p)  { return static_cast<soxr_t>(p); }

/* Open and configure an ALSA PCM handle */
static bool open_pcm(const char* device, snd_pcm_stream_t stream,
                     unsigned rate, unsigned channels, unsigned period_frames,
                     snd_pcm_t** out) {
    int err = snd_pcm_open(out, device, stream, 0);
    if (err < 0) {
        std::fprintf(stderr, "AlsaSrcDriver: open '%s': %s\n", device, snd_strerror(err));
        return false;
    }

    snd_pcm_hw_params_t* hp;
    snd_pcm_hw_params_alloca(&hp);
    snd_pcm_hw_params_any(*out, hp);
    snd_pcm_hw_params_set_access(*out, hp, SND_PCM_ACCESS_RW_INTERLEAVED);
    snd_pcm_hw_params_set_format(*out, hp, SND_PCM_FORMAT_S32_LE);
    snd_pcm_hw_params_set_channels(*out, hp, channels);

    unsigned r = rate; int dir = 0;
    snd_pcm_hw_params_set_rate_near(*out, hp, &r, &dir);

    snd_pcm_uframes_t period = period_frames;
    snd_pcm_uframes_t buf    = period_frames * 4;
    snd_pcm_hw_params_set_period_size_near(*out, hp, &period, &dir);
    snd_pcm_hw_params_set_buffer_size_near(*out, hp, &buf);

    err = snd_pcm_hw_params(*out, hp);
    if (err < 0) {
        std::fprintf(stderr, "AlsaSrcDriver: hw_params '%s': %s\n", device, snd_strerror(err));
        snd_pcm_close(*out);
        *out = nullptr;
        return false;
    }
    if (r != rate)
        std::fprintf(stderr, "AlsaSrcDriver: '%s' negotiated %u Hz (wanted %u)\n",
                     device, r, rate);
    snd_pcm_prepare(*out);
    return true;
}

#endif /* WITH_ALSA_SRC */

/* ── AlsaSrcDriver ─────────────────────────────────────────────────── */

AlsaSrcDriver::AlsaSrcDriver(Config cfg) : cfg_(std::move(cfg)) {}

AlsaSrcDriver::~AlsaSrcDriver() { stop(); }

bool AlsaSrcDriver::start() {
    if (cfg_.sink_device.empty()) {
        std::fputs("AlsaSrcDriver: no sink device configured\n", stderr);
        return false;
    }
#ifndef WITH_ALSA_SRC
    std::fputs("AlsaSrcDriver: not compiled in (rebuild with -DWITH_ALSA_SRC=ON)\n", stderr);
    return false;
#else
    {
        snd_pcm_t* h = nullptr;
        if (!open_pcm(cfg_.source_device.c_str(), SND_PCM_STREAM_CAPTURE,
                      cfg_.sample_rate, cfg_.channels, cfg_.period_frames, &h))
            return false;
        src_pcm_ = h;
    }
    {
        snd_pcm_t* h = nullptr;
        if (!open_pcm(cfg_.sink_device.c_str(), SND_PCM_STREAM_PLAYBACK,
                      cfg_.sample_rate, cfg_.channels, cfg_.period_frames, &h)) {
            snd_pcm_close(pcm(src_pcm_)); src_pcm_ = nullptr;
            return false;
        }
        snk_pcm_ = h;
    }

    soxr_error_t       serr;
    soxr_io_spec_t     io  = soxr_io_spec(SOXR_INT32_I, SOXR_INT32_I);
    soxr_quality_spec_t q  = soxr_quality_spec(SOXR_MQ, 0);
    soxr_runtime_spec_t rt = soxr_runtime_spec(1);
    soxr_t sx = soxr_create(cfg_.sample_rate, cfg_.sample_rate,
                            cfg_.channels, &serr, &io, &q, &rt);
    if (!sx) {
        std::fprintf(stderr, "AlsaSrcDriver: soxr_create: %s\n", serr ? serr : "unknown");
        snd_pcm_close(pcm(src_pcm_)); src_pcm_ = nullptr;
        snd_pcm_close(pcm(snk_pcm_)); snk_pcm_ = nullptr;
        return false;
    }
    soxr_ = sx;

    running_ = true;
    thread_  = std::thread(&AlsaSrcDriver::run, this);
    std::printf("AlsaSrcDriver: %s → %s  %u ch  %u Hz  period=%u\n",
                cfg_.source_device.c_str(), cfg_.sink_device.c_str(),
                cfg_.channels, cfg_.sample_rate, cfg_.period_frames);
    return true;
#endif
}

void AlsaSrcDriver::stop() {
    running_ = false;
    if (thread_.joinable()) thread_.join();
#ifdef WITH_ALSA_SRC
    if (soxr_)    { soxr_delete(sxr(soxr_));      soxr_    = nullptr; }
    if (snk_pcm_) { snd_pcm_close(pcm(snk_pcm_)); snk_pcm_ = nullptr; }
    if (src_pcm_) { snd_pcm_close(pcm(src_pcm_)); src_pcm_ = nullptr; }
#endif
}

int64_t AlsaSrcDriver::on_ptp_update(int64_t /*offset_ns*/, int64_t freq_ppb, bool locked) {
    ratio_.store(locked ? 1.0 + static_cast<double>(freq_ppb) * 1e-9 : 1.0,
                 std::memory_order_relaxed);
    return 0;
}

#ifdef WITH_ALSA_SRC
void AlsaSrcDriver::run() {
    const unsigned CH      = cfg_.channels;
    const unsigned FRAMES  = cfg_.period_frames;
    /* Headroom: max realistic AES67 correction << 1000 ppb → output barely > FRAMES */
    const size_t   OUT_MAX = FRAMES + 64;

    std::vector<int32_t> in_buf(FRAMES  * CH);
    std::vector<int32_t> out_buf(OUT_MAX * CH);

    while (running_) {
        /* Capture one period */
        snd_pcm_sframes_t n = snd_pcm_readi(pcm(src_pcm_), in_buf.data(), FRAMES);
        if (n < 0) {
            int err = snd_pcm_recover(pcm(src_pcm_), static_cast<int>(n), /*silent=*/0);
            if (err < 0)
                std::fprintf(stderr, "AlsaSrcDriver: src recover: %s\n", snd_strerror(err));
            continue;
        }

        /* Update ratio; slew over one period for click-free transitions */
        double ratio = ratio_.load(std::memory_order_relaxed);
        soxr_set_io_ratio(sxr(soxr_), ratio, FRAMES);

        /* Resample */
        const void* in_p  = in_buf.data();
        void*       out_p = out_buf.data();
        size_t idone = 0, odone = 0;
        soxr_error_t err = soxr_process(sxr(soxr_),
                                        &in_p, static_cast<size_t>(n),  &idone,
                                        &out_p, OUT_MAX,                &odone);
        if (err) {
            std::fprintf(stderr, "AlsaSrcDriver: soxr_process: %s\n", err);
            continue;
        }

        /* Play — loop handles partial writes (rare but possible) */
        const int32_t*    ptr       = out_buf.data();
        snd_pcm_sframes_t remaining = static_cast<snd_pcm_sframes_t>(odone);
        while (remaining > 0 && running_) {
            snd_pcm_sframes_t w = snd_pcm_writei(pcm(snk_pcm_), ptr, remaining);
            if (w < 0) {
                int re = snd_pcm_recover(pcm(snk_pcm_), static_cast<int>(w), 0);
                if (re < 0)
                    std::fprintf(stderr, "AlsaSrcDriver: snk recover: %s\n", snd_strerror(re));
                break;
            }
            ptr       += w * static_cast<snd_pcm_sframes_t>(CH);
            remaining -= w;
        }
    }
}
#else
void AlsaSrcDriver::run() {}
#endif
