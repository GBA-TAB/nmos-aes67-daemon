#include "ptp_clock_shm.hpp"

#include <fcntl.h>
#include <sys/mman.h>
#include <time.h>
#include <unistd.h>

#include <cstring>

bool read_ptp_clock_shm(PtpClockShm& out) {
  int fd = shm_open(PTP_CLOCK_SHM_NAME, O_RDONLY, 0);
  if (fd < 0) return false;

  void* addr = mmap(nullptr, sizeof(PtpClockShm), PROT_READ, MAP_SHARED, fd, 0);
  close(fd);
  if (addr == MAP_FAILED) return false;

  const volatile auto* shm = static_cast<const volatile PtpClockShm*>(addr);
  bool consistent = false;
  for (int attempt = 0; attempt < 5; ++attempt) {
    uint32_t before = shm->update_count;
    if (before & 1) continue;  // writer mid-update
    std::memcpy(&out, const_cast<const PtpClockShm*>(shm), sizeof(out));
    if (shm->update_count == before) {
      consistent = true;
      break;
    }
  }
  munmap(addr, sizeof(PtpClockShm));

  return consistent && out.magic == PTP_CLOCK_SHM_MAGIC &&
         out.version == PTP_CLOCK_SHM_VERSION;
}

bool read_ptp_clock_shm_fresh(PtpClockShm& out, int64_t max_age_ns) {
  if (!read_ptp_clock_shm(out)) return false;

  struct timespec ts;
  clock_gettime(CLOCK_MONOTONIC, &ts);
  int64_t now_ns = static_cast<int64_t>(ts.tv_sec) * 1'000'000'000LL + ts.tv_nsec;

  return (now_ns - static_cast<int64_t>(out.monotonic_ns)) <= max_age_ns;
}
