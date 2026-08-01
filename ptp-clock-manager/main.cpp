#include "ravenna_ptp.hpp"
#include "clock_discipline.hpp"
#include "shm_clock.h"
#include "drivers/audio_clock_driver.hpp"
#include "drivers/clock_tai_driver.hpp"
#include "drivers/alsa_src_driver.hpp"
#include "drivers/pipewire_driver.hpp"

#include <atomic>
#include <csignal>
#include <cstdio>
#include <cstring>
#include <memory>
#include <string>
#include <thread>
#include <vector>
#include <chrono>

#include <fcntl.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <time.h>
#include <unistd.h>

using namespace std::chrono_literals;

static std::atomic_bool g_running{true};

static void on_signal(int) { g_running = false; }

/* ---------- SHM helpers ---------- */

static PtpClockShm* shm_create() {
    int fd = shm_open(PTP_CLOCK_SHM_NAME, O_CREAT | O_RDWR, 0644);
    if (fd < 0) { std::perror("shm_open"); return nullptr; }
    if (ftruncate(fd, sizeof(PtpClockShm)) < 0) {
        std::perror("ftruncate"); ::close(fd); return nullptr;
    }
    auto* p = static_cast<PtpClockShm*>(
        mmap(nullptr, sizeof(PtpClockShm), PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0));
    ::close(fd);
    if (p == MAP_FAILED) { std::perror("mmap"); return nullptr; }
    memset(p, 0, sizeof(PtpClockShm));
    p->magic   = PTP_CLOCK_SHM_MAGIC;
    p->version = PTP_CLOCK_SHM_VERSION;
    return p;
}

static void shm_publish(PtpClockShm* shm, const RavennaPtpStatus& s,
                        int64_t freq_ppb, double /*unused*/) {
    /* Seqlock write: odd count signals write-in-progress to readers */
    __atomic_add_fetch(&shm->update_count, 1, __ATOMIC_RELEASE);

    shm->lock_state     = static_cast<uint8_t>(s.lock_state);
    memcpy(shm->gmid,    &s.gmid, 8);
    shm->offset_ns      = s.ptp_offset_ns;
    shm->freq_ppb       = freq_ppb;
    shm->network_jitter = s.network_jitter;
    shm->clock_jitter   = s.clock_jitter;

    struct timespec ts{};
    clock_gettime(CLOCK_TAI,       &ts);
    shm->tai_ns         = static_cast<int64_t>(ts.tv_sec) * 1'000'000'000LL + ts.tv_nsec;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    shm->monotonic_ns   = static_cast<uint64_t>(ts.tv_sec) * 1'000'000'000ULL + ts.tv_nsec;

    __atomic_add_fetch(&shm->update_count, 1, __ATOMIC_RELEASE);
}

/* ---------- Main ---------- */

static void usage(const char* prog) {
    std::printf(
        "Usage: %s [options]\n"
        "  --poll-ms N          poll interval ms (default: 250)\n"
        "  --alsa-sink DEVICE   add ALSA SRC driver for device (e.g. hw:2)\n"
        "  --pipewire           add PipeWire AES67 driver\n"
        "  --no-tai             disable CLOCK_TAI discipline\n"
        "  --help\n",
        prog);
}

int main(int argc, char** argv) {
    unsigned poll_ms    = 250;
    bool     do_tai     = true;
    bool     do_pipewire = false;
    std::vector<std::string> alsa_sinks;

    for (int i = 1; i < argc; ++i) {
        std::string a = argv[i];
        if (a == "--help") { usage(argv[0]); return 0; }
        else if (a == "--no-tai")   do_tai      = false;
        else if (a == "--pipewire") do_pipewire = true;
        else if (a == "--alsa-sink" && i + 1 < argc) alsa_sinks.push_back(argv[++i]);
        else if (a == "--poll-ms"   && i + 1 < argc) poll_ms = static_cast<unsigned>(std::stoul(argv[++i]));
        else { std::fprintf(stderr, "Unknown option: %s\n", argv[i]); return 1; }
    }

    signal(SIGTERM, on_signal);
    signal(SIGINT,  on_signal);

    /* Build driver list */
    std::vector<std::unique_ptr<AudioClockDriver>> drivers;

    if (do_tai)
        drivers.push_back(std::make_unique<ClockTaiDriver>());

    for (const auto& dev : alsa_sinks) {
        AlsaSrcDriver::Config c;
        c.sink_device = dev;
        drivers.push_back(std::make_unique<AlsaSrcDriver>(c));
    }

    if (do_pipewire)
        drivers.push_back(std::make_unique<PipeWireDriver>());

    /* Start drivers */
    for (auto& d : drivers) {
        if (!d->start())
            std::fprintf(stderr, "ptp-clock-manager: driver '%s' failed to start\n",
                         d->name().c_str());
    }

    /* Open RAVENNA netlink */
    RavennaPtp ptp;
    if (!ptp.open()) {
        std::fputs("ptp-clock-manager: failed to open RAVENNA netlink socket\n", stderr);
        return 1;
    }

    /* Create SHM */
    PtpClockShm* shm = shm_create();
    if (!shm) return 1;

    std::printf("ptp-clock-manager: running (poll=%u ms, drivers=%zu)\n",
                poll_ms, drivers.size());

    const auto interval = std::chrono::milliseconds(poll_ms);
    int64_t applied_freq_ppb = 0;

    while (g_running) {
        auto tick_start = std::chrono::steady_clock::now();

        auto status = ptp.get_status();
        if (status) {
            bool locked = (status->lock_state == 2 /* PTPLS_LOCKED */);
            if (!locked) applied_freq_ppb = 0;

            for (auto& d : drivers) {
                int64_t r = d->on_ptp_update(status->ptp_offset_ns, applied_freq_ppb, locked);
                if (r != 0) applied_freq_ppb = r;
            }

            shm_publish(shm, *status, applied_freq_ppb, 0.0);
        }

        auto elapsed = std::chrono::steady_clock::now() - tick_start;
        if (elapsed < interval)
            std::this_thread::sleep_for(interval - elapsed);
    }

    std::puts("ptp-clock-manager: shutting down");
    for (auto& d : drivers) d->stop();
    ptp.close();
    shm_unlink(PTP_CLOCK_SHM_NAME);
    munmap(shm, sizeof(PtpClockShm));
    return 0;
}
