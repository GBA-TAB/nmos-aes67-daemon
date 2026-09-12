using System.Text.Json.Serialization;

namespace AesDaemonWebUI.Models;

// GET /api/sources -> { "sources": [SourceInfo, ...] }
public class SourcesResponse
{
    [JsonPropertyName("sources")]
    public List<SourceInfo> Sources { get; set; } = [];
}

// One entry in GET /api/sources, and the body shape for PUT /api/source/{id}.
public class SourceInfo
{
    [JsonPropertyName("id")]
    public int Id { get; set; }

    [JsonPropertyName("enabled")]
    public bool Enabled { get; set; }

    [JsonPropertyName("name")]
    public string Name { get; set; } = "";

    [JsonPropertyName("io")]
    public string Io { get; set; } = "";

    [JsonPropertyName("max_samples_per_packet")]
    public int MaxSamplesPerPacket { get; set; }

    [JsonPropertyName("codec")]
    public string Codec { get; set; } = "";

    [JsonPropertyName("address")]
    public string Address { get; set; } = "";

    [JsonPropertyName("ttl")]
    public int Ttl { get; set; }

    [JsonPropertyName("payload_type")]
    public int PayloadType { get; set; }

    [JsonPropertyName("dscp")]
    public int Dscp { get; set; }

    [JsonPropertyName("refclk_ptp_traceable")]
    public bool RefclkPtpTraceable { get; set; }

    [JsonPropertyName("map")]
    public List<int> Map { get; set; } = [];
}

// GET /api/source/status/{id}
public class SourceStatus
{
    [JsonPropertyName("source_flags")]
    public SourceFlags SourceFlags { get; set; } = new();

    [JsonPropertyName("leg2")]
    public SourceLeg2Flags Leg2 { get; set; } = new();
}

public class SourceFlags
{
    [JsonPropertyName("transmitting")]
    public bool Transmitting { get; set; }

    [JsonPropertyName("underrun")]
    public bool Underrun { get; set; }
}

public class SourceLeg2Flags
{
    [JsonPropertyName("present")]
    public bool Present { get; set; }

    [JsonPropertyName("transmitting")]
    public bool Transmitting { get; set; }

    [JsonPropertyName("underrun")]
    public bool Underrun { get; set; }
}
