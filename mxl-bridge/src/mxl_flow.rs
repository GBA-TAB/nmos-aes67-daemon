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

/// Builds the flow_def JSON passed to `mxlCreateFlowWriter`. This *is* an NMOS Flow resource JSON
/// (confirmed against MXL's own examples/flow-configs/flow-audio.json) — audio/float32 is MXL's only
/// supported audio sample format (docs/Architecture.md:341), fixed regardless of the AES67 network
/// codec's bit depth (that's a separate, source-side concern handled in alsa_capture.rs's int32->f32
/// conversion).
pub fn build_audio_flow_def(cfg: &Config, flow_id: uuid::Uuid, source_id: uuid::Uuid, device_id: uuid::Uuid) -> String {
    serde_json::json!({
        "id": flow_id.to_string(),
        "device_id": device_id.to_string(),
        "source_id": source_id.to_string(),
        "label": cfg.label,
        "description": format!("{} (bridged from AES67 via mxl-bridge)", cfg.label),
        "format": "urn:x-nmos:format:audio",
        "media_type": "audio/float32",
        "sample_rate": { "numerator": cfg.sample_rate, "denominator": 1 },
        "channel_count": cfg.channels,
        "bit_depth": 32,
        "parents": [],
        "tags": {
            "urn:x-nmos:tag:grouphint/v1.0": [format!("mxl-bridge:{}", cfg.label)]
        }
    })
    .to_string()
}

/// Owns the MXL instance and the samples writer for one continuous (audio) flow.
pub struct MxlAudioFlow {
    pub flow_id: uuid::Uuid,
    instance: mxl::MxlInstance,
    writer: mxl::SamplesWriter,
    channels: usize,
}

impl MxlAudioFlow {
    pub fn create(cfg: &Config, mxl_so_path: &std::path::Path) -> anyhow::Result<Self> {
        let api = mxl::load_api(mxl_so_path)
            .map_err(|e| anyhow::anyhow!("mxl::load_api({mxl_so_path:?}) failed: {e:?}"))?;
        let instance = mxl::MxlInstance::new(api, &cfg.mxl_domain, "")
            .map_err(|e| anyhow::anyhow!("MxlInstance::new({}) failed: {e:?}", cfg.mxl_domain))?;

        let flow_id = stable_id(&format!("mxl-bridge-flow:{}", cfg.label));
        let source_id = stable_id(&format!("mxl-bridge-source:{}", cfg.label));
        let device_id = stable_id("mxl-bridge-device");
        let flow_def = build_audio_flow_def(cfg, flow_id, source_id, device_id);

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
        if channels != cfg.channels as usize {
            anyhow::bail!(
                "MXL flow channel_count ({channels}) does not match configured channels ({})",
                cfg.channels
            );
        }

        let writer = writer
            .to_samples_writer()
            .map_err(|e| anyhow::anyhow!("to_samples_writer failed: {e:?}"))?;

        Ok(Self { flow_id, instance, writer, channels })
    }

    pub fn current_index(&self, sample_rate: &mxl::Rational) -> u64 {
        self.instance.get_current_index(sample_rate)
    }

    /// Writes one period of planar float32 samples (one Vec per channel, all the same length) at
    /// `index` (the sample index of the *first* sample in this batch — see mxl/docs/Timing.md).
    pub fn write_samples(&self, index: u64, planar: &[Vec<f32>]) -> anyhow::Result<()> {
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

        access.commit().map_err(|e| anyhow::anyhow!("commit failed: {e:?}"))
    }
}

/// Reinterprets an f32 slice as raw little-endian bytes (matches MXL's `audio/float32` on-disk
/// layout — IEEE 754, host byte order on x86_64 is already little-endian).
fn bytemuck_cast_f32_slice(src: &[f32]) -> &[u8] {
    // SAFETY: f32 has no padding and any bit pattern is valid; the resulting slice's lifetime and
    // length are derived correctly from the source.
    unsafe { std::slice::from_raw_parts(src.as_ptr() as *const u8, std::mem::size_of_val(src)) }
}
