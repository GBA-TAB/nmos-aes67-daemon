using System.Text.Json.Serialization;

namespace AesDaemonWebUI.Models;

// Mirrors daemon/json.cpp's config_to_json exactly (field-for-field, snake_case on the wire) -
// verified live against a real running daemon's GET /api/config, not just the source's own
// json.cpp. Read-only fields here (HttpPort, MacAddr, IpAddr, NodeId, MaxTicFrameSize) are
// daemon-derived and shown disabled in the Config UI, matching the original React app.
public class DaemonConfig
{
    [JsonPropertyName("http_port")]
    public int HttpPort { get; set; }

    [JsonPropertyName("rtsp_port")]
    public int RtspPort { get; set; }

    [JsonPropertyName("http_base_dir")]
    public string HttpBaseDir { get; set; } = "";

    [JsonPropertyName("log_severity")]
    public int LogSeverity { get; set; }

    [JsonPropertyName("playout_delay")]
    public int PlayoutDelay { get; set; }

    [JsonPropertyName("tic_frame_size_at_1fs")]
    public int TicFrameSizeAt1Fs { get; set; }

    [JsonPropertyName("max_tic_frame_size")]
    public int MaxTicFrameSize { get; set; }

    [JsonPropertyName("sample_rate")]
    public int SampleRate { get; set; }

    [JsonPropertyName("rtp_mcast_base")]
    public string RtpMcastBase { get; set; } = "";

    [JsonPropertyName("rtp_mcast_base_sec")]
    public string RtpMcastBaseSec { get; set; } = "";

    [JsonPropertyName("rtp_port")]
    public int RtpPort { get; set; }

    [JsonPropertyName("rtp_port_sec")]
    public int RtpPortSec { get; set; }

    [JsonPropertyName("ptp_domain")]
    public int PtpDomain { get; set; }

    [JsonPropertyName("ptp_dscp")]
    public int PtpDscp { get; set; }

    [JsonPropertyName("sap_mcast_addr")]
    public string SapMcastAddr { get; set; } = "";

    [JsonPropertyName("sap_interval")]
    public int SapInterval { get; set; }

    [JsonPropertyName("syslog_proto")]
    public string SyslogProto { get; set; } = "";

    [JsonPropertyName("syslog_server")]
    public string SyslogServer { get; set; } = "";

    [JsonPropertyName("status_file")]
    public string StatusFile { get; set; } = "";

    [JsonPropertyName("interface_name")]
    public string InterfaceName { get; set; } = "";

    [JsonPropertyName("mdns_enabled")]
    public bool MdnsEnabled { get; set; }

    [JsonPropertyName("custom_node_id")]
    public string CustomNodeId { get; set; } = "";

    [JsonPropertyName("node_id")]
    public string NodeId { get; set; } = "";

    [JsonPropertyName("ptp_status_script")]
    public string PtpStatusScript { get; set; } = "";

    [JsonPropertyName("mac_addr")]
    public string MacAddr { get; set; } = "";

    [JsonPropertyName("ip_addr")]
    public string IpAddr { get; set; } = "";

    [JsonPropertyName("streamer_channels")]
    public int StreamerChannels { get; set; }

    [JsonPropertyName("alsa_channels")]
    public int AlsaChannels { get; set; }

    [JsonPropertyName("streamer_files_num")]
    public int StreamerFilesNum { get; set; }

    [JsonPropertyName("streamer_file_duration")]
    public int StreamerFileDuration { get; set; }

    [JsonPropertyName("streamer_player_buffer_files_num")]
    public int StreamerPlayerBufferFilesNum { get; set; }

    [JsonPropertyName("streamer_enabled")]
    public bool StreamerEnabled { get; set; }

    [JsonPropertyName("auto_sinks_update")]
    public bool AutoSinksUpdate { get; set; }

    [JsonPropertyName("nmos_enabled")]
    public bool NmosEnabled { get; set; }

    [JsonPropertyName("nmos_registry_address")]
    public string NmosRegistryAddress { get; set; } = "";

    [JsonPropertyName("nmos_registry_port")]
    public int NmosRegistryPort { get; set; }

    [JsonPropertyName("nmos_node_port")]
    public int NmosNodePort { get; set; }

    [JsonPropertyName("nmos_label")]
    public string NmosLabel { get; set; } = "";

    [JsonPropertyName("nmos_registry_autodiscovery")]
    public bool NmosRegistryAutodiscovery { get; set; }

    [JsonPropertyName("nmos_mdns_enabled")]
    public bool NmosMdnsEnabled { get; set; }

    [JsonPropertyName("is12_enabled")]
    public bool Is12Enabled { get; set; }

    [JsonPropertyName("is08_enabled")]
    public bool Is08Enabled { get; set; }
}
