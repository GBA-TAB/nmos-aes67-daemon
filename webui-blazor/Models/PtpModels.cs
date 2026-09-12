using System.ComponentModel.DataAnnotations;
using System.Text.Json.Serialization;

namespace AesDaemonWebUI.Models;

// GET /api/ptp/status - verified live against the real daemon.
public class PtpStatus
{
    [JsonPropertyName("status")]
    public string Status { get; set; } = "";

    [JsonPropertyName("gmid")]
    public string Gmid { get; set; } = "";

    [JsonPropertyName("jitter")]
    public int Jitter { get; set; }

    [JsonPropertyName("active_leg")]
    public int ActiveLeg { get; set; }

    [JsonPropertyName("leg0_status")]
    public string Leg0Status { get; set; } = "";

    [JsonPropertyName("leg1_status")]
    public string Leg1Status { get; set; } = "";

    [JsonPropertyName("leg0_gmid")]
    public string Leg0Gmid { get; set; } = "";

    [JsonPropertyName("leg1_gmid")]
    public string Leg1Gmid { get; set; } = "";

    [JsonPropertyName("legs_aligned")]
    public bool LegsAligned { get; set; }
}

// GET/POST /api/ptp/config
public class PtpConfig
{
    [JsonPropertyName("domain")]
    [Range(0, 127)]
    public int Domain { get; set; }

    [JsonPropertyName("dscp")]
    public int Dscp { get; set; }
}
