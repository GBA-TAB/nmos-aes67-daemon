//! `mxl-bridge audiotest ...` - the MXL side of the end-to-end audio test (contract/audiotest.py
//! drives it and does the wire side and the comparison).
//!
//!   audiotest gen  <config.json> <flow-uuid> <channels> <seconds> <seed> [block-frames]
//!       creates <flow-uuid> and writes a deterministic noise pattern into it: every sample is
//!       `pattern(seed, channel, mxl_index)`, so anything seen downstream can be checked on its own
//!       and its MXL time is known exactly. Prints one JSON line when writing starts.
//!   audiotest dump <config.json> <flow-uuid> <channels> <seconds>
//!       reads an existing flow and writes to stdout one JSON header line
//!       {"first_index", "channels", "frames"} followed by frames x channels little-endian i32
//!       (sample * 2^23, exact for 24-bit audio carried as float32), then a newline and a JSON
//!       trailer {"heads": [[tai_ns, head_index], ...]}: when each sample became readable.
//!
//!   audiotest capture <iface> <group> <port> <seconds>
//!       records the RTP packets sent to <group>:<port> as seen on <iface> - both what this host
//!       transmits and what it receives (AF_PACKET; needs CAP_NET_RAW and the host's network
//!       namespace: `docker run --net=host --cap-add NET_RAW`). Writes per packet: u64 capture time
//!       (CLOCK_TAI ns - the clock MXL indices count), u32 RTP timestamp, u16 sequence, u16
//!       payload length (little-endian), then the payload.
//!
//!   audiotest relay <iface> <from-group> <to-group> <count> <seconds>
//!       re-sends, unchanged, the RTP this host transmits to <from-group> + i (i < count) to
//!       <to-group> + i, with TTL 0 and multicast loopback: the packets never leave the host but
//!       reach its own RAVENNA driver, which does not receive its own transmissions otherwise.
//!       Loads the rx side with the tx streams' audio (RTP timestamps kept) for soak tests.
//!       <seconds> 0 = until killed. Same privileges as capture. Also checks every stream's RTP
//!       continuity and logs each irregularity (missing packets, skipped sequence numbers only,
//!       timestamp jumps) plus a JSON "relay totals" line every 60 s on stderr.
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
        Some("gen") if args.len() == 6 || args.len() == 7 => {
            let block = args.get(6).map(|b| b.parse()).transpose()?.unwrap_or(BLOCK);
            generate(&args[1], &args[2], args[3].parse()?, args[4].parse()?, args[5].parse()?, block)
        }
        Some("dump") if args.len() == 5 => dump(&args[1], &args[2], args[3].parse()?, args[4].parse()?),
        Some("capture") if args.len() == 5 => capture(&args[1], args[2].parse()?, args[3].parse()?, args[4].parse()?),
        Some("relay") if args.len() == 6 => relay(&args[1], args[2].parse()?, args[3].parse()?, args[4].parse()?, args[5].parse()?),
        _ => anyhow::bail!(
            "usage: audiotest gen <config> <flow> <channels> <seconds> <seed> [block] | dump <config> <flow> <channels> <seconds> | capture <iface> <group> <port> <seconds> | relay <iface> <from-group> <to-group> <count> <seconds>"
        ),
    }
}

fn generate(config: &str, flow: &str, channels: u32, seconds: f64, seed: u64, block: usize) -> anyhow::Result<()> {
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
    let mut next = w.current_index().saturating_sub(block as u64);
    let end = next + (seconds * cfg.sample_rate as f64) as u64;
    println!("{}", serde_json::json!({ "event": "writing", "flow": flow, "first_index": next, "seed": seed, "channels": channels }));
    std::io::stdout().flush()?;
    // Once a minute a heartbeat line: keeps an otherwise idle `kubectl exec` stream alive (the
    // kubelet closes idle streams after 4 h) and reports how late this writer committed its blocks
    // (lateness = how far "now" was past the block's end), to tell a late writer from a reader fault.
    let mut beat = std::time::Instant::now();
    let (mut max_late, mut late_over_2ms, mut blocks) = (0u64, 0u64, 0u64);
    while next < end {
        while next + block as u64 <= w.current_index() && next < end {
            let planar: Vec<Vec<f32>> =
                (0..channels).map(|ch| (0..block as u64).map(|k| to_f32(pattern(seed, ch, next + k))).collect()).collect();
            let late = w.current_index().saturating_sub(next + block as u64);
            w.write_at(next, &planar)?;
            next += block as u64;
            blocks += 1;
            max_late = max_late.max(late);
            late_over_2ms += (late * 1000 > 2 * cfg.sample_rate as u64) as u64;
        }
        if beat.elapsed() >= Duration::from_secs(60) {
            let ms = |v: u64| v as f64 * 1000.0 / cfg.sample_rate as f64;
            println!("{}", serde_json::json!({ "event": "alive", "tai_s": tai_ns() / 1_000_000_000, "blocks": blocks, "max_late_ms": ms(max_late), "late_over_2ms": late_over_2ms }));
            std::io::stdout().flush()?;
            (max_late, late_over_2ms, blocks) = (0, 0, 0);
            beat = std::time::Instant::now();
        }
        std::thread::sleep(Duration::from_micros(500));
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
    // (TAI ns, head) whenever the head moves: when each sample became readable.
    let mut heads: Vec<(u64, u64)> = Vec::new();
    let mut last_head = 0;
    let mut done = 0;
    while done < frames {
        loop {
            let h = r.head_index()?;
            if h != last_head {
                heads.push((tai_ns(), h));
                last_head = h;
            }
            if h >= end {
                break;
            }
            std::thread::sleep(Duration::from_micros(200));
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
    // Trailer after the samples: the head trace.
    writeln!(out)?;
    writeln!(out, "{}", serde_json::json!({ "heads": heads }))?;
    out.flush()?;
    Ok(())
}

fn tai_ns() -> u64 {
    let mut t = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { libc::clock_gettime(libc::CLOCK_TAI, &mut t) };
    t.tv_sec as u64 * 1_000_000_000 + t.tv_nsec as u64
}

/// An AF_PACKET socket bound to `iface` with a 32 MB receive buffer and a 200 ms timeout.
fn packet_socket(iface: &str) -> anyhow::Result<(i32, u32)> {
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
    Ok((fd, ifindex))
}

/// Ethernet (optionally one 802.1Q tag) / IPv4 / UDP: (destination address, destination port, UDP
/// payload range in the frame).
fn udp_of(f: &[u8]) -> Option<([u8; 4], u16, std::ops::Range<usize>)> {
    let mut off = 14;
    if f.len() < off + 20 {
        return None;
    }
    let mut ethertype = u16::from_be_bytes([f[12], f[13]]);
    if ethertype == 0x8100 {
        ethertype = u16::from_be_bytes([f[16], f[17]]);
        off = 18;
    }
    if ethertype != 0x0800 || f.len() < off + 20 || f[off + 9] != 17 {
        return None;
    }
    let udp = off + ((f[off] & 0x0f) as usize) * 4;
    if f.len() < udp + 8 {
        return None;
    }
    let end = udp + u16::from_be_bytes([f[udp + 4], f[udp + 5]]) as usize;
    if end > f.len() || end < udp + 8 {
        return None;
    }
    Some(([f[off + 16], f[off + 17], f[off + 18], f[off + 19]], u16::from_be_bytes([f[udp + 2], f[udp + 3]]), udp + 8..end))
}

fn capture(iface: &str, group: std::net::Ipv4Addr, port: u16, seconds: f64) -> anyhow::Result<()> {
    let (fd, ifindex) = packet_socket(iface)?;

    // Join the group like a real receiver: with IGMP snooping the switch only forwards a group to
    // ports that asked for it (a stream this host transmits is seen regardless).
    let join_fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0) };
    let mreq = libc::ip_mreqn {
        imr_multiaddr: libc::in_addr { s_addr: u32::from_ne_bytes(group.octets()) },
        imr_address: libc::in_addr { s_addr: 0 },
        imr_ifindex: ifindex as i32,
    };
    if join_fd < 0
        || unsafe { libc::setsockopt(join_fd, libc::IPPROTO_IP, libc::IP_ADD_MEMBERSHIP, &mreq as *const _ as *const libc::c_void, std::mem::size_of::<libc::ip_mreqn>() as u32) } != 0
    {
        eprintln!("capture: joining {group} failed: {} (continuing)", std::io::Error::last_os_error());
    }
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
        if packets == 0 {
            eprintln!(
                "capture: eth {:02x}{:02x} vlan {} dst-mac {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} | ip ihl {} flags {:02x} | udp {}->{} csum {:04x} | source {}.{}.{}.{} ttl {} dscp {} | rtp v{} padding {} extension {} csrc {} marker {} pt {} ssrc {:08x}",
                f[12], f[13], if off == 18 { format!("{}", u16::from_be_bytes([f[14], f[15]]) & 0x0fff) } else { "-".into() },
                f[0], f[1], f[2], f[3], f[4], f[5], f[off] & 0x0f, f[off + 6],
                u16::from_be_bytes([f[udp], f[udp + 1]]), u16::from_be_bytes([f[udp + 2], f[udp + 3]]), u16::from_be_bytes([f[udp + 6], f[udp + 7]]),
                f[off + 12], f[off + 13], f[off + 14], f[off + 15], f[off + 8], f[off + 1] >> 2,
                f[rtp] >> 6, (f[rtp] >> 5) & 1, (f[rtp] >> 4) & 1, f[rtp] & 0x0f, f[rtp + 1] >> 7, f[rtp + 1] & 0x7f,
                u32::from_be_bytes([f[rtp + 8], f[rtp + 9], f[rtp + 10], f[rtp + 11]])
            );
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
    unsafe {
        libc::close(fd);
        if join_fd >= 0 {
            libc::close(join_fd);
        }
    }
    eprintln!("capture: {packets} RTP packets for {group}:{port} on {iface}; capture_drops={}", st.tp_drops);
    Ok(())
}

/// Per-stream RTP continuity as the relay sees it (every packet this host transmits).
#[derive(Default, Debug)]
struct Continuity {
    last: Option<(u16, u32)>,
    /// RTP timestamp step of a regular packet (learned from the first consecutive pair).
    step: u32,
    packets: u64,
    /// Sequence and timestamp both jump: packets really missing.
    missing: u64,
    /// Sequence jumps but the timestamp is continuous: only numbers skipped.
    seq_only: u64,
    /// Consecutive sequence, irregular timestamp.
    ts_jump: u64,
    /// Anything else (duplicates, reordering, restarts).
    other: u64,
}

impl Continuity {
    /// Returns a description of an irregularity, if this packet shows one.
    fn packet(&mut self, seq: u16, ts: u32) -> Option<String> {
        self.packets += 1;
        let Some((ls, lt)) = self.last.replace((seq, ts)) else { return None };
        let (dseq, dts) = (seq.wrapping_sub(ls), ts.wrapping_sub(lt));
        if dseq == 1 && self.step == 0 {
            self.step = dts;
        }
        if dseq == 1 && dts == self.step {
            return None;
        }
        let kind = if self.step == 0 {
            return None;
        } else if dseq > 1 && dseq < 1000 && dts == self.step * dseq as u32 {
            self.missing += dseq as u64 - 1;
            "missing"
        } else if dseq > 1 && dseq < 1000 && dts == self.step {
            self.seq_only += dseq as u64 - 1;
            "seq-only"
        } else if dseq == 1 {
            self.ts_jump += 1;
            "ts-jump"
        } else {
            self.other += 1;
            "other"
        };
        Some(format!("{kind} seq {ls}->{seq} ts {lt}->{ts} (step {})", self.step))
    }
}

fn relay(iface: &str, from: std::net::Ipv4Addr, to: std::net::Ipv4Addr, count: u32, seconds: f64) -> anyhow::Result<()> {
    let (fd, ifindex) = packet_socket(iface)?;
    let tx = std::net::UdpSocket::bind("0.0.0.0:0")?;
    tx.set_multicast_ttl_v4(0)?;
    tx.set_multicast_loop_v4(true)?;
    let mreq = libc::ip_mreqn { imr_multiaddr: libc::in_addr { s_addr: 0 }, imr_address: libc::in_addr { s_addr: 0 }, imr_ifindex: ifindex as i32 };
    let rc = unsafe {
        use std::os::fd::AsRawFd;
        libc::setsockopt(tx.as_raw_fd(), libc::IPPROTO_IP, libc::IP_MULTICAST_IF, &mreq as *const _ as *const libc::c_void, std::mem::size_of::<libc::ip_mreqn>() as u32)
    };
    anyhow::ensure!(rc == 0, "IP_MULTICAST_IF {iface}: {}", std::io::Error::last_os_error());
    let (base, dest) = (u32::from(from), u32::from(to));
    let deadline = (seconds > 0.0).then(|| std::time::Instant::now() + Duration::from_secs_f64(seconds));
    let mut streams: Vec<Continuity> = (0..count).map(|_| Continuity::default()).collect();
    let mut last_ch0: Vec<Option<i32>> = vec![None; count as usize];
    let (mut errors, mut drops) = (0u64, 0u64);
    let mut last_report = std::time::Instant::now();
    let mut buf = vec![0u8; 9000];
    eprintln!("relay: {from}+0..{count} -> {to}+0..{count} on {iface} (TTL 0, loopback)");
    let report = |streams: &[Continuity], errors: u64, drops: u64, what: &str| {
        let sum = |f: fn(&Continuity) -> u64| streams.iter().map(f).sum::<u64>();
        eprintln!(
            "relay {what}: {{\"t\":{},\"packets\":{},\"missing\":{},\"seq_only\":{},\"ts_jump\":{},\"other\":{},\"send_errors\":{errors},\"socket_drops\":{drops},\"per_stream_missing\":{:?}}}",
            tai_ns() / 1_000_000_000,
            sum(|c| c.packets), sum(|c| c.missing), sum(|c| c.seq_only), sum(|c| c.ts_jump), sum(|c| c.other),
            streams.iter().map(|c| c.missing).collect::<Vec<_>>()
        );
    };
    while deadline.map_or(true, |d| std::time::Instant::now() < d) {
        let mut sll: libc::sockaddr_ll = unsafe { std::mem::zeroed() };
        let mut len = std::mem::size_of::<libc::sockaddr_ll>() as u32;
        let n = unsafe { libc::recvfrom(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len(), 0, &mut sll as *mut _ as *mut libc::sockaddr, &mut len) };
        if n > 0 && sll.sll_pkttype == libc::PACKET_OUTGOING as u8 {
            if let Some((dst, port, payload)) = udp_of(&buf[..n as usize]) {
                let i = u32::from_be_bytes(dst).wrapping_sub(base);
                if i < count && payload.len() >= 12 {
                    let p = &buf[payload.clone()];
                    let seq = u16::from_be_bytes([p[2], p[3]]);
                    let ts = u32::from_be_bytes([p[4], p[5], p[6], p[7]]);
                    let body = &p[12..];
                    if let Some(what) = streams[i as usize].packet(seq, ts) {
                        // The audio either side (L24, channel 0): whether the samples skip too or
                        // only the timestamp does can then be checked against a known pattern.
                        let first = body.get(..3).map(|b| i32::from_be_bytes([b[0], b[1], b[2], 0]) >> 8);
                        eprintln!(
                            "relay: stream {i} {what} at TAI {} | ch0 last before {:?}, first after {:?}",
                            tai_ns() / 1_000_000,
                            last_ch0[i as usize],
                            first
                        );
                    }
                    // Channel 0 of the last frame (8-channel L24 streams).
                    last_ch0[i as usize] = body.len().checked_sub(24).map(|o| i32::from_be_bytes([body[o], body[o + 1], body[o + 2], 0]) >> 8);
                    let target = std::net::SocketAddrV4::new(std::net::Ipv4Addr::from(dest + i), port);
                    if tx.send_to(p, target).is_err() {
                        errors += 1;
                    }
                }
            }
        }
        if last_report.elapsed() >= Duration::from_secs(60) {
            drops += socket_drops(fd);
            report(&streams, errors, drops, "totals");
            last_report = std::time::Instant::now();
        }
    }
    drops += socket_drops(fd);
    unsafe { libc::close(fd) };
    report(&streams, errors, drops, "done");
    Ok(())
}

/// Packets the kernel dropped for this AF_PACKET socket since the last call (it resets them).
fn socket_drops(fd: i32) -> u64 {
    #[repr(C)]
    struct TpacketStats {
        tp_packets: u32,
        tp_drops: u32,
    }
    let mut st = TpacketStats { tp_packets: 0, tp_drops: 0 };
    let mut len = std::mem::size_of::<TpacketStats>() as u32;
    const PACKET_STATISTICS: i32 = 6;
    unsafe { libc::getsockopt(fd, libc::SOL_PACKET, PACKET_STATISTICS, &mut st as *mut _ as *mut libc::c_void, &mut len) };
    st.tp_drops as u64
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

    #[test]
    fn continuity_classifies_gaps() {
        let mut c = Continuity::default();
        assert!(c.packet(10, 1000).is_none());
        assert!(c.packet(11, 1048).is_none());
        assert!(c.packet(14, 1192).unwrap().starts_with("missing"));
        assert!(c.packet(16, 1240).unwrap().starts_with("seq-only"));
        assert!(c.packet(17, 1300).unwrap().starts_with("ts-jump"));
        assert!(c.packet(17, 1348).unwrap().starts_with("other"));
        assert!(c.packet(u16::MAX, 1396).is_some());
        assert!(c.packet(0, 1444).is_none(), "sequence wraps");
        assert_eq!((c.missing, c.seq_only, c.ts_jump, c.other), (2, 1, 1, 2));
    }

    /// Pinned values: contract/audiotest.py asserts the same ones.
    #[test]
    fn pattern_reference_values() {
        assert_eq!(pattern(1, 0, 0), 139_589);
        assert_eq!(pattern(1, 7, 123_456_789), -175_737);
    }
}
