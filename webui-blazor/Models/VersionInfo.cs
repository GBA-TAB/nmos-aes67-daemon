using System.Text.Json.Serialization;

namespace AesDaemonWebUI.Models;

public class VersionInfo
{
    [JsonPropertyName("version")]
    public string Version { get; set; } = "";
}
