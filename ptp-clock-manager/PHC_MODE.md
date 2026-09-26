# Media clock from PTP: NIC hardware timestamps for the RAVENNA driver and MXL (prototype)

Status: **prototype, not yet run end to end** (2026-09-26). Off by default; nothing changes unless
the driver is loaded with `ptp_source=1` and ptp-clock-manager is started with `--phc`.

## Why

The RAVENNA driver runs its own minimal PTP slave: it timestamps Sync arrivals in software in its
netfilter hook (`get_clock_time()` = `CLOCK_MONOTONIC`), takes the grandmaster's origin time from
Follow_Up, and never measures the path delay (no Delay_Req). Its media clock (the 1 ms TIC and every
RTP timestamp) is servoed to those samples.

The lab's Intel I210 (`igb`) timestamps PTP packets in hardware. Measured with ptp4l on the same
network and grandmaster (Luminex GigaCore, domain 127), with the driver loaded:

| | offset to the grandmaster |
|---|---|
| ptp4l, I210 hardware timestamps | 2-7 ns rms, 10 ns max, path delay measured |
| driver, software timestamps | microseconds (interrupt and softirq latency in every sample) |

(The I210's own oscillator runs about 34 ppm off; ptp4l corrects it within the I210's +-62.5 ppm range.)

## Design

```
grandmaster --PTP--> I210 (hw timestamps) --> ptp4l --disciplines--> /dev/ptp0 (PHC)
                                                                            |
ptp-clock-manager --phc /dev/ptp0:  every 125 ms read PHC vs CLOCK_MONOTONIC (PTP_SYS_OFFSET_EXTENDED,
                                    best of 5), ask ptp4l for lock + grandmaster (pmc TIME_STATUS_NP)
                                                                            |
                                    netlink MT_ALSA_Msg_SetPTPExternalSample {PTP time, local time, GMID, locked}
                                                                            v
RAVENNA driver, ptp_source=1:       ProcessExternalSample() = a Sync (T2 = local time) + Follow_Up
                                    (T1 = PTP time) -> the unchanged ProcessT1() servo and TIC logic
```

- **The system clock is not touched.** An earlier idea was phc2sys disciplining `CLOCK_TAI` and the
  driver ticking on `CLOCK_TAI`. The lab grandmaster's timescale is about 3400 s off real TAI, so that
  would have moved the host's wall clock by an hour. Here only the NIC's PHC follows the grandmaster;
  the driver keeps its own clock and just gets far better (PTP time, local time) pairs.
- **The servo is unchanged.** `ProcessExternalSample()` fills exactly the fields the Sync path fills
  and calls `ProcessT1()`. Samples come at 8 Hz, a typical Sync rate, so its gains still fit.
- **Fail-safe.** The Sync watchdog in `timerProcess()` still runs: no sample for 2 s drops the lock.
  A sample with `locked = 0` (ptp4l not within 1 us of a grandmaster) drops it immediately.
- **PTP packets** still pass through the driver's hook to user space (`NF_ACCEPT`); with
  `ptp_source=1` the driver ignores them.
- **Status**: `GetPTPStatus` works as before (lock state, GMID, offset), so the daemon, its web UI
  and ptp-clock-manager's CLOCK_TAI discipline keep working.

## MXL media clock (`--media-clock`)

MXL's time (`mxlGetTime()`, every flow index) is `CLOCK_TAI` - the system clock, steered by NTP. The
RAVENNA media clock follows the PTP grandmaster. With a free-running grandmaster the two drift apart
(lab, 2026-09-26: +27 ppm), and every MXL flow bridged to 2110 slips against its hardware: mxl-bridge
raised its tx read delay by 1 ms every few minutes and snapped its rx write index about once a minute
per stream. NTP wander would do the same on a smaller scale even with a GPS grandmaster.

So MXL gets its own clock, decoupled from the system clock (which stays on NTP):

```
ptp4l (hw timestamps) -> /dev/ptp0 -> ptp-clock-manager --phc /dev/ptp0 --media-clock <domain>/.media-clock
                                      8 Hz: PHC vs CLOCK_MONOTONIC_RAW -> servo -> record
MXL apps: MXL_MEDIA_CLOCK=<domain in the pod>/.media-clock  (libmxl, GBA-TAB/mxl 083e0cf1)
          media time = refMediaNs + (CLOCK_MONOTONIC_RAW - refRawNs) * rate   (lock-free seqlock read)
```

- The record (`mxl/mediaclock.h`, mirrored in `media_clock.hpp`) lives in the MXL domain directory,
  which every MXL pod already mounts; MXL ignores it when listing flows.
- The servo (`MediaClockServo`) re-anchors at the current mapped time on every update, so MXL time is
  continuous and never runs backwards, and steers the phase error out over 2 s. Only a PHC step of
  more than 1 ms (ptp4l at startup) is followed as a jump. Test: `media-clock-test` (+27 ppm, 30 ns
  read noise, a 5 ms step: monotonic, one re-anchor, settled error < 130 ns).
- libmxl holds over on the last rate if updates stop; it never falls back to `CLOCK_TAI`.
- The timescale is the grandmaster's. With a GPS-locked grandmaster that is TAI; either way MXL
  indices then equal RTP timestamps (mod 2^32).
- `--phc` turns ptp-clock-manager's `CLOCK_TAI` discipline off (`--tai` re-enables it): the system
  clock stays on NTP.

## Pieces

| | |
|---|---|
| driver | module option `ptp_source` (0 default / 1), `ProcessExternalSample()` in `PTP.c`, command in `manager.c` |
| shared header | `MT_ALSA_Msg_SetPTPExternalSample` appended (existing ids unchanged), `TPTPExternalSample` |
| ptp-clock-manager | `--phc DEVICE`, `--ptp4l-uds PATH` (default `/var/run/ptp4lro`), `--ptp-domain N` (ptp4l's domain), `phc_source.cpp` |
| ptp4l | `phc-mode/ptp4l.conf`: slave only, hardware timestamps, `priority1`/`clockClass` 255 |

## Trying it (root)

```bash
# 1. ptp4l on the media NIC (keeps running)
sudo ptp4l -f ptp-clock-manager/phc-mode/ptp4l.conf -m
# 2. driver in external mode: stop aes67-daemon, ptp-clock-manager and the bridge first
sudo modprobe -r MergingRavennaALSA
sudo modprobe MergingRavennaALSA ptp_source=1 audio_cpu_affinity=6
# 3. ptp-clock-manager feeding it (as root for /dev/ptp0; the service user needs a udev rule, below)
sudo ./build/ptp-clock-manager --phc /dev/ptp0 --ptp-domain 127
# 4. start aes67-daemon; PTP status should read locked, GMID = the grandmaster
```

Back to normal: reload the driver without `ptp_source`, start the ptp-clock-manager service.

For the service: its user needs read access to the PHC, e.g.
`/etc/udev/rules.d/60-ptp.rules`: `KERNEL=="ptp[0-9]*", GROUP="ptpclock", MODE="0640"`.

## To verify

- Lock: driver status locked within seconds of the first sample, GMID matches ptp4l's.
- RTP timestamps vs the grandmaster: capture a stream and compare with another PTP-locked device
  (or ptp4l's PHC) - the offset should drop from microseconds to well under 1 us.
- The soak (`mxl-bridge/contract/soak.py`): bit-exact, no RTP timestamp jumps, tick jitter.
- Unlock/relock: stop ptp4l -> lock lost within 2 s; start it -> relock, audio resumes.
