using System.Text.Json.Serialization;

namespace AesDaemonWebUI.Models;

// GET /api/sinks -> { "sinks": [SinkInfo, ...] } - verified live against the real daemon.
public class SinksResponse
{
    [JsonPropertyName("sinks")]
    public List<SinkInfo> Sinks { get; set; } = [];
}

// One entry in GET /api/sinks, and the body shape for PUT /api/sink/{id}.
public class SinkInfo
{
    [JsonPropertyName("id")]
    public int Id { get; set; }

    [JsonPropertyName("name")]
    public string Name { get; set; } = "";

    [JsonPropertyName("io")]
    public string Io { get; set; } = "";

    [JsonPropertyName("use_sdp")]
    public bool UseSdp { get; set; }

    [JsonPropertyName("source")]
    public string Source { get; set; } = "";

    [JsonPropertyName("sdp")]
    public string Sdp { get; set; } = "";

    [JsonPropertyName("delay")]
    public int Delay { get; set; }

    [JsonPropertyName("ignore_refclk_gmid")]
    public bool IgnoreRefclkGmid { get; set; }

    [JsonPropertyName("map")]
    public List<int> Map { get; set; } = [];
}

// GET /api/sink/status/{id} - verified live.
public class SinkStatus
{
    [JsonPropertyName("sink_flags")]
    public SinkFlags SinkFlags { get; set; } = new();

    [JsonPropertyName("leg2")]
    public SinkLeg2Flags Leg2 { get; set; } = new();

    [JsonPropertyName("sink_min_time")]
    public long SinkMinTime { get; set; }
}

public class SinkFlags
{
    [JsonPropertyName("rtp_seq_id_error")]
    public bool RtpSeqIdError { get; set; }

    [JsonPropertyName("rtp_ssrc_error")]
    public bool RtpSsrcError { get; set; }

    [JsonPropertyName("rtp_payload_type_error")]
    public bool RtpPayloadTypeError { get; set; }

    [JsonPropertyName("rtp_sac_error")]
    public bool RtpSacError { get; set; }

    [JsonPropertyName("receiving_rtp_packet")]
    public bool ReceivingRtpPacket { get; set; }

    [JsonPropertyName("some_muted")]
    public bool SomeMuted { get; set; }

    [JsonPropertyName("all_muted")]
    public bool AllMuted { get; set; }

    [JsonPropertyName("muted")]
    public bool Muted { get; set; }
}

public class SinkLeg2Flags
{
    [JsonPropertyName("present")]
    public bool Present { get; set; }

    [JsonPropertyName("rtp_seq_id_error")]
    public bool RtpSeqIdError { get; set; }

    [JsonPropertyName("rtp_ssrc_error")]
    public bool RtpSsrcError { get; set; }

    [JsonPropertyName("rtp_payload_type_error")]
    public bool RtpPayloadTypeError { get; set; }

    [JsonPropertyName("rtp_sac_error")]
    public bool RtpSacError { get; set; }

    [JsonPropertyName("receiving_rtp_packet")]
    public bool ReceivingRtpPacket { get; set; }
}
