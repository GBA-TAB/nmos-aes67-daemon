use serde::Deserialize;

#[derive(Deserialize, Clone, Debug)]
pub struct Config {
    /// ALSA capture device to bridge from (the RAVENNA PCM device), e.g. "hw:RAVENNA,0".
    pub alsa_source_device: String,
    pub sample_rate: u32,
    pub channels: u32,
    /// ALSA period size in frames. Also the MXL sample-batch size per commit.
    pub period_frames: u32,

    /// MXL domain directory (must live on tmpfs) where flow ring buffers are stored.
    pub mxl_domain: String,
    /// Human-readable label used for both the MXL flow and the NMOS Source/Flow/Sender.
    pub label: String,

    pub nmos_node_port: u16,
    pub nmos_label: String,
    /// IS-04 registry address. If unset, registration is skipped (Node API still served).
    pub nmos_registry_address: Option<String>,
    pub nmos_registry_port: u16,
    pub interface_name: String,

    /// TX direction (MXL flow -> ALSA playback): which flow to consume and which ALSA device to
    /// play it out on. Both optional and both required together — real deployments will get this
    /// from IS-05 activation instead (not implemented yet, see README), this is a manual override
    /// for testing the TX path standalone.
    #[serde(default)]
    pub tx_source_flow_id: Option<String>,
    #[serde(default)]
    pub tx_alsa_playback_device: Option<String>,
}

impl Config {
    pub fn load(path: &str) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("reading config file '{path}': {e}"))?;
        let cfg: Config = serde_json::from_str(&text)
            .map_err(|e| anyhow::anyhow!("parsing config file '{path}': {e}"))?;
        Ok(cfg)
    }
}
