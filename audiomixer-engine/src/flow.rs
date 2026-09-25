//! Adapted from mxl-bridge's own `src/mxl_flow.rs` (same per-flow running-index design, same
//! grouphint-format fix — see its own comment below for why that specific tag value can't be
//! interpolated with a label). Not shared as a library for the same reason as `ids.rs`: mxl-bridge
//! is a binary crate. Trimmed to just the generic writer/reader wrapper — this app's own id
//! derivation lives in `ids.rs`, not here.

/// Builds the flow_def JSON passed to `mxlCreateFlowWriter`. This *is* an NMOS Flow resource JSON.
pub fn build_audio_flow_def(
    sample_rate: u32,
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
        "description": format!("{label} (mxl-test-app bus output)"),
        "format": "urn:x-nmos:format:audio",
        "media_type": "audio/float32",
        "sample_rate": { "numerator": sample_rate, "denominator": 1 },
        "channel_count": channel_count,
        "bit_depth": 32,
        "parents": [],
        "tags": {
            // Fixed, never interpolated with `label`: MXL's FlowParser requires this tag's value
            // as a strict "<scope>:<value>" pair where scope must literally be "device" or
            // "node" — see mxl-bridge's mxl_flow.rs for how that was found (a colon in the label
            // broke it there).
            "urn:x-nmos:tag:grouphint/v1.0": ["device:mxl-test-app"]
        }
    })
    .to_string()
}

/// Owns the MXL instance and the samples writer for one continuous (audio) flow, plus its own
/// running write index.
pub struct FlowWriter {
    instance: mxl::MxlInstance,
    writer: mxl::SamplesWriter,
    channels: usize,
    sample_rate: mxl::Rational,
    next_index: Option<u64>,
}

impl FlowWriter {
    #[allow(clippy::too_many_arguments)]
    pub fn create(
        mxl_domain: &str,
        mxl_so_path: &std::path::Path,
        sample_rate: u32,
        flow_id: uuid::Uuid,
        source_id: uuid::Uuid,
        device_id: uuid::Uuid,
        label: &str,
        channel_count: u32,
    ) -> anyhow::Result<Self> {
        let api = mxl::load_api(mxl_so_path).map_err(|e| anyhow::anyhow!("mxl::load_api({mxl_so_path:?}) failed: {e:?}"))?;
        let instance =
            mxl::MxlInstance::new(api, mxl_domain, "").map_err(|e| anyhow::anyhow!("MxlInstance::new({mxl_domain}) failed: {e:?}"))?;

        let flow_def = build_audio_flow_def(sample_rate, flow_id, source_id, device_id, label, channel_count);

        let (writer, info, was_created) =
            instance.create_flow_writer(&flow_def, None).map_err(|e| anyhow::anyhow!("create_flow_writer failed: {e:?}"))?;
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

        let writer = writer.to_samples_writer().map_err(|e| anyhow::anyhow!("to_samples_writer failed: {e:?}"))?;

        let sample_rate = mxl::Rational { numerator: sample_rate as i64, denominator: 1 };
        Ok(Self { instance, writer, channels, sample_rate, next_index: None })
    }

    /// Writes one period of planar float32 samples, stamped so the block ENDS now (TAI, via MXL's
    /// current index): consecutive blocks continue seamlessly from the previous one, but when that
    /// accumulated index drifts more than 5 ms from the real clock it snaps back to it. Before
    /// (until 2026-09-25) the index was taken from the clock only on the first block and then
    /// just accumulated: every period the engine skipped or ran late added up, and live the output
    /// flows sat ~4.8 s behind TAI - invisible to head-following readers, but a clock-based reader
    /// (anything reading "now minus a margin") found nothing. Same fix as mxl-bridge's writer.
    pub fn write_next(&mut self, planar: &[Vec<f32>]) -> anyhow::Result<()> {
        let count = planar.first().map(|c| c.len()).unwrap_or(0);
        if count == 0 {
            return Ok(());
        }
        let target = self.instance.get_current_index(&self.sample_rate).saturating_sub(count as u64);
        let tolerance = (self.sample_rate.numerator / 200).max(1) as u64; // 5 ms
        let index = match self.next_index {
            Some(i) if i.abs_diff(target) <= tolerance => i,
            // Ahead of the clock (a catch-up burst after a stall): MXL refuses to write before the
            // last committed index, so snapping back would fail every block until the clock caught
            // up. Drop this block instead - the next ones land at the same index and realign.
            Some(i) if i > target => {
                tracing::debug!(accumulated_index = i, target, "MXL output ahead of the real clock, dropping a block");
                return Ok(());
            }
            Some(i) => {
                tracing::warn!(accumulated_index = i, target, drift_samples = target - i, "MXL output index fell behind the real clock, snapping to it");
                target
            }
            None => target,
        };

        let mut access =
            self.writer.open_samples(index + count as u64 - 1, count).map_err(|e| anyhow::anyhow!("open_samples failed: {e:?}"))?;

        for ch in 0..self.channels.min(planar.len()) {
            let (dst1, dst2) = access.channel_data_mut(ch).map_err(|e| anyhow::anyhow!("channel_data_mut({ch}) failed: {e:?}"))?;
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

fn bytemuck_cast_f32_slice(src: &[f32]) -> &[u8] {
    // SAFETY: f32 has no padding and any bit pattern is valid; the resulting slice's lifetime and
    // length are derived correctly from the source.
    unsafe { std::slice::from_raw_parts(src.as_ptr() as *const u8, std::mem::size_of_val(src)) }
}

/// A real, genuine mismatch (not a server-side fault) — the flow being subscribed to has more
/// channels than this grid entry's own standard-sized placeholder can hold. Distinguishable out of
/// `FlowReader::open`'s `anyhow::Result` via `downcast_ref` specifically so `nmos/server.rs`'s
/// receiver-activation handler can map this one case to a real IS-05-correct `400 Bad Request`
/// instead of the generic `500` every other `FlowReader::open` failure still gets.
#[derive(Debug)]
pub struct ChannelCountExceedsPlaceholder {
    pub actual: usize,
    pub placeholder: usize,
}

impl std::fmt::Display for ChannelCountExceedsPlaceholder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "flow has {} channel(s), this receiver's placeholder only accepts up to {}", self.actual, self.placeholder)
    }
}

impl std::error::Error for ChannelCountExceedsPlaceholder {}

/// Reads an existing MXL audio flow — some other producer (mxl-bridge, or another MXL app) writes
/// it, this app consumes and mixes it.
pub struct FlowReader {
    reader: mxl::SamplesReader,
    channels: usize,
    next_index: Option<u64>,
}

impl FlowReader {
    /// `placeholder_channels` is a grid entry's own standard-sized ("Stream Rx") receive capacity
    /// (`layout::is_standard_stream_size`, e.g. 8), not an exact count the real flow must match --
    /// see `SESSION-2026-09-15-DYNAMIC-RX-SIZING-DESIGN.md`/`SESSION-2026-09-15-STANDARD-SIZE-GRID-PLAN.md`
    /// for the full design. Accepts any real flow whose own channel count fits within the
    /// placeholder (`channels <= placeholder_channels`); a real flow *larger* than the placeholder
    /// is a genuine, distinguishable rejection (`ChannelCountExceedsPlaceholder`, downcastable out
    /// of the returned `anyhow::Error` -- see `nmos/server.rs`'s receiver-activation handler, which
    /// maps exactly this case to a real `400`, not `500`). A real flow *smaller* than the
    /// placeholder is the normal case, not an error -- `read_next`'s own returned buffer stays the
    /// real (smaller) channel count; padding the placeholder's remaining channels with silence for
    /// display purposes is the caller's job (`engine.rs`'s per-period read step), not this type's.
    pub fn open(mxl_domain: &str, mxl_so_path: &std::path::Path, flow_id: &str, placeholder_channels: usize) -> anyhow::Result<Self> {
        let api = mxl::load_api(mxl_so_path).map_err(|e| anyhow::anyhow!("mxl::load_api({mxl_so_path:?}) failed: {e:?}"))?;
        let instance =
            mxl::MxlInstance::new(api, mxl_domain, "").map_err(|e| anyhow::anyhow!("MxlInstance::new({mxl_domain}) failed: {e:?}"))?;

        let reader =
            instance.create_flow_reader(flow_id).map_err(|e| anyhow::anyhow!("create_flow_reader({flow_id}) failed: {e:?}"))?;
        let info = reader.get_info().map_err(|e| anyhow::anyhow!("get_info failed: {e:?}"))?.config;
        let channels = info
            .continuous()
            .map_err(|e| anyhow::anyhow!("flow {flow_id} is not a continuous (audio) flow: {e:?}"))?
            .channelCount as usize;
        if channels > placeholder_channels {
            return Err(ChannelCountExceedsPlaceholder { actual: channels, placeholder: placeholder_channels }.into());
        }

        let reader = reader.to_samples_reader().map_err(|e| anyhow::anyhow!("to_samples_reader failed: {e:?}"))?;
        Ok(Self { reader, channels, next_index: None })
    }

    pub fn head_index(&self) -> anyhow::Result<u64> {
        // `anyhow::Error::from(e)` (not a formatted `anyhow::anyhow!("...: {e:?}")` string) keeps
        // the real `mxl::Error` downcastable out of the returned error -- `engine.rs`'s own
        // read-failure handling needs to tell `mxl::Error::FlowInvalid` (needs a fresh reopen, not
        // just a resync) apart from every other failure (where resync is the right response), and
        // a string-formatted error can't be downcast back to anything.
        Ok(self.reader.get_runtime_info().map_err(|e| anyhow::Error::from(e).context("get_runtime_info failed"))?.headIndex)
    }

    pub fn resync_to_head(&mut self) -> anyhow::Result<()> {
        self.next_index = Some(self.head_index()?);
        Ok(())
    }

    pub fn read_next(&mut self, count: usize, timeout: std::time::Duration) -> anyhow::Result<Vec<Vec<f32>>> {
        let index = match self.next_index {
            Some(i) => i,
            None => self.head_index()?,
        };
        let planar = self.read_samples_at(index, count, timeout)?;
        self.next_index = Some(index + count as u64);
        Ok(planar)
    }

    fn read_samples_at(&self, index: u64, count: usize, timeout: std::time::Duration) -> anyhow::Result<Vec<Vec<f32>>> {
        // Same downcast-preserving wrap as `head_index` above, same reason.
        let data = self.reader.get_samples(index, count, timeout).map_err(|e| anyhow::Error::from(e).context("get_samples failed"))?;

        let mut planar = Vec::with_capacity(self.channels);
        for ch in 0..self.channels {
            let (b1, b2) = data.channel_data(ch).map_err(|e| anyhow::anyhow!("channel_data({ch}) failed: {e:?}"))?;
            let mut bytes = Vec::with_capacity(b1.len() + b2.len());
            bytes.extend_from_slice(b1);
            bytes.extend_from_slice(b2);
            let samples: Vec<f32> = bytes.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect();
            planar.push(samples);
        }
        Ok(planar)
    }
}
