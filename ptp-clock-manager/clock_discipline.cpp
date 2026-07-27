#include "clock_discipline.hpp"

#include <algorithm>
#include <cmath>
#include <cstdio>
#include <cstring>
#include <sys/timex.h>  /* adjtimex, clock_adjtime, ADJ_FREQUENCY, ADJ_OFFSET, ADJ_NANO */
#include <time.h>

ClockDiscipline::ClockDiscipline() : cfg_(Config{}) { set_tai_offset(); }
ClockDiscipline::ClockDiscipline(Config cfg) : cfg_(cfg) {
    set_tai_offset();
}

void ClockDiscipline::set_tai_offset() {
    struct timex tx{};
    if (adjtimex(&tx) >= 0 && tx.tai > 0)
        tai_offset_s_ = tx.tai;
    /* Ensure the kernel knows the TAI offset (required for CLOCK_TAI accuracy) */
    if (tx.tai == 0) {
        struct timex set{};
        set.modes    = ADJ_TAI;
        set.constant = static_cast<long>(tai_offset_s_);  /* field reused for TAI */
        adjtimex(&set);
    }
}

void ClockDiscipline::apply_freq(double freq_ppb) {
    /* ADJ_FREQUENCY unit is (ppm << 16), i.e. scaled_ppm = ppb * 65536 / 1000 */
    long scaled = static_cast<long>(freq_ppb * 65536.0 / 1000.0);
    struct timex tx{};
    tx.modes  = ADJ_FREQUENCY;
    tx.freq   = scaled;
    if (clock_adjtime(CLOCK_REALTIME, &tx) < 0)
        std::perror("ClockDiscipline: clock_adjtime ADJ_FREQUENCY");
}

double ClockDiscipline::feed(int64_t offset_ns) {
    if (first_) {
        first_ = false;
        if (std::abs(static_cast<double>(offset_ns)) > cfg_.step_threshold_ns) {
            step(offset_ns);
            return 0.0;
        }
    }

    /* PI. offset_ns is local-minus-master (negative = local clock behind master),
     * so the correction is -offset: behind -> speed up (positive freq). */
    double err = -static_cast<double>(offset_ns);
    integral_ += err * cfg_.ki;
    integral_  = std::clamp(integral_, -cfg_.max_freq_ppb, cfg_.max_freq_ppb);
    double freq = std::clamp(cfg_.kp * err + integral_,
                             -cfg_.max_freq_ppb, cfg_.max_freq_ppb);
    apply_freq(freq);
    return freq;
}

void ClockDiscipline::step(int64_t offset_ns) {
    /* offset_ns is local-minus-master; add the negation to step onto master time. */
    const int64_t correction_ns = -offset_ns;

    struct timespec delta{};
    delta.tv_sec  = correction_ns / 1'000'000'000LL;
    delta.tv_nsec = correction_ns % 1'000'000'000LL;

    struct timespec now{};
    clock_gettime(CLOCK_REALTIME, &now);

    struct timespec next{};
    next.tv_sec  = now.tv_sec  + delta.tv_sec;
    next.tv_nsec = now.tv_nsec + delta.tv_nsec;
    if (next.tv_nsec >= 1'000'000'000L) {
        next.tv_sec++;
        next.tv_nsec -= 1'000'000'000L;
    } else if (next.tv_nsec < 0) {
        next.tv_sec--;
        next.tv_nsec += 1'000'000'000L;
    }

    if (clock_settime(CLOCK_REALTIME, &next) < 0)
        std::perror("ClockDiscipline: clock_settime");

    reset();
}

void ClockDiscipline::reset() {
    integral_ = 0.0;
    first_    = true;
}
