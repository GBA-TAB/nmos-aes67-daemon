#include "media_clock.hpp"

#include <algorithm>
#include <cmath>
#include <cstdio>
#include <cstring>
#include <fcntl.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <unistd.h>

int64_t MediaClockServo::at(int64_t raw) const {
    return m_.ref_media + std::llround(static_cast<double>(raw - m_.ref_raw) * m_.rate);
}

bool MediaClockServo::feed(int64_t raw_ns, int64_t phc_ns) {
    window_.emplace_back(raw_ns, phc_ns);
    while (window_.size() > 2 && static_cast<double>(raw_ns - window_.front().first) > rate_window_s * 1e9)
        window_.pop_front();

    // Rate over the window (endpoints: hardware-timestamped PHC reads are good to tens of ns, so
    // 8 s gives ~0.01 ppm); 1.0 until there is a second sample.
    double rate = 1.0;
    if (window_.size() >= 2) {
        const auto& [r0, p0] = window_.front();
        rate = static_cast<double>(phc_ns - p0) / static_cast<double>(raw_ns - r0);
    }

    if (!have_) {
        m_ = {raw_ns, phc_ns, rate};
        have_ = true;
        return true;
    }
    const int64_t err = phc_ns - at(raw_ns);
    last_err_ = err;
    if (std::llabs(err) > step_threshold_ns) {
        // The PHC moved (ptp4l step) or the rate estimate was far off: follow it, restart the rate.
        std::fprintf(stderr, "MediaClockServo: phase error %lld ns - re-anchoring (step)\n", static_cast<long long>(err));
        ++steps_;
        window_.clear();
        window_.emplace_back(raw_ns, phc_ns);
        m_ = {raw_ns, phc_ns, m_.rate};
        return true;
    }
    // Continuous: anchor where the current mapping is now; steer the phase error out over tau.
    const double correction = std::clamp(static_cast<double>(err) / (phase_time_constant_s * 1e9), -50e-6, 50e-6);
    m_ = {raw_ns, at(raw_ns), rate + correction};
    return true;
}

MediaClockPublisher::~MediaClockPublisher() {
    if (rec_) munmap(rec_, sizeof(*rec_));
}

bool MediaClockPublisher::open() {
    int fd = ::open(path_.c_str(), O_RDWR | O_CREAT | O_CLOEXEC, 0644);
    if (fd < 0) {
        std::fprintf(stderr, "MediaClockPublisher: open %s: %s\n", path_.c_str(), std::strerror(errno));
        return false;
    }
    fchmod(fd, 0644);
    if (ftruncate(fd, sizeof(mxlMediaClockRecord)) < 0) {
        std::perror("MediaClockPublisher: ftruncate");
        ::close(fd);
        return false;
    }
    void* p = mmap(nullptr, sizeof(mxlMediaClockRecord), PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    ::close(fd);
    if (p == MAP_FAILED) {
        std::perror("MediaClockPublisher: mmap");
        return false;
    }
    rec_ = static_cast<mxlMediaClockRecord*>(p);
    // Keep a record written by an earlier run (readers keep holding over on it); stamp it ours.
    if (rec_->magic != MXL_MEDIA_CLOCK_MAGIC || rec_->version != MXL_MEDIA_CLOCK_VERSION) {
        std::memset(rec_, 0, sizeof(*rec_));
        rec_->version = MXL_MEDIA_CLOCK_VERSION;
        __atomic_store_n(&rec_->magic, MXL_MEDIA_CLOCK_MAGIC, __ATOMIC_RELEASE);
    }
    if (rec_->seq & 1U) rec_->seq++;  // a previous writer died mid-update
    return true;
}

void MediaClockPublisher::publish(const MediaClockServo::Mapping& m, int64_t updated_raw, bool locked, uint64_t gmid) {
    if (!rec_) return;
    __atomic_add_fetch(&rec_->seq, 1U, __ATOMIC_RELEASE);  // odd: writing
    __atomic_thread_fence(__ATOMIC_RELEASE);
    rec_->refRawNs = m.ref_raw;
    rec_->refMediaNs = m.ref_media;
    rec_->rate = m.rate;
    rec_->updatedRawNs = updated_raw;
    rec_->flags = locked ? MXL_MEDIA_CLOCK_LOCKED : 0U;
    std::memcpy(rec_->gmIdentity, &gmid, 8);
    __atomic_thread_fence(__ATOMIC_RELEASE);
    __atomic_add_fetch(&rec_->seq, 1U, __ATOMIC_RELEASE);  // even: done
}
