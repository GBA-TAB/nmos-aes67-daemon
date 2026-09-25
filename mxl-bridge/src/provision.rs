//! Fixed stream capacity, declared in the config and enforced on the daemon ("capacity" block).
//!
//! The bridge owns every daemon stream: `tx` = daemon Sources (MXL -> ST 2110-30, the bridge's
//! MXL Receivers), `rx` = daemon Sinks (ST 2110-30 -> MXL, the bridge's MXL Senders). Stream n of a
//! side uses a contiguous block of ALSA channels right after stream n-1's.
//!
//! A daemon Sink cannot exist without an SDP, so an unconnected rx stream is *parked*: an SDP on an
//! unused multicast group with the stream's channel count. Connecting it (NMOS IS-05 on the
//! daemon's own node) replaces that SDP.
//!
//! Reconcile rules keep live connections intact:
//! - tx: created when missing; rewritten only when its map, address or codec differ (a Source
//!   rewrite re-announces its SDP);
//! - rx: created parked when missing; a map-only difference is fixed keeping the Sink's SDP (its
//!   connection survives); replaced by a parked Sink only when the channel count differs;
//! - streams with ids beyond the declared count are removed.
//!
//! `plan` is pure (unit-tested); `reconcile` fetches the daemon state, logs the plan and - in
//! `apply` mode - executes it.

use serde::Deserialize;

use crate::daemon_client::{DaemonSink, DaemonSource};

/// The daemon's own ceiling on stream ids (SessionManager::stream_id_max + 1).
pub const MAX_STREAMS: usize = 64;

/// Default daemon playout delay for rx Sinks, in samples (12 ms at 48 kHz).
pub const DEFAULT_RX_DELAY: u32 = 576;

#[derive(Deserialize, Clone, Debug, PartialEq)]
#[serde(untagged)]
pub enum ChannelSpec {
    /// Every stream this many channels (needs `streams`).
    Each(u32),
    /// One entry per stream.
    List(Vec<u32>),
}

#[derive(Deserialize, Clone, Copy, Debug, PartialEq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Execute the plan.
    #[default]
    Apply,
    /// Only log it.
    Log,
}

#[derive(Deserialize, Clone, Debug)]
pub struct StreamSet {
    #[serde(default)]
    pub streams: Option<u32>,
    pub channels: ChannelSpec,
    /// tx only: stream n sends to this address + n (e.g. "239.55.5.5").
    #[serde(default)]
    pub multicast_base: Option<String>,
    /// rx only: where unconnected streams are parked (e.g. "239.255.255.1").
    #[serde(default)]
    pub parking_group: Option<String>,
    /// rx only: the daemon's playout delay per Sink, in samples (its jitter buffer: added to the
    /// rx latency as is). Applied to live Sinks too, keeping their connection. Default 576 (12 ms).
    #[serde(default)]
    pub delay: Option<u32>,
}

#[derive(Deserialize, Clone, Debug)]
pub struct Capacity {
    /// ALSA width the daemon must run at (its own `alsa_channels`) - checked, not set.
    pub alsa_channels: u32,
    pub tx: StreamSet,
    pub rx: StreamSet,
    #[serde(default)]
    pub mode: Mode,
    /// How often the layout is re-checked after startup (a parked stream the daemon dropped comes
    /// back); 0 = startup only.
    #[serde(default = "default_recheck_secs")]
    pub recheck_secs: u64,
}

fn default_recheck_secs() -> u64 {
    30
}

/// Channel count of each stream, from `streams` + `channels`.
pub fn stream_sizes(set: &StreamSet) -> anyhow::Result<Vec<u32>> {
    let sizes = match (&set.channels, set.streams) {
        (ChannelSpec::Each(ch), Some(n)) => vec![*ch; n as usize],
        (ChannelSpec::Each(_), None) => anyhow::bail!("'channels' as a number needs 'streams'"),
        (ChannelSpec::List(list), Some(n)) if list.len() != n as usize => {
            anyhow::bail!("'streams' is {n} but 'channels' lists {}", list.len())
        }
        (ChannelSpec::List(list), _) => list.clone(),
    };
    anyhow::ensure!(sizes.len() <= MAX_STREAMS, "{} streams exceed the daemon's {MAX_STREAMS}", sizes.len());
    anyhow::ensure!(sizes.iter().all(|c| (1..=64).contains(c)), "every stream needs 1..=64 channels");
    Ok(sizes)
}

/// Contiguous ALSA channel maps: stream 0 gets 0..c0, stream 1 the next c1, ...
pub fn channel_maps(sizes: &[u32], alsa_channels: u32) -> anyhow::Result<Vec<Vec<u8>>> {
    let total: u32 = sizes.iter().sum();
    anyhow::ensure!(total <= alsa_channels, "{total} channels do not fit in alsa_channels {alsa_channels}");
    anyhow::ensure!(alsa_channels <= 256, "alsa_channels above 256");
    let mut next = 0u32;
    Ok(sizes
        .iter()
        .map(|&c| {
            let map = (next..next + c).map(|ch| ch as u8).collect();
            next += c;
            map
        })
        .collect())
}

fn ipv4_plus(base: &str, n: u32) -> anyhow::Result<String> {
    let ip: std::net::Ipv4Addr = base.parse().map_err(|_| anyhow::anyhow!("'{base}' is not an IPv4 address"))?;
    Ok(std::net::Ipv4Addr::from(u32::from(ip) + n).to_string())
}

pub fn desired_source(id: u8, map: Vec<u8>, address: String) -> DaemonSource {
    DaemonSource {
        id,
        enabled: true,
        name: format!("Bridge TX {}", id as u32 + 1),
        io: "Audio Device".into(),
        max_samples_per_packet: 48,
        codec: "L24".into(),
        address,
        ttl: 15,
        payload_type: 98,
        dscp: 56,
        refclk_ptp_traceable: false,
        map,
    }
}

/// An SDP the daemon accepts for a Sink that receives nothing yet: `channels` of L24/48k on an
/// unused group. Marked with the session name so parked Sinks are recognisable.
pub fn parking_sdp(id: u8, channels: usize, group: &str) -> String {
    format!(
        "v=0\r\no=- 0 0 IN IP4 0.0.0.0\r\ns={PARKED_PREFIX} {}\r\nc=IN IP4 {group}/15\r\nt=0 0\r\n\
         m=audio 5004 RTP/AVP 98\r\nc=IN IP4 {group}/15\r\na=rtpmap:98 L24/48000/{channels}\r\n\
         a=ptime:1\r\na=mediaclk:direct=0\r\na=recvonly\r\n",
        id as u32 + 1
    )
}

pub const PARKED_PREFIX: &str = "mxl-bridge parked RX";

#[allow(dead_code)] // the plan no longer distinguishes parked Sinks; kept for tests and tooling
pub fn is_parked(sink: &DaemonSink) -> bool {
    sink.sdp.contains(PARKED_PREFIX)
}

pub fn parked_sink(id: u8, map: Vec<u8>, group: &str, delay: u32) -> DaemonSink {
    DaemonSink {
        id,
        name: format!("Bridge RX {}", id as u32 + 1),
        io: "Audio Device".into(),
        use_sdp: true,
        source: String::new(),
        sdp: parking_sdp(id, map.len(), group),
        delay,
        ignore_refclk_gmid: true,
        map,
    }
}

/// Channel count an SDP declares (`a=rtpmap:<pt> <codec>/<rate>/<channels>`).
pub fn sdp_channels(sdp: &str) -> Option<usize> {
    sdp.lines().find_map(|l| l.trim().strip_prefix("a=rtpmap:")).and_then(|v| v.rsplit('/').next()).and_then(|c| c.trim().parse().ok())
}

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    PutSource(DaemonSource),
    DeleteSource(u8),
    /// New or replaced rx stream, parked.
    PutParkedSink(DaemonSink),
    /// Same Sink with a corrected map - its SDP (and connection) kept.
    RemapSink(DaemonSink),
    DeleteSink(u8),
}

impl std::fmt::Display for Action {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let span = |m: &[u8]| m.first().map(|a| format!("ALSA {}-{}", a, m.last().unwrap())).unwrap_or_default();
        match self {
            Action::PutSource(s) => write!(f, "tx {}: set '{}' -> {} {} {}ch", s.id, s.name, s.address, span(&s.map), s.map.len()),
            Action::DeleteSource(id) => write!(f, "tx {id}: remove (beyond declared capacity)"),
            Action::PutParkedSink(s) => write!(f, "rx {}: create parked '{}' {} {}ch", s.id, s.name, span(&s.map), s.map.len()),
            Action::RemapSink(s) => write!(f, "rx {}: set '{}' to {}, delay {} (connection kept)", s.id, s.name, span(&s.map), s.delay),
            Action::DeleteSink(id) => write!(f, "rx {id}: remove (beyond declared capacity)"),
        }
    }
}

/// What it takes to turn the daemon's current streams into the declared capacity.
pub fn plan(cap: &Capacity, sources: &[DaemonSource], sinks: &[DaemonSink]) -> anyhow::Result<Vec<Action>> {
    let tx_maps = channel_maps(&stream_sizes(&cap.tx)?, cap.alsa_channels)?;
    let rx_maps = channel_maps(&stream_sizes(&cap.rx)?, cap.alsa_channels)?;
    let base = cap.tx.multicast_base.as_deref().ok_or_else(|| anyhow::anyhow!("tx.multicast_base is required"))?;
    let group = cap.rx.parking_group.as_deref().ok_or_else(|| anyhow::anyhow!("rx.parking_group is required"))?;
    let delay = cap.rx.delay.unwrap_or(DEFAULT_RX_DELAY);
    let mut actions = Vec::new();

    for (i, map) in tx_maps.into_iter().enumerate() {
        let id = i as u8;
        let want = desired_source(id, map, ipv4_plus(base, i as u32)?);
        match sources.iter().find(|s| s.id == id) {
            Some(cur) if cur.map == want.map && cur.address == want.address && cur.codec == want.codec && cur.enabled => {}
            _ => actions.push(Action::PutSource(want)),
        }
    }
    for s in sources.iter().filter(|s| s.id as usize >= cap_count(&cap.tx)) {
        actions.push(Action::DeleteSource(s.id));
    }

    for (i, map) in rx_maps.into_iter().enumerate() {
        let id = i as u8;
        match sinks.iter().find(|s| s.id == id) {
            None => actions.push(Action::PutParkedSink(parked_sink(id, map, group, delay))),
            Some(cur) if cur.map == map && cur.delay == delay => {}
            Some(cur) if sdp_channels(&cur.sdp) == Some(map.len()) => {
                actions.push(Action::RemapSink(DaemonSink { map, delay, ..cur.clone() }))
            }
            Some(_) => actions.push(Action::PutParkedSink(parked_sink(id, map, group, delay))),
        }
    }
    for s in sinks.iter().filter(|s| s.id as usize >= cap_count(&cap.rx)) {
        actions.push(Action::DeleteSink(s.id));
    }
    Ok(actions)
}

fn cap_count(set: &StreamSet) -> usize {
    stream_sizes(set).map(|s| s.len()).unwrap_or(0)
}

/// Fetches the daemon's streams, plans, logs, and (apply mode) executes. Returns how many actions
/// were planned.
pub async fn reconcile(http: &reqwest::Client, base_url: &str, cap: &Capacity, daemon_alsa_channels: u32) -> anyhow::Result<usize> {
    anyhow::ensure!(
        daemon_alsa_channels >= cap.alsa_channels,
        "capacity needs alsa_channels {} but the daemon runs {} - set \"alsa_channels\": {} in the daemon's config and restart it",
        cap.alsa_channels,
        daemon_alsa_channels,
        cap.alsa_channels
    );
    #[derive(Deserialize)]
    struct Streams {
        sources: Vec<DaemonSource>,
        sinks: Vec<DaemonSink>,
    }
    let cur: Streams = http.get(format!("{base_url}/api/streams")).send().await?.error_for_status()?.json().await?;
    let actions = plan(cap, &cur.sources, &cur.sinks)?;
    if actions.is_empty() {
        tracing::debug!("capacity: daemon streams match the declared layout");
        return Ok(0);
    }
    for a in &actions {
        match cap.mode {
            Mode::Log => tracing::info!("capacity (log mode, not applied): {a}"),
            Mode::Apply => tracing::info!("capacity: {a}"),
        }
    }
    if cap.mode == Mode::Log {
        return Ok(actions.len());
    }
    for a in &actions {
        let res = match a {
            Action::PutSource(s) => http.put(format!("{base_url}/api/source/{}", s.id)).json(s).send().await,
            Action::DeleteSource(id) => http.delete(format!("{base_url}/api/source/{id}")).send().await,
            Action::PutParkedSink(s) | Action::RemapSink(s) => http.put(format!("{base_url}/api/sink/{}", s.id)).json(s).send().await,
            Action::DeleteSink(id) => http.delete(format!("{base_url}/api/sink/{id}")).send().await,
        };
        match res {
            Ok(r) if r.status().is_success() => {}
            Ok(r) => {
                let status = r.status();
                let body = r.text().await.unwrap_or_default();
                tracing::error!("capacity: '{a}' failed: HTTP {status} {body}");
            }
            Err(e) => tracing::error!("capacity: '{a}' failed: {e}"),
        }
    }
    Ok(actions.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cap16x8() -> Capacity {
        Capacity {
            alsa_channels: 128,
            tx: StreamSet { streams: Some(16), channels: ChannelSpec::Each(8), multicast_base: Some("239.55.5.5".into()), parking_group: None, delay: None },
            rx: StreamSet { streams: Some(16), channels: ChannelSpec::Each(8), multicast_base: None, parking_group: Some("239.255.255.1".into()), delay: None },
            mode: Mode::Apply,
            recheck_secs: 30,
        }
    }

    fn sink(id: u8, map: Vec<u8>, channels: usize) -> DaemonSink {
        DaemonSink {
            id,
            name: format!("s{id}"),
            io: "Audio Device".into(),
            use_sdp: true,
            source: String::new(),
            sdp: format!("v=0\r\ns=live\r\nm=audio 5004 RTP/AVP 98\r\na=rtpmap:98 L24/48000/{channels}\r\n"),
            delay: 576,
            ignore_refclk_gmid: true,
            map,
        }
    }

    #[test]
    fn sixteen_by_eight_fills_128_contiguous_channels() {
        let maps = channel_maps(&stream_sizes(&cap16x8().tx).unwrap(), 128).unwrap();
        assert_eq!(maps.len(), 16);
        assert_eq!(maps[0], (0..8).collect::<Vec<u8>>());
        assert_eq!(maps[15], (120..128).collect::<Vec<u8>>());
    }

    #[test]
    fn sizes_reject_what_does_not_fit() {
        assert!(channel_maps(&[8; 17], 128).is_err());
        let bad = StreamSet { streams: Some(3), channels: ChannelSpec::List(vec![8, 8]), multicast_base: None, parking_group: None, delay: None };
        assert!(stream_sizes(&bad).is_err());
        let list = StreamSet { streams: None, channels: ChannelSpec::List(vec![16, 8, 8]), multicast_base: None, parking_group: None, delay: None };
        assert_eq!(channel_maps(&stream_sizes(&list).unwrap(), 64).unwrap()[1], (16..24).collect::<Vec<u8>>());
    }

    #[test]
    fn empty_daemon_gets_every_stream() {
        let a = plan(&cap16x8(), &[], &[]).unwrap();
        assert_eq!(a.iter().filter(|a| matches!(a, Action::PutSource(_))).count(), 16);
        assert_eq!(a.iter().filter(|a| matches!(a, Action::PutParkedSink(_))).count(), 16);
        let Action::PutSource(s) = &a[1] else { panic!() };
        assert_eq!(s.address, "239.55.5.6");
    }

    #[test]
    fn matching_streams_are_left_alone() {
        let cap = cap16x8();
        let sources: Vec<_> = (0..16).map(|i| desired_source(i, (i * 8..i * 8 + 8).collect(), ipv4_plus("239.55.5.5", i as u32).unwrap())).collect();
        let sinks: Vec<_> = (0..16).map(|i| sink(i, (i * 8..i * 8 + 8).collect(), 8)).collect();
        assert!(plan(&cap, &sources, &sinks).unwrap().is_empty());
    }

    #[test]
    fn live_sink_with_wrong_map_is_remapped_keeping_its_sdp() {
        let cur = sink(2, (32..40).collect(), 8);
        let a = plan(&cap16x8(), &[], &[cur.clone()]).unwrap();
        let remap = a.iter().find_map(|a| if let Action::RemapSink(s) = a { Some(s) } else { None }).unwrap();
        assert_eq!(remap.map, (16..24).collect::<Vec<u8>>());
        assert_eq!(remap.sdp, cur.sdp);
    }

    #[test]
    fn sink_with_other_channel_count_is_replaced_parked_and_extras_removed() {
        let a = plan(&cap16x8(), &[], &[sink(3, (16..32).collect(), 16), sink(20, (0..8).collect(), 8)]).unwrap();
        assert!(a.iter().any(|a| matches!(a, Action::PutParkedSink(s) if s.id == 3 && s.map.len() == 8)));
        assert!(a.contains(&Action::DeleteSink(20)));
    }

    #[test]
    fn a_new_delay_is_applied_to_live_sinks_keeping_their_sdp() {
        let mut cap = cap16x8();
        cap.rx.delay = Some(192);
        let cur = sink(0, (0..8).collect(), 8);
        let a = plan(&cap, &[], &[cur.clone()]).unwrap();
        let s = a.iter().find_map(|a| if let Action::RemapSink(s) = a { Some(s) } else { None }).unwrap();
        assert_eq!((s.delay, &s.sdp), (192, &cur.sdp));
    }

    #[test]
    fn parking_sdp_is_recognisable_and_declares_its_channels() {
        let s = parked_sink(4, (32..40).collect(), "239.255.255.1", 576);
        assert!(is_parked(&s));
        assert_eq!(sdp_channels(&s.sdp), Some(8));
        assert!(s.sdp.contains("c=IN IP4 239.255.255.1/15"));
    }
}
