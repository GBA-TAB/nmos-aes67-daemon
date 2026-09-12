using System.Text.Json.Serialization;

namespace AesDaemonWebUI.Models;

public class Is08ResourceProperties
{
    [JsonPropertyName("name")]
    public string Name { get; set; } = "";

    [JsonPropertyName("description")]
    public string Description { get; set; } = "";
}

public class Is08Channel
{
    [JsonPropertyName("label")]
    public string Label { get; set; } = "";
}

// GET /x-nmos/channelmapping/v1.0/map/active/ - the daemon's own ground truth for "what is
// actually patched right now", keyed by output id then channel index (as a string, per the IS-08
// wire format). Reading this directly instead of re-deriving the same fact from /api/sinks and
// /api/sources' map[] arrays (matched to IS-08 resources by label/name) is what fixed a real
// staleness bug: two sinks on this daemon ended up with overlapping ALSA channel ranges, which
// made the name-matching derivation pick the wrong "currently selected" dropdown value - the
// active map is authoritative and immune to that class of drift.
public class Is08ActiveMapResponse
{
    [JsonPropertyName("map")]
    public Dictionary<string, Dictionary<string, Is08ActiveMapEntry>> Map { get; set; } = new();
}

public class Is08ActiveMapEntry
{
    [JsonPropertyName("input")]
    public string? Input { get; set; }

    [JsonPropertyName("channel_index")]
    public int? ChannelIndex { get; set; }
}

// POST /x-nmos/channelmapping/v1.0/map/activations/ body shape.
public class Is08MapActivation
{
    [JsonPropertyName("activation")]
    public Is08ActivationMode Activation { get; set; } = new();

    [JsonPropertyName("map")]
    public Dictionary<string, Dictionary<string, Is08ChannelAssignment>> Action { get; set; } = new();
}

public class Is08ActivationMode
{
    [JsonPropertyName("mode")]
    public string Mode { get; set; } = "activate_immediate";
}

public class Is08ChannelAssignment
{
    [JsonPropertyName("input")]
    public string? Input { get; set; }

    [JsonPropertyName("channel_index")]
    public int? ChannelIndex { get; set; }
}
