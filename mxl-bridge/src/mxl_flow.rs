use crate::config::Config;

// Names and ids follow `mxl-<host>-<domain>-<app>-<resource>` (GBA-TAB/mxl docs/Naming.md): unique across
// hosts running the same apps on one registry, stable across restarts. host = MXL_HOST_NICKNAME, domain = MXL_DOMAIN_NICKNAME,
// app = MXL_APP_NAME (the orchestrator's instance name; default "bridge"). Resources: `rx<nn>` for
// daemon Sink nn's MXL source/flow/sender, `tx<nn>` for daemon Source nn's receiver,
// `packedrx-<name>` / `packedtx-<name>` for IS-08 packed flows.
use mxl::naming::{Kind, Naming};

static NAMING: std::sync::OnceLock<Naming> = std::sync::OnceLock::new();

/// This process's naming context (read from the environment once).
pub fn naming() -> &'static Naming {
    NAMING.get_or_init(|| Naming::from_env("bridge"))
}

pub fn sink_resource(daemon_id: u8) -> String {
    format!("rx{daemon_id:02}")
}
pub fn source_resource(daemon_id: u8) -> String {
    format!("tx{daemon_id:02}")
}
pub fn packed_rx_resource(name: &str) -> String {
    format!("packedrx-{name}")
}
pub fn packed_tx_resource(name: &str) -> String {
    format!("packedtx-{name}")
}

// Single source of truth for the ids that must agree between the MXL flow itself and the NMOS
// resources describing it (nmos/resources.rs and nmos/state.rs both call these).
pub fn node_id() -> uuid::Uuid {
    naming().app_id(Kind::Node)
}
pub fn device_id() -> uuid::Uuid {
    naming().app_id(Kind::Device)
}
// One Source/Flow/Sender mirrors each daemon Sink, one Receiver mirrors each daemon Source (see
// nmos/state.rs) - keyed by the daemon's own small-integer id, discovered at runtime.
pub fn sink_source_id(daemon_id: u8) -> uuid::Uuid {
    naming().id(&sink_resource(daemon_id), Kind::Source)
}
pub fn sink_flow_id(daemon_id: u8) -> uuid::Uuid {
    naming().id(&sink_resource(daemon_id), Kind::Flow)
}
pub fn sink_sender_id(daemon_id: u8) -> uuid::Uuid {
    naming().id(&sink_resource(daemon_id), Kind::Sender)
}
pub fn source_receiver_id(daemon_id: u8) -> uuid::Uuid {
    naming().id(&source_resource(daemon_id), Kind::Receiver)
}
// Packed flows (nmos/is08.rs): identified by a controller-chosen `name`. No NMOS Sender/Receiver
// mirrors these, so an app that writes into (or reads from) them computes the id itself from the
// same name - on the same host, with this bridge's app name (docs/Naming.md).
pub fn packed_rx_flow_id(name: &str) -> uuid::Uuid {
    naming().id(&packed_rx_resource(name), Kind::Flow)
}
pub fn packed_rx_source_id(name: &str) -> uuid::Uuid {
    naming().id(&packed_rx_resource(name), Kind::Source)
}
pub fn packed_tx_flow_id(name: &str) -> uuid::Uuid {
    naming().id(&packed_tx_resource(name), Kind::Flow)
}

/// Builds the flow_def JSON passed to `mxlCreateFlowWriter`. This *is* an NMOS Flow resource JSON
/// (confirmed against MXL's own examples/flow-configs/flow-audio.json) — audio/float32 is MXL's only
/// supported audio sample format (docs/Architecture.md:341), fixed regardless of the AES67 network
/// codec's bit depth (that's a separate, source-side concern handled in alsa_capture.rs's int32->f32
/// conversion). `label`/`channel_count` are the caller's own (a per-Sink/Source mirror's, or a packed
/// flow's) — not read from `Config`, since Phase 2 has many independently-sized flows, not one.
pub fn build_audio_flow_def(
    cfg: &Config,
    flow_id: uuid::Uuid,
    source_id: uuid::Uuid,
    device_id: uuid::Uuid,
    label: &str,
    channel_count: u32,
) -> String {
    let def = serde_json::json!({
        "id": flow_id.to_string(),
        "device_id": device_id.to_string(),
        "source_id": source_id.to_string(),
        "label": label,
        "description": format!("{label} (bridged from AES67 via mxl-bridge)"),
        "format": "urn:x-nmos:format:audio",
        "media_type": "audio/float32",
        "sample_rate": { "numerator": cfg.sample_rate, "denominator": 1 },
        "channel_count": channel_count,
        "bit_depth": 32,
        "parents": [],
    });
    // A complete IS-04 Flow (`version`) grouped `<instance>:<role>` (the media function contract
    // of mxl-k8s-operator, C-ID-4/5/7). MXL's FlowParser takes the grouphint as
    // `<group>:<role>[:device|node]` with no ':' inside a part (the old fixed `device:mxl-bridge`
    // came from misreading that); `grouphint` replaces any ':' (a packed flow's `packed-rx:mix1`).
    // The role is the flow's resource: its label is `<app>-<resource>` (naming), else the label.
    let mut def = def;
    let instance = naming().app_name();
    let role = label.strip_prefix(&format!("{instance}-")).unwrap_or(label);
    mxl_function::identity::complete_flow_def(&mut def, &instance, role, None, None);
    def.to_string()
}

/// How far `next_index` may diverge from `MxlInstance::get_current_index` (the real,
/// clock-derived "correct" index for right now) before `write_next`/`read_next` snap-correct to
/// it instead of continuing to accumulate. 5ms at 48kHz - roughly half a period at this project's
/// own configured `period_frames`/`sample_rate` (480/48000 = 10ms) - loose enough that ordinary
/// per-period scheduling jitter never triggers a correction, tight enough to bound real drift to
/// a small, likely-inaudible jump rather than letting it grow without limit. See `write_next`'s
/// own doc comment for why this exists at all: found live, real ALSA hardware, growing without
/// bound (2.5s and climbing within 15 real seconds) - `next_index` is seeded once from the real
/// clock and then purely accumulates every period, so it silently diverges from the real clock at
/// whatever rate the local ALSA device's own hardware clock actually runs relative to the CLOCK_TAI
/// (or PTP, if genuinely locked) that `get_current_index` is really derived from - never
/// re-verified otherwise. `gst-mxl-rs`'s own mxlsink (a sibling MXL writer, `render_continuous.rs`)
/// never accumulates an index at all - it derives one fresh from each buffer's own real timestamp
/// every single time, structurally immune to this - not directly portable here (this has no
/// GStreamer buffer/PTS to anchor to, just a raw ALSA period), so this is the same "never trust an
/// un-reverified accumulated value" principle adapted to a real-time polling loop instead.
const DRIFT_TOLERANCE_SAMPLES_AT_48K: u64 = 240; // 5ms @ 48kHz; scaled by sample_rate at use sites.

fn drift_tolerance_samples(sample_rate: &mxl::Rational) -> u64 {
    (DRIFT_TOLERANCE_SAMPLES_AT_48K * sample_rate.numerator as u64) / (48_000 * sample_rate.denominator as u64).max(1)
}

/// Owns the MXL instance and the samples writer for one continuous (audio) flow, plus its own
/// running write index (Phase 2: each Sink's flow is created/destroyed independently as leases
/// come and go, §1 — so unlike Phase 1's one global index, every flow now tracks its own).
pub struct MxlAudioFlow {
    instance: mxl::MxlInstance,
    writer: mxl::SamplesWriter,
    channels: usize,
    sample_rate: mxl::Rational,
    next_index: Option<u64>,
}

impl MxlAudioFlow {
    #[allow(clippy::too_many_arguments)]
    pub fn create(
        cfg: &Config,
        mxl_so_path: &std::path::Path,
        flow_id: uuid::Uuid,
        source_id: uuid::Uuid,
        device_id: uuid::Uuid,
        label: &str,
        channel_count: u32,
    ) -> anyhow::Result<Self> {
        let api = mxl::load_api(mxl_so_path)
            .map_err(|e| anyhow::anyhow!("mxl::load_api({mxl_so_path:?}) failed: {e:?}"))?;
        let instance = mxl::MxlInstance::new(api, &cfg.mxl_domain, "")
            .map_err(|e| anyhow::anyhow!("MxlInstance::new({}) failed: {e:?}", cfg.mxl_domain))?;

        let flow_def = build_audio_flow_def(cfg, flow_id, source_id, device_id, label, channel_count);

        let (writer, info, was_created) = instance
            .create_flow_writer(&flow_def, None)
            .map_err(|e| anyhow::anyhow!("create_flow_writer failed: {e:?}"))?;
        if !was_created {
            tracing::warn!(%flow_id, "reusing pre-existing MXL flow (was not newly created)");
        }
        let channels = info
            .continuous()
            .map_err(|e| anyhow::anyhow!("flow is not a continuous (audio) flow: {e:?}"))?
            .channelCount as usize;
        if channels != channel_count as usize {
            anyhow::bail!("MXL flow channel_count ({channels}) does not match expected ({channel_count})");
        }

        let writer = writer
            .to_samples_writer()
            .map_err(|e| anyhow::anyhow!("to_samples_writer failed: {e:?}"))?;

        let sample_rate = mxl::Rational { numerator: cfg.sample_rate as i64, denominator: 1 };
        Ok(Self { instance, writer, channels, sample_rate, next_index: None })
    }

    /// Writes one period of planar float32 samples (one Vec per channel, all the same length),
    /// continuing this flow's own monotonic index from wherever the previous call left off —
    /// seeded from MXL's own current-time-based index on the very first call (it reads the same
    /// ptp-clock-manager-disciplined system clock internally per mxl/docs/Timing.md's "index 0 =
    /// SMPTE 2059-1 epoch" model, no need to duplicate that computation here). Mirrors the
    /// write_samples loop in mxl's own flow-writer.rs example, generalized to per-flow state -
    /// except every call also re-checks the accumulated index against a fresh real one
    /// (`get_current_index`) and snaps to it past `drift_tolerance_samples` (see that function's
    /// own doc comment for why this exists - found live, real ALSA hardware, unbounded growing
    /// latency): the local ALSA device's own hardware clock isn't guaranteed to run at exactly the
    /// same rate as whatever real/PTP clock `get_current_index` is really derived from, and pure
    /// accumulation never re-verifies that assumption.
    ///
    /// `pending_frames`: frames still waiting in the ALSA capture buffer right after this block
    /// was read - i.e. how much newer than this block's last sample "now" is. The block is stamped
    /// so it ENDS there (`capture_start_index`): it was captured in the past, not starting now.
    /// Stamping its first sample with the current index (as before 2026-09-25) labelled every block
    /// up to one period in the future (measured: flow latency -0.1..-7.4 ms against TAI).
    pub fn write_next(&mut self, planar: &[Vec<f32>], pending_frames: u64) -> anyhow::Result<()> {
        let count = planar.first().map(|c| c.len()).unwrap_or(0);
        if count == 0 {
            return Ok(());
        }
        let real_index = capture_start_index(self.instance.get_current_index(&self.sample_rate), pending_frames, count);
        let index = match self.next_index {
            Some(i) if i.abs_diff(real_index) <= drift_tolerance_samples(&self.sample_rate) => i,
            Some(i) => {
                tracing::warn!(
                    accumulated_index = i,
                    real_index,
                    drift_samples = i.abs_diff(real_index),
                    "MXL write index drifted from the real clock beyond tolerance, snapping to it"
                );
                real_index
            }
            None => real_index,
        };

        tracing::debug!(index, count, "write_next");
        self.write_at(index, planar)
    }

    /// The flow's current (TAI-derived) sample index.
    pub fn current_index(&self) -> u64 {
        self.instance.get_current_index(&self.sample_rate)
    }

    /// Writes `planar` so its first sample lands exactly at `index` (the test generator needs to
    /// know the index of every sample; `write_next` chooses its own).
    pub fn write_at(&mut self, index: u64, planar: &[Vec<f32>]) -> anyhow::Result<()> {
        let count = planar.first().map(|c| c.len()).unwrap_or(0);
        if count == 0 {
            return Ok(());
        }
        let mut access = self
            .writer
            .open_samples(index + count as u64 - 1, count)
            .map_err(|e| anyhow::anyhow!("open_samples failed: {e:?}"))?;

        for ch in 0..self.channels.min(planar.len()) {
            let (dst1, dst2) = access
                .channel_data_mut(ch)
                .map_err(|e| anyhow::anyhow!("channel_data_mut({ch}) failed: {e:?}"))?;
            let src = &planar[ch];
            let src_bytes: &[u8] = bytemuck_cast_f32_slice(src);
            let (b1, b2) = src_bytes.split_at(dst1.len().min(src_bytes.len()));
            dst1[..b1.len()].copy_from_slice(b1);
            if !b2.is_empty() {
                dst2[..b2.len()].copy_from_slice(b2);
            }
        }

        access.commit().map_err(|e| anyhow::anyhow!("commit failed: {e:?}"))?;
        self.next_index = Some(index + count as u64);
        Ok(())
    }
}

/// Index of the FIRST sample of a just-read capture block of `count` frames, given the current
/// index (`now`) and the frames still pending in the capture buffer (newer than the block).
pub fn capture_start_index(now: u64, pending_frames: u64, count: usize) -> u64 {
    now.saturating_sub(pending_frames + count as u64)
}

/// Reinterprets an f32 slice as raw little-endian bytes (matches MXL's `audio/float32` on-disk
/// layout — IEEE 754, host byte order on x86_64 is already little-endian).
fn bytemuck_cast_f32_slice(src: &[f32]) -> &[u8] {
    // SAFETY: f32 has no padding and any bit pattern is valid; the resulting slice's lifetime and
    // length are derived correctly from the source.
    unsafe { std::slice::from_raw_parts(src.as_ptr() as *const u8, std::mem::size_of_val(src)) }
}

/// Reads an existing MXL audio flow (TX direction — some other producer, possibly this same
/// process's own MxlAudioFlow, or a genuinely separate MXL app, writes it; we consume and play it
/// out over ALSA). Which flow_id to open is an IS-05 activation concern (not yet wired up — see
/// README), passed in directly for now.
pub struct MxlAudioFlowSource {
    /// None between releasing a stale reader and opening the flow again.
    reader: Option<mxl::SamplesReader>,
    channels: usize,
    next_index: Option<u64>,
    /// The flow this reads, to open it again when its writer was replaced.
    flow_id: String,
    /// The head last seen and since when it has not moved: a head that stands still while time
    /// passes is a writer that stopped, or a flow that was created anew under the same id (a
    /// restarted writer) while this reader still maps the old one. Read on, it would replay the
    /// same block forever (2026-10-02: three music streams frozen after their players restarted).
    last_head: u64,
    head_since: std::time::Instant,
    /// The stand-still was reported (once, until the head moves again).
    stale_reported: bool,
    /// Opened again since the caller last asked ([`Self::take_reopened`]).
    reopened: bool,
    // for `current_index`: MXL's own time (the media clock when MXL_MEDIA_CLOCK is set), never
    // CLOCK_TAI read directly - that would disagree with every flow's index on a media clock
    instance: mxl::MxlInstance,
    sample_rate: mxl::Rational,
}

impl MxlAudioFlowSource {
    /// `expected_channels` is the caller's own — the activating mirror's (a Source's, or a packed
    /// flow's) known channel count, not read from `Config` (Phase 2 has many independently-sized
    /// flows, not one).
    pub fn open(cfg: &Config, mxl_so_path: &std::path::Path, flow_id: &str, expected_channels: usize) -> anyhow::Result<Self> {
        let api = mxl::load_api(mxl_so_path)
            .map_err(|e| anyhow::anyhow!("mxl::load_api({mxl_so_path:?}) failed: {e:?}"))?;
        let instance = mxl::MxlInstance::new(api, &cfg.mxl_domain, "")
            .map_err(|e| anyhow::anyhow!("MxlInstance::new({}) failed: {e:?}", cfg.mxl_domain))?;

        let reader = instance
            .create_flow_reader(flow_id)
            .map_err(|e| anyhow::anyhow!("create_flow_reader({flow_id}) failed: {e:?}"))?;
        let info = reader
            .get_info()
            .map_err(|e| anyhow::anyhow!("get_info failed: {e:?}"))?
            .config;
        let channels = info
            .continuous()
            .map_err(|e| anyhow::anyhow!("flow {flow_id} is not a continuous (audio) flow: {e:?}"))?
            .channelCount as usize;
        if channels != expected_channels {
            anyhow::bail!("MXL flow channel_count ({channels}) does not match expected ({expected_channels})");
        }

        let reader = reader
            .to_samples_reader()
            .map_err(|e| anyhow::anyhow!("to_samples_reader failed: {e:?}"))?;

        let sample_rate = mxl::Rational { numerator: cfg.sample_rate as i64, denominator: 1 };
        Ok(Self {
            reader: Some(reader),
            channels,
            next_index: None,
            flow_id: flow_id.to_string(),
            last_head: 0,
            head_since: std::time::Instant::now(),
            stale_reported: false,
            reopened: false,
            instance,
            sample_rate,
        })
    }

    /// "Now" as an index of this flow's rate, on MXL's clock (see the field note).
    pub fn current_index(&self) -> u64 {
        self.instance.get_current_index(&self.sample_rate)
    }

    /// Current write head of the flow — the sensible starting point for a fresh reader (matches
    /// mxl's own flow-reader.rs example), rather than "now" per wall clock, since the writer may be
    /// behind that.
    pub fn head_index(&self) -> anyhow::Result<u64> {
        Ok(self
            .rd()?
            .get_runtime_info()
            .map_err(|e| anyhow::anyhow!("get_runtime_info failed: {e:?}"))?
            .headIndex)
    }

    /// Resets this reader's own tracked index to the flow's current head — call after `read_next`
    /// returns an error (most likely cause: the tracked index drifted out of the writer's valid
    /// ring-buffer window, e.g. a stall let the writer lap the reader).
    /// Reads the `count` samples right after the previous read while that stays within `tolerance`
    /// of the target - `delay` behind the older of `now` (TAI index) and this flow's head - and
    /// jumps to the target otherwise (first read, a stall, real clock drift). Fast writers are read
    /// `delay` behind real time; writers that lag get their own lag plus `delay`, instead of reads
    /// landing before their data exists. The tolerance must exceed the caller's wake-up jitter and
    /// the writer's block size, or it snaps (skipping or repeating samples) on noise.
    pub fn read_aligned(&mut self, count: usize, now: u64, delay: u64, tolerance: u64, timeout: std::time::Duration) -> anyhow::Result<Vec<Vec<f32>>> {
        // A writer keeping up (head within `delay` of now) is read exactly `delay` behind now:
        // deterministic. Only one lagging further is read `delay` behind its own head.
        let head = self.live_head()?;
        let target_end = if head + delay >= now { now.saturating_sub(delay) } else { head.saturating_sub(delay) };
        let end = match self.next_index {
            Some(i) if i.abs_diff(target_end) <= tolerance => i,
            _ => target_end,
        };
        let planar = self.read_samples_at(end, count, timeout)?;
        self.next_index = Some(end + count as u64);
        Ok(planar)
    }

    /// The flow's head, checked for a writer that stopped or was replaced: a head that has not
    /// moved for `STALE_HEAD` makes this reader open the flow again (a replaced flow then reads
    /// from its new writer); while it still does not move, reads fail (silence) instead of
    /// replaying the last block.
    fn live_head(&mut self) -> anyhow::Result<u64> {
        const STALE_HEAD: std::time::Duration = std::time::Duration::from_millis(500);
        if self.reader.is_some() {
            let head = self.head_index()?;
            if head != self.last_head {
                self.last_head = head;
                self.head_since = std::time::Instant::now();
                self.stale_reported = false;
                return Ok(head);
            }
            if self.head_since.elapsed() < STALE_HEAD {
                return Ok(head);
            }
            // Stood still. The instance hands back the reader it already holds for this flow id
            // (still mapping the replaced flow): release it first, then open the flow again.
            self.reader = None;
            self.next_index = None;
            self.head_since = std::time::Instant::now();
            if !self.stale_reported {
                self.stale_reported = true;
                tracing::warn!(flow = %self.flow_id, head, "flow head stood still: released the reader, opening the flow again");
            }
        } else if self.head_since.elapsed() < STALE_HEAD {
            anyhow::bail!("flow {} has no writer", self.flow_id);
        }
        self.head_since = std::time::Instant::now();
        let reader = self
            .instance
            .create_flow_reader(&self.flow_id)
            .map_err(|e| anyhow::anyhow!("flow {} cannot be opened: {e:?}", self.flow_id))?
            .to_samples_reader()
            .map_err(|e| anyhow::anyhow!("to_samples_reader failed: {e:?}"))?;
        let head = reader
            .get_runtime_info()
            .map_err(|e| anyhow::anyhow!("get_runtime_info failed: {e:?}"))?
            .headIndex;
        self.reader = Some(reader);
        if head == self.last_head {
            // still the same head: no writer yet (silence, retried every STALE_HEAD), no replay
            anyhow::bail!("flow {} has no writer (head stands still)", self.flow_id);
        }
        tracing::info!(flow = %self.flow_id, head, "flow opened again: reading its new writer");
        self.last_head = head;
        self.stale_reported = false;
        self.reopened = true;
        Ok(head)
    }

    /// Whether the flow was opened again since the last call: the caller then starts over with its
    /// base read delay (the failed reads in between raised it for no lateness of the writer).
    pub fn take_reopened(&mut self) -> bool {
        std::mem::take(&mut self.reopened)
    }

    fn rd(&self) -> anyhow::Result<&mxl::SamplesReader> {
        self.reader.as_ref().ok_or_else(|| anyhow::anyhow!("flow {} not open", self.flow_id))
    }

    /// Forgets the read position: the next `read_aligned` starts at its target again.
    pub fn realign(&mut self) {
        self.next_index = None;
    }

    pub fn resync_to_head(&mut self) -> anyhow::Result<()> {
        self.next_index = Some(self.head_index()?);
        Ok(())
    }


    /// Blocking read of `count` samples ending at `index` (same end-of-batch indexing convention as
    /// write_next). Returns owned planar float32 data, one Vec<f32> per channel.
    /// Reads the `count` samples ENDING at `end_index` (MXL's reader index names the last sample
    /// of the range: the result covers `end_index - count + 1 ..= end_index`).
    pub fn read_samples_at(
        &self,
        index: u64,
        count: usize,
        timeout: std::time::Duration,
    ) -> anyhow::Result<Vec<Vec<f32>>> {
        let data = self
            .rd()?
            .get_samples(index, count, timeout)
            .map_err(|e| anyhow::anyhow!("get_samples failed: {e:?}"))?;

        let mut planar = Vec::with_capacity(self.channels);
        for ch in 0..self.channels {
            let (b1, b2) = data
                .channel_data(ch)
                .map_err(|e| anyhow::anyhow!("channel_data({ch}) failed: {e:?}"))?;
            let mut bytes = Vec::with_capacity(b1.len() + b2.len());
            bytes.extend_from_slice(b1);
            bytes.extend_from_slice(b2);
            // bytes is exactly count * 4 (f32) bytes per channel by construction (get_samples
            // returns `count` samples' worth of data per fragment pair).
            let samples: Vec<f32> = bytes
                .chunks_exact(4)
                .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                .collect();
            planar.push(samples);
        }
        Ok(planar)
    }
}

#[cfg(test)]
mod capture_index_tests {
    use super::capture_start_index;

    #[test]
    fn a_block_ends_where_the_pending_frames_begin() {
        // 480 frames just read, 96 more already waiting: the block covers now-576 .. now-97.
        let start = capture_start_index(1_000_000, 96, 480);
        assert_eq!(start, 1_000_000 - 576);
        assert_eq!(start + 480 - 1, 1_000_000 - 96 - 1, "last sample is just before the pending ones");
        // Nothing pending: the last sample is the one just before now - never in the future.
        assert_eq!(capture_start_index(1_000_000, 0, 480) + 480, 1_000_000);
        assert_eq!(capture_start_index(100, 96, 480), 0, "saturates at the epoch");
    }
}
