using System.ComponentModel.DataAnnotations;
using System.Text.Json.Serialization;

namespace AesDaemonWebUI.Models;

// POST /api/config body - deliberately only the writable subset DaemonConfig exposes read-only
// (http_port, mac_addr, ip_addr, node_id, interface_name, status_file, ptp_status_script,
// ptp_domain/ptp_dscp - the latter two have their own PTP config endpoint), matching
// Services.js's setConfig(...) parameter list exactly (verified against Services.js:141-166)
// rather than posting the whole DaemonConfig back and hoping the daemon ignores the rest.
//
// Validation attributes replace the original's manual per-field *Err boolean + checkValidity()
// pattern - same constraints (regex patterns, min/max), enforced via EditForm +
// DataAnnotationsValidator instead of hand-rolled state.
public class ConfigUpdateRequest
{
    [JsonPropertyName("log_severity")]
    public int LogSeverity { get; set; }

    [JsonPropertyName("syslog_proto")]
    public string SyslogProto { get; set; } = "";

    [JsonPropertyName("syslog_server")]
    [RegularExpression(@"^[^\s]*$", ErrorMessage = "Syslog server must not contain spaces")]
    public string SyslogServer { get; set; } = "";

    [JsonPropertyName("rtp_mcast_base")]
    [Required]
    [RegularExpression(McastAddressPattern, ErrorMessage = "Must be a valid multicast address (224.0.0.0-239.255.255.255)")]
    public string RtpMcastBase { get; set; } = "";

    [JsonPropertyName("rtp_mcast_base_sec")]
    [Required]
    [RegularExpression(McastAddressPattern, ErrorMessage = "Must be a valid multicast address (224.0.0.0-239.255.255.255)")]
    public string RtpMcastBaseSec { get; set; } = "";

    [JsonPropertyName("rtp_port")]
    [Range(1024, 65536)]
    public int RtpPort { get; set; }

    [JsonPropertyName("rtp_port_sec")]
    [Range(1024, 65536)]
    public int RtpPortSec { get; set; }

    [JsonPropertyName("rtsp_port")]
    [Range(1024, 65536)]
    public int RtspPort { get; set; }

    [JsonPropertyName("playout_delay")]
    [Range(0, 4000)]
    public int PlayoutDelay { get; set; }

    [JsonPropertyName("tic_frame_size_at_1fs")]
    public int TicFrameSizeAt1Fs { get; set; }

    [JsonPropertyName("sample_rate")]
    public int SampleRate { get; set; }

    [JsonPropertyName("max_tic_frame_size")]
    [Range(192, 8192)]
    public int MaxTicFrameSize { get; set; }

    [JsonPropertyName("sap_mcast_addr")]
    [Required]
    [RegularExpression(McastAddressPattern, ErrorMessage = "Must be a valid multicast address (224.0.0.0-239.255.255.255)")]
    public string SapMcastAddr { get; set; } = "";

    [JsonPropertyName("sap_interval")]
    [Range(0, 255)]
    public int SapInterval { get; set; }

    [JsonPropertyName("mdns_enabled")]
    public bool MdnsEnabled { get; set; }

    // Empty is always valid (falls back to the daemon-derived node id) - only a non-empty value
    // must satisfy the original's minLength=5/maxLength=48/pattern trio (Config.jsx:299, and its
    // inputIsValid() at :174 explicitly exempts the empty string from customNodeIdErr).
    [JsonPropertyName("custom_node_id")]
    [RegularExpression("^$|^[A-Za-z0-9 _]{5,48}$", ErrorMessage = "Must be empty, or 5-48 letters, digits, spaces and underscores")]
    public string CustomNodeId { get; set; } = "";

    [JsonPropertyName("auto_sinks_update")]
    public bool AutoSinksUpdate { get; set; }

    [JsonPropertyName("streamer_enabled")]
    public bool StreamerEnabled { get; set; }

    [JsonPropertyName("streamer_channels")]
    [Range(2, 16)]
    public int StreamerChannels { get; set; }

    [JsonPropertyName("streamer_files_num")]
    [Range(4, 16)]
    public int StreamerFilesNum { get; set; }

    [JsonPropertyName("streamer_file_duration")]
    [Range(1, 4)]
    public int StreamerFileDuration { get; set; }

    [JsonPropertyName("streamer_player_buffer_files_num")]
    public int StreamerPlayerBufferFilesNum { get; set; }

    [JsonPropertyName("nmos_enabled")]
    public bool NmosEnabled { get; set; }

    [JsonPropertyName("nmos_registry_autodiscovery")]
    public bool NmosRegistryAutodiscovery { get; set; }

    [JsonPropertyName("nmos_registry_address")]
    [StringLength(253)]
    public string NmosRegistryAddress { get; set; } = "";

    [JsonPropertyName("nmos_registry_port")]
    [Range(1, 65535)]
    public int NmosRegistryPort { get; set; }

    [JsonPropertyName("nmos_node_port")]
    [Range(1, 65535)]
    public int NmosNodePort { get; set; }

    [JsonPropertyName("nmos_mdns_enabled")]
    public bool NmosMdnsEnabled { get; set; }

    // RFC 8331/IANA multicast range 224.0.0.0-239.255.255.255 - same regex as the original
    // (Config.jsx's own "pattern" attribute), ported verbatim.
    public const string McastAddressPattern = @"^2(?:2[4-9]|3\d)(?:\.(?:25[0-5]|2[0-4]\d|1\d\d|[1-9]\d?|0)){3}$";

    public static ConfigUpdateRequest FromConfig(DaemonConfig c) => new()
    {
        LogSeverity = c.LogSeverity,
        SyslogProto = c.SyslogProto,
        SyslogServer = c.SyslogServer,
        RtpMcastBase = c.RtpMcastBase,
        RtpMcastBaseSec = c.RtpMcastBaseSec,
        RtpPort = c.RtpPort,
        RtpPortSec = c.RtpPortSec,
        RtspPort = c.RtspPort,
        PlayoutDelay = c.PlayoutDelay,
        TicFrameSizeAt1Fs = c.TicFrameSizeAt1Fs,
        SampleRate = c.SampleRate,
        MaxTicFrameSize = c.MaxTicFrameSize,
        SapMcastAddr = c.SapMcastAddr,
        SapInterval = c.SapInterval,
        MdnsEnabled = c.MdnsEnabled,
        CustomNodeId = c.CustomNodeId,
        AutoSinksUpdate = c.AutoSinksUpdate,
        StreamerEnabled = c.StreamerEnabled,
        StreamerChannels = c.StreamerChannels,
        StreamerFilesNum = c.StreamerFilesNum,
        StreamerFileDuration = c.StreamerFileDuration,
        StreamerPlayerBufferFilesNum = c.StreamerPlayerBufferFilesNum,
        NmosEnabled = c.NmosEnabled,
        NmosRegistryAutodiscovery = c.NmosRegistryAutodiscovery,
        NmosRegistryAddress = c.NmosRegistryAddress,
        NmosRegistryPort = c.NmosRegistryPort,
        NmosNodePort = c.NmosNodePort,
        NmosMdnsEnabled = c.NmosMdnsEnabled,
    };
}
