using System.Text.Json.Serialization;

namespace AesDaemonWebUI.Models;

// GET /api/browse/sources/all -> { "remote_sources": [RemoteSource, ...] } - verified live.
public class RemoteSourcesResponse
{
    [JsonPropertyName("remote_sources")]
    public List<RemoteSource> RemoteSources { get; set; } = [];
}

public class RemoteSource
{
    [JsonPropertyName("source")]
    public string Source { get; set; } = ""; // "SAP" or "mDNS"

    [JsonPropertyName("id")]
    public string Id { get; set; } = "";

    [JsonPropertyName("name")]
    public string Name { get; set; } = "";

    [JsonPropertyName("domain")]
    public string Domain { get; set; } = "";

    [JsonPropertyName("address")]
    public string Address { get; set; } = "";

    [JsonPropertyName("sdp")]
    public string Sdp { get; set; } = "";

    [JsonPropertyName("last_seen")]
    public int LastSeen { get; set; }

    [JsonPropertyName("announce_period")]
    public int AnnouncePeriod { get; set; }
}
