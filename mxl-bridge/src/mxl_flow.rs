use crate::config::Config;

/// Fixed namespace for deriving stable (UUIDv5) resource ids from a human-readable label, mirroring
/// the daemon's `make_resource_uuid` pattern (nmos_manager.cpp) — reproducible across restarts, no
/// persisted state needed.
const ID_NAMESPACE: uuid::Uuid = uuid::Uuid::from_bytes([
    0x6d, 0x78, 0x6c, 0x2d, 0x62, 0x72, 0x69, 0x64, 0x67, 0x65, 0x2d, 0x6e, 0x73, 0x2d, 0x00, 0x00,
]);

pub fn stable_id(name: &str) -> uuid::Uuid {
    uuid::Uuid::new_v5(&ID_NAMESPACE, name.as_bytes())
}

// Single source of truth for the ids that must agree between the MXL flow itself and the NMOS
// resources describing it (nmos/resources.rs and nmos/state.rs both call these, instead of each
// re-deriving the same string format independently and risking drift).
pub fn node_id() -> uuid::Uuid {
    stable_id("mxl-bridge-node")
}
pub fn device_id() -> uuid::Uuid {
    stable_id("mxl-bridge-device")
}

// Phase 2: one Source/Flow/Sender mirrors each daemon Sink, one Receiver mirrors each daemon
// Source (see nmos/state.rs) — keyed by the daemon's own small-integer id rather than a
// config-provided label, since these are discovered at runtime, not fixed at startup.
pub fn sink_source_id(daemon_id: u8) -> uuid::Uuid {
    stable_id(&format!("mxl-bridge-sink-source:{daemon_id}"))
}
pub fn sink_flow_id(daemon_id: u8) -> uuid::Uuid {
    stable_id(&format!("mxl-bridge-sink-flow:{daemon_id}"))
}
pub fn sink_sender_id(daemon_id: u8) -> uuid::Uuid {
    stable_id(&format!("mxl-bridge-sink-sender:{daemon_id}"))
}
pub fn source_receiver_id(daemon_id: u8) -> uuid::Uuid {
    stable_id(&format!("mxl-bridge-source-receiver:{daemon_id}"))
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
    serde_json::json!({
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
        "tags": {
            "urn:x-nmos:tag:grouphint/v1.0": [format!("mxl-bridge:{label}")]
        }
    })
    .to_string()
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
    /// write_samples loop in mxl's own flow-writer.rs example, generalized to per-flow state.
    pub fn write_next(&mut self, planar: &[Vec<f32>]) -> anyhow::Result<()> {
        let count = planar.first().map(|c| c.len()).unwrap_or(0);
        if count == 0 {
            return Ok(());
        }
        let index = match self.next_index {
            Some(i) => i,
            None => self.instance.get_current_index(&self.sample_rate),
        };

        tracing::debug!(index, count, "write_next");
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
    reader: mxl::SamplesReader,
    channels: usize,
    next_index: Option<u64>,
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

        // `instance` isn't stored on Self: `SamplesReader` already keeps its own
        // Arc<InstanceContext> alive internally, so nothing here needs a separate handle to it.
        Ok(Self { reader, channels, next_index: None })
    }

    /// Current write head of the flow — the sensible starting point for a fresh reader (matches
    /// mxl's own flow-reader.rs example), rather than "now" per wall clock, since the writer may be
    /// behind that.
    pub fn head_index(&self) -> anyhow::Result<u64> {
        Ok(self
            .reader
            .get_runtime_info()
            .map_err(|e| anyhow::anyhow!("get_runtime_info failed: {e:?}"))?
            .headIndex)
    }

    /// Resets this reader's own tracked index to the flow's current head — call after `read_next`
    /// returns an error (most likely cause: the tracked index drifted out of the writer's valid
    /// ring-buffer window, e.g. a stall let the writer lap the reader).
    pub fn resync_to_head(&mut self) -> anyhow::Result<()> {
        self.next_index = Some(self.head_index()?);
        Ok(())
    }

    /// Blocking read of `count` samples, continuing this reader's own monotonic index from
    /// wherever the previous call left off (seeded from `head_index()` on the first call, or after
    /// `resync_to_head`). Each SourceEntry's reader tracks this independently (Phase 2: readers are
    /// opened/closed per-activation, not one global index like Phase 1).
    pub fn read_next(&mut self, count: usize, timeout: std::time::Duration) -> anyhow::Result<Vec<Vec<f32>>> {
        let index = match self.next_index {
            Some(i) => i,
            None => self.head_index()?,
        };
        tracing::debug!(index, count, "read_next");
        let planar = self.read_samples_at(index, count, timeout)?;
        self.next_index = Some(index + count as u64);
        Ok(planar)
    }

    /// Blocking read of `count` samples ending at `index` (same end-of-batch indexing convention as
    /// write_next). Returns owned planar float32 data, one Vec<f32> per channel.
    fn read_samples_at(
        &self,
        index: u64,
        count: usize,
        timeout: std::time::Duration,
    ) -> anyhow::Result<Vec<Vec<f32>>> {
        let data = self
            .reader
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
