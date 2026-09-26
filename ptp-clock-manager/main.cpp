#include "ravenna_ptp.hpp"
#include "phc_source.hpp"
#include "media_clock.hpp"
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
#include <sched.h>
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
        "  --phc DEVICE         external PTP mode: feed the RAVENNA driver (module option\n"
        "                       ptp_source=1) from this PHC, disciplined by ptp4l (e.g. /dev/ptp0);\n"
        "                       poll interval defaults to 125 ms\n"
        "  --ptp4l-uds PATH     ptp4l's read-only management socket (default /var/run/ptp4lro)\n"
        "  --ptp-domain N       ptp4l's PTP domain, for its management queries (default 0)\n"
        "  --media-clock PATH   with --phc: publish MXL's media clock record there (MXL_MEDIA_CLOCK),\n"
        "                       e.g. <MXL domain>/.media-clock. --phc turns the CLOCK_TAI discipline\n"
        "                       off (the system clock stays on NTP); --tai turns it back on\n"
        "  --help\n",
        prog);
}

int main(int argc, char** argv) {
    unsigned poll_ms    = 250;
    bool     do_tai     = true;
    bool     do_pipewire = false;
    std::vector<std::string> alsa_sinks;
    std::string phc_device;
    std::string ptp4l_uds = "/var/run/ptp4lro";  /* linuxptp's read-only management socket */
    int ptp_domain = 0;
    bool poll_given = false;
    std::string media_clock_path;
    bool tai_given = false;

    for (int i = 1; i < argc; ++i) {
        std::string a = argv[i];
        if (a == "--help") { usage(argv[0]); return 0; }
        else if (a == "--no-tai")   { do_tai = false; tai_given = true; }
        else if (a == "--tai")      { do_tai = true;  tai_given = true; }
        else if (a == "--media-clock" && i + 1 < argc) media_clock_path = argv[++i];
        else if (a == "--pipewire") do_pipewire = true;
        else if (a == "--alsa-sink" && i + 1 < argc) alsa_sinks.push_back(argv[++i]);
        else if (a == "--poll-ms"   && i + 1 < argc) { poll_ms = static_cast<unsigned>(std::stoul(argv[++i])); poll_given = true; }
        else if (a == "--phc"       && i + 1 < argc) phc_device = argv[++i];
        else if (a == "--ptp4l-uds" && i + 1 < argc) ptp4l_uds = argv[++i];
        else if (a == "--ptp-domain" && i + 1 < argc) ptp_domain = std::stoi(argv[++i]);
        else { std::fprintf(stderr, "Unknown option: %s\n", argv[i]); return 1; }
    }

    setvbuf(stdout, nullptr, _IOLBF, 0);  /* journal gets each line as it happens, not at exit */
    if (!phc_device.empty() && !tai_given) do_tai = false;  /* media time comes from the PHC; the system clock keeps NTP */
    if (!media_clock_path.empty() && phc_device.empty()) {
        std::fputs("ptp-clock-manager: --media-clock needs --phc\n", stderr);
        return 1;
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

    /* External PTP mode: PHC (ptp4l, hardware timestamps) -> driver */
    std::unique_ptr<PhcSource> phc;
    if (!phc_device.empty()) {
        phc = std::make_unique<PhcSource>(phc_device, ptp4l_uds, ptp_domain);
        if (!phc->open()) return 1;
        if (!poll_given) poll_ms = 125;  /* the servo's gains assume Sync-like intervals */
        std::printf("ptp-clock-manager: external PTP mode from %s (ptp4l at %s)\n", phc_device.c_str(), ptp4l_uds.c_str());
    }
    MediaClockServo mc_servo;
    std::unique_ptr<MediaClockPublisher> mc_pub;
    if (!media_clock_path.empty()) {
        /* Real-time priority: every MXL reader holds over if the record's seqlock stays odd,
         * so the update window must not be preempted. */
        sched_param sp{};
        sp.sched_priority = 60;
        if (sched_setscheduler(0, SCHED_FIFO, &sp) != 0)
            std::perror("ptp-clock-manager: SCHED_FIFO for the media clock publisher (continuing)");
        mc_pub = std::make_unique<MediaClockPublisher>(media_clock_path);
        if (!mc_pub->open()) return 1;
        std::printf("ptp-clock-manager: publishing MXL media clock at %s\n", media_clock_path.c_str());
    }
    Ptp4lState p4l;
    unsigned loops = 0;
    int last_err = 0;

    /* Create SHM */
    PtpClockShm* shm = shm_create();
    if (!shm) return 1;

    std::printf("ptp-clock-manager: running (poll=%u ms, drivers=%zu)\n",
                poll_ms, drivers.size());

    const auto interval = std::chrono::milliseconds(poll_ms);
    int64_t applied_freq_ppb = 0;

    while (g_running) {
        auto tick_start = std::chrono::steady_clock::now();

        if (phc) {
            if (loops++ % 8 == 0) {
                bool was = p4l.locked;
                p4l = phc->ptp4l_state();
                if (p4l.locked != was)
                    std::printf("ptp-clock-manager: ptp4l %s (offset %lld ns)\n", p4l.locked ? "locked" : "not locked",
                                static_cast<long long>(p4l.master_offset));
            }
            if (mc_pub) {
                if (auto r = phc->sample(CLOCK_MONOTONIC_RAW)) {
                    if (mc_servo.feed(static_cast<int64_t>(r->mono_ns), static_cast<int64_t>(r->ptp_ns)))
                        mc_pub->publish(mc_servo.mapping(), static_cast<int64_t>(r->mono_ns), p4l.locked, p4l.gmid);
                    if (loops % 80 == 1)  /* every ~10 s */
                        std::printf("ptp-clock-manager: media clock rate %+.3f ppm vs raw, phase error %lld ns, steps %u\n",
                                    (mc_servo.mapping().rate - 1.0) * 1e6, static_cast<long long>(mc_servo.last_phase_error_ns()), mc_servo.steps());
                }
            }
            if (auto s = phc->sample()) {
                int err = ptp.send_external_sample(s->ptp_ns, s->mono_ns, p4l.gmid, p4l.locked);
                if (err != last_err) {
                    std::fprintf(stderr, "ptp-clock-manager: external sample -> driver: %d%s\n", err,
                                 err == -401 ? " (driver not loaded with ptp_source=1)" : "");
                    last_err = err;
                }
            }
        }

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
