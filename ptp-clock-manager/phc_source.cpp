#include "phc_source.hpp"

#include <cerrno>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <ctime>
#include <fcntl.h>
#include <linux/ptp_clock.h>
#include <sys/ioctl.h>
#include <unistd.h>

PhcSource::PhcSource(std::string device, std::string ptp4l_uds)
    : device_(std::move(device)), uds_(std::move(ptp4l_uds)) {}

PhcSource::~PhcSource() {
    if (fd_ >= 0) ::close(fd_);
}

bool PhcSource::open() {
    fd_ = ::open(device_.c_str(), O_RDONLY);
    if (fd_ < 0) {
        std::fprintf(stderr, "PhcSource: open %s: %s\n", device_.c_str(), std::strerror(errno));
        return false;
    }
    return true;
}

static uint64_t ns_of(const ptp_clock_time& t) {
    return static_cast<uint64_t>(t.sec) * 1'000'000'000ULL + t.nsec;
}

static uint64_t now_ns(clockid_t c) {
    timespec ts{};
    clock_gettime(c, &ts);
    return static_cast<uint64_t>(ts.tv_sec) * 1'000'000'000ULL + ts.tv_nsec;
}

std::optional<PhcSample> PhcSource::sample(clockid_t clock) {
    if (fd_ < 0) return std::nullopt;
    ptp_sys_offset_extended req{};
    req.n_samples = 5;
    // Kernels >= 6.13 take the system clock to bracket with in the first reserved word (clockid);
    // older ones reject a non-zero value, then REALTIME is converted below.
    if (monotonic_ioctl_) req.rsv[0] = static_cast<unsigned>(clock);
    if (ioctl(fd_, PTP_SYS_OFFSET_EXTENDED, &req) < 0) {
        if (errno == EINVAL && monotonic_ioctl_) {
            std::fputs("PhcSource: kernel brackets with CLOCK_REALTIME only - converting\n", stderr);
            monotonic_ioctl_ = false;
            return sample(clock);
        }
        std::perror("PhcSource: PTP_SYS_OFFSET_EXTENDED");
        return std::nullopt;
    }
    // The narrowest bracket is the read least disturbed by interrupts or preemption.
    unsigned best = 0;
    uint64_t best_w = UINT64_MAX;
    for (unsigned i = 0; i < req.n_samples; ++i) {
        uint64_t w = ns_of(req.ts[i][2]) - ns_of(req.ts[i][0]);
        if (w < best_w) { best_w = w; best = i; }
    }
    uint64_t sys = ns_of(req.ts[best][0]) + best_w / 2;
    if (!monotonic_ioctl_) {
        // REALTIME -> the requested clock, read back to back (good to a microsecond or so).
        uint64_t rt = now_ns(CLOCK_REALTIME), target = now_ns(clock);
        sys = sys - rt + target;
    }
    return PhcSample{ns_of(req.ts[best][1]), sys, static_cast<uint32_t>(best_w)};
}

Ptp4lState PhcSource::ptp4l_state() {
    Ptp4lState st;
    char cmd[512];
    std::snprintf(cmd, sizeof(cmd),
                  "pmc -u -b 0 -s '%s' -i /tmp/ptp-clock-manager.pmc 'GET TIME_STATUS_NP' 2>/dev/null",
                  uds_.c_str());
    FILE* p = popen(cmd, "r");
    if (!p) return st;
    char line[256];
    bool gm_present = false;
    bool have_offset = false;
    while (std::fgets(line, sizeof(line), p)) {
        char key[64], val[128];
        if (std::sscanf(line, " %63s %127s", key, val) != 2) continue;
        if (!std::strcmp(key, "master_offset")) { st.master_offset = std::strtoll(val, nullptr, 10); have_offset = true; }
        else if (!std::strcmp(key, "gmPresent")) gm_present = !std::strcmp(val, "true");
        else if (!std::strcmp(key, "gmIdentity")) {
            // "d0699e.fffe.138afc" -> 8 bytes in wire order
            unsigned b[8];
            if (std::sscanf(val, "%2x%2x%2x.%2x%2x.%2x%2x%2x", &b[0], &b[1], &b[2], &b[3], &b[4], &b[5], &b[6], &b[7]) == 8) {
                uint8_t bytes[8];
                for (int i = 0; i < 8; ++i) bytes[i] = static_cast<uint8_t>(b[i]);
                std::memcpy(&st.gmid, bytes, 8);
            }
        }
    }
    pclose(p);
    // Locked: a grandmaster is present and ptp4l's servo has pulled the PHC within 1 us of it.
    st.locked = gm_present && have_offset && std::llabs(st.master_offset) < 1000;
    return st;
}
