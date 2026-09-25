//! `mxl-bridge audiotest ...` - the MXL side of the end-to-end audio test (contract/audiotest.py
//! drives it and does the wire side and the comparison).
//!
//!   audiotest gen  <config.json> <flow-uuid> <channels> <seconds> <seed>
//!       creates <flow-uuid> and writes a deterministic noise pattern into it: every sample is
//!       `pattern(seed, channel, mxl_index)`, so anything seen downstream can be checked on its own
//!       and its MXL time is known exactly. Prints one JSON line when writing starts.
//!   audiotest dump <config.json> <flow-uuid> <channels> <seconds>
//!       reads an existing flow and writes to stdout one JSON header line
//!       {"first_index", "channels", "frames"} followed by frames x channels little-endian i32
//!       (sample * 2^23, exact for 24-bit audio carried as float32).
//!
//!   audiotest capture <iface> <group> <port> <seconds>
//!       records the RTP packets sent to <group>:<port> as seen on <iface> - both what this host
//!       transmits and what it receives (AF_PACKET; needs CAP_NET_RAW and the host's network
//!       namespace: `docker run --net=host --cap-add NET_RAW`). Writes per packet: u64 capture time
//!       (CLOCK_TAI ns - the clock MXL indices count), u32 RTP timestamp, u16 sequence, u16
//!       payload length (little-endian), then the payload.
//!
//! Logs go to stderr; stdout carries only the data.

use std::io::Write;
use std::time::Duration;

use crate::config::Config;
use crate::mxl_flow::{MxlAudioFlow, MxlAudioFlowSource};

const BLOCK: usize = 480;
/// Amplitude limit of the pattern: +-2^20 (about -18 dBFS), whole 24-bit steps.
const PATTERN_BITS: u32 = 21;

/// splitmix64 - the analyser (contract/audiotest.py) implements the same function.
fn splitmix64(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// The 24-bit sample (as an integer in -2^20..2^20) at `index` on `channel`.
pub fn pattern(seed: u64, channel: u32, index: u64) -> i32 {
    let h = splitmix64(seed ^ ((channel as u64) << 48) ^ index);
    ((h >> (64 - PATTERN_BITS)) as i64 - (1i64 << (PATTERN_BITS - 1))) as i32
}

fn to_f32(v: i32) -> f32 {
    v as f32 / 8_388_608.0 // 2^23: exact for |v| < 2^24
}

fn mxl_so(cfg: &Config) -> anyhow::Result<std::path::PathBuf> {
    match &cfg.mxl_so_path {
        Some(p) => Ok(std::path::PathBuf::from(p)),
        None => crate::find_mxl_so(),
    }
}

pub fn run(args: &[String]) -> anyhow::Result<()> {
    match args.first().map(String::as_str) {
        Some("gen") if args.len() == 6 => generate(&args[1], &args[2], args[3].parse()?, args[4].parse()?, args[5].parse()?),
        Some("dump") if args.len() == 5 => dump(&args[1], &args[2], args[3].parse()?, args[4].parse()?),
        Some("capture") if args.len() == 5 => capture(&args[1], args[2].parse()?, args[3].parse()?, args[4].parse()?),
        _ => anyhow::bail!("usage: audiotest gen <config> <flow> <channels> <seconds> <seed> | audiotest dump <config> <flow> <channels> <seconds>"),
    }
}

fn generate(config: &str, flow: &str, channels: u32, seconds: f64, seed: u64) -> anyhow::Result<()> {
    let cfg = Config::load(config)?;
    let flow_id: uuid::Uuid = flow.parse()?;
    let mut w = MxlAudioFlow::create(
        &cfg,
        &mxl_so(&cfg)?,
        flow_id,
        uuid::Uuid::new_v4(),
        uuid::Uuid::new_v4(),
        "audiotest pattern",
        channels,
    )?;
    // Start one block behind "now", like a capture would, then keep pace with the clock.
    let mut next = w.current_index().saturating_sub(BLOCK as u64);
    let end = next + (seconds * cfg.sample_rate as f64) as u64;
    println!("{}", serde_json::json!({ "event": "writing", "flow": flow, "first_index": next, "seed": seed, "channels": channels }));
    std::io::stdout().flush()?;
    while next < end {
        while next + BLOCK as u64 <= w.current_index() && next < end {
            let planar: Vec<Vec<f32>> =
                (0..channels).map(|ch| (0..BLOCK as u64).map(|k| to_f32(pattern(seed, ch, next + k))).collect()).collect();
            w.write_at(next, &planar)?;
            next += BLOCK as u64;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    Ok(())
}

fn dump(config: &str, flow: &str, channels: usize, seconds: f64) -> anyhow::Result<()> {
    let cfg = Config::load(config)?;
    let r = MxlAudioFlowSource::open(&cfg, &mxl_so(&cfg)?, flow, channels)?;
    let frames = (seconds * cfg.sample_rate as f64) as usize / BLOCK * BLOCK;
    // 100 ms behind the head: already written, well inside the flow's history.
    let mut end = r.head_index()?.saturating_sub(4800);
    let first_index = end + 1 - BLOCK as u64;
    let mut out = std::io::BufWriter::new(std::io::stdout().lock());
    writeln!(out, "{}", serde_json::json!({ "first_index": first_index, "channels": channels, "frames": frames }))?;
    let mut done = 0;
    while done < frames {
        while r.head_index()? < end {
            std::thread::sleep(Duration::from_millis(2));
        }
        let planar = r.read_samples_at(end, BLOCK, Duration::from_millis(200))?;
        for k in 0..BLOCK {
            for ch in 0..channels {
                let v = (planar[ch][k] * 8_388_608.0).round() as i32;
                out.write_all(&v.to_le_bytes())?;
            }
        }
        end += BLOCK as u64;
        done += BLOCK;
    }
    out.flush()?;
    Ok(())
}

fn tai_ns() -> u64 {
    let mut t = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { libc::clock_gettime(libc::CLOCK_TAI, &mut t) };
    t.tv_sec as u64 * 1_000_000_000 + t.tv_nsec as u64
}

fn capture(iface: &str, group: std::net::Ipv4Addr, port: u16, seconds: f64) -> anyhow::Result<()> {
    const ETH_P_ALL: u16 = 0x0003;
    let fd = unsafe { libc::socket(libc::AF_PACKET, libc::SOCK_RAW, (ETH_P_ALL).to_be() as i32) };
    anyhow::ensure!(fd >= 0, "AF_PACKET socket: {} (needs CAP_NET_RAW)", std::io::Error::last_os_error());
    let ifindex = unsafe { libc::if_nametoindex(std::ffi::CString::new(iface)?.as_ptr()) };
    anyhow::ensure!(ifindex != 0, "no interface {iface}");
    let mut sll: libc::sockaddr_ll = unsafe { std::mem::zeroed() };
    sll.sll_family = libc::AF_PACKET as u16;
    sll.sll_protocol = ETH_P_ALL.to_be();
    sll.sll_ifindex = ifindex as i32;
    let rc = unsafe { libc::bind(fd, &sll as *const _ as *const libc::sockaddr, std::mem::size_of::<libc::sockaddr_ll>() as u32) };
    anyhow::ensure!(rc == 0, "bind {iface}: {}", std::io::Error::last_os_error());
    // The NIC carries every stream (and more); a small default buffer drops packets under that
    // load. 32 MB, forced (needs CAP_NET_ADMIN; falls back to the plain request).
    let rcvbuf: libc::c_int = 32 << 20;
    let sz = std::mem::size_of::<libc::c_int>() as u32;
    unsafe {
        if libc::setsockopt(fd, libc::SOL_SOCKET, libc::SO_RCVBUFFORCE, &rcvbuf as *const _ as *const libc::c_void, sz) != 0 {
            libc::setsockopt(fd, libc::SOL_SOCKET, libc::SO_RCVBUF, &rcvbuf as *const _ as *const libc::c_void, sz);
        }
    }
    let tv = libc::timeval { tv_sec: 0, tv_usec: 200_000 };
    unsafe { libc::setsockopt(fd, libc::SOL_SOCKET, libc::SO_RCVTIMEO, &tv as *const _ as *const libc::c_void, std::mem::size_of::<libc::timeval>() as u32) };

    let want = group.octets();
    let deadline = std::time::Instant::now() + Duration::from_secs_f64(seconds);
    let mut out = std::io::BufWriter::new(std::io::stdout().lock());
    let mut buf = vec![0u8; 9000];
    let mut packets = 0u64;
    while std::time::Instant::now() < deadline {
        let n = unsafe { libc::recv(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len(), 0) };
        if n <= 0 {
            continue;
        }
        let f = &buf[..n as usize];
        // Ethernet (optionally one 802.1Q tag) / IPv4 / UDP / RTP.
        let mut off = 14;
        if f.len() < off + 20 {
            continue;
        }
        let mut ethertype = u16::from_be_bytes([f[12], f[13]]);
        if ethertype == 0x8100 {
            ethertype = u16::from_be_bytes([f[16], f[17]]);
            off = 18;
        }
        if ethertype != 0x0800 || f.len() < off + 20 || f[off + 9] != 17 || f[off + 16..off + 20] != want {
            continue;
        }
        let ip_len = ((f[off] & 0x0f) as usize) * 4;
        let udp = off + ip_len;
        if f.len() < udp + 8 + 12 || u16::from_be_bytes([f[udp + 2], f[udp + 3]]) != port {
            continue;
        }
        let rtp = udp + 8;
        let cc = (f[rtp] & 0x0f) as usize;
        let mut payload = rtp + 12 + 4 * cc;
        if f[rtp] & 0x10 != 0 && f.len() >= payload + 4 {
            payload += 4 + 4 * u16::from_be_bytes([f[payload + 2], f[payload + 3]]) as usize;
        }
        let udp_end = udp + u16::from_be_bytes([f[udp + 4], f[udp + 5]]) as usize;
        if payload > udp_end || udp_end > f.len() {
            continue;
        }
        let seq = u16::from_be_bytes([f[rtp + 2], f[rtp + 3]]);
        let ts = u32::from_be_bytes([f[rtp + 4], f[rtp + 5], f[rtp + 6], f[rtp + 7]]);
        let body = &f[payload..udp_end];
        out.write_all(&tai_ns().to_le_bytes())?;
        out.write_all(&ts.to_le_bytes())?;
        out.write_all(&seq.to_le_bytes())?;
        out.write_all(&(body.len() as u16).to_le_bytes())?;
        out.write_all(body)?;
        packets += 1;
    }
    out.flush()?;
    // Packets the kernel dropped because this socket fell behind: capture-side loss, not the stream's.
    #[repr(C)]
    struct TpacketStats {
        tp_packets: u32,
        tp_drops: u32,
    }
    let mut st = TpacketStats { tp_packets: 0, tp_drops: 0 };
    let mut len = std::mem::size_of::<TpacketStats>() as u32;
    const PACKET_STATISTICS: i32 = 6;
    unsafe { libc::getsockopt(fd, libc::SOL_PACKET, PACKET_STATISTICS, &mut st as *mut _ as *mut libc::c_void, &mut len) };
    unsafe { libc::close(fd) };
    eprintln!("capture: {packets} RTP packets for {group}:{port} on {iface}; capture_drops={}", st.tp_drops);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pattern_is_deterministic_bounded_and_exact_in_f32() {
        assert_eq!(pattern(7, 3, 1_000_000), pattern(7, 3, 1_000_000));
        assert_ne!(pattern(7, 3, 1_000_000), pattern(7, 4, 1_000_000));
        for i in 0..10_000u64 {
            let v = pattern(42, (i % 8) as u32, i);
            assert!((-(1 << 20)..(1 << 20)).contains(&v));
            assert_eq!((to_f32(v) * 8_388_608.0) as i32, v, "float32 carries 24-bit samples exactly");
        }
    }

    /// Pinned values: contract/audiotest.py asserts the same ones.
    #[test]
    fn pattern_reference_values() {
        assert_eq!(pattern(1, 0, 0), 139_589);
        assert_eq!(pattern(1, 7, 123_456_789), -175_737);
    }
}
