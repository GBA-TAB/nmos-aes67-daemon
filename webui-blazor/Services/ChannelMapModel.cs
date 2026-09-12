using System.Text.RegularExpressions;
using AesDaemonWebUI.Models;

namespace AesDaemonWebUI.Services;

// Ports ChannelMap.jsx's grid-building logic as a pure, testable class - kept separate from the
// page per the migration plan's own note that this is the single highest-risk piece to port
// faithfully (it string-matches resource labels like "Stream Rx: <name>"/"ALSA Capture N" across
// four independent data sources: IS-08 inputs, IS-08 outputs, local sinks, and local sources).
//
// Unlike the original, every "what is currently selected" fact (PlaybackSelection,
// CaptureSelection, and each row's AlsaChannel) is read from the daemon's own
// /x-nmos/channelmapping/v1.0/map/active/ endpoint - ground truth for what is actually patched
// right now - rather than re-derived by cross-referencing /api/sinks and /api/sources' map[]
// arrays against IS-08 resource labels. That indirect approach (which the original app used) goes
// stale whenever two sinks/sources end up with overlapping ALSA channel ranges - a real case found
// live on this daemon (two sinks both claiming channels 32-39) that made it pick the wrong
// "currently selected" dropdown value. Label-matching is still used, but only to determine which
// resources exist and what choices to offer - never to infer what's currently active.
public record ChannelOption(string Value, string Label);

public record RxChannelRow(string Value, string Label, int SinkId, int AlsaChannel);

public record TxChannelRow(string Value, string Label, int SourceId, int AlsaChannel);

// One IS-08 input or output resource, already resolved to its properties.name and per-channel
// labels - the page fetches these (2 HTTP calls per resource) and hands the flattened result here.
public record ChannelMapResourceInfo(string Id, string Label, List<string> ChannelLabels);

public class ChannelMapView
{
    public List<int> Channels { get; init; } = new();
    public Dictionary<int, string> CaptureUuid { get; init; } = new();
    public Dictionary<int, string> PlaybackUuid { get; init; } = new();
    public Dictionary<string, int> InputSinkIds { get; init; } = new();
    public Dictionary<string, List<ChannelOption>> InputOptions { get; init; } = new();
    public List<ChannelOption> TxOptions { get; init; } = new();
    public List<ChannelOption> CaptureOptions { get; init; } = new();
    public Dictionary<int, string> PlaybackSelection { get; init; } = new();
    public Dictionary<int, string> CaptureSelection { get; init; } = new();
    public List<RxChannelRow> RxChannels { get; init; } = new();
    public List<TxChannelRow> TxChannels { get; init; } = new();
}

public static class ChannelMapModel
{
    private static readonly Regex CaptureRe = new(@"^ALSA Capture (\d+)$");
    private static readonly Regex PlaybackRe = new(@"^ALSA Playback (\d+)$");
    private static readonly Regex SdpSessionNameRe = new(@"(?:^|\r?\n)s=([^\r\n]+)");
    private const string StreamRxPrefix = "Stream Rx: ";
    private const string StreamTxPrefix = "Stream Tx: ";

    public static ChannelMapView Build(
        List<ChannelMapResourceInfo> inputInfos,
        List<ChannelMapResourceInfo> outputInfos,
        List<SinkInfo> sinks,
        List<SourceInfo> sources,
        Is08ActiveMapResponse activeMap)
    {
        var sinksByName = sinks
            .GroupBy(s => s.Name)
            .ToDictionary(g => g.Key, g => g.First().Id);
        var sinkStreamNames = new Dictionary<int, string>();
        foreach (var s in sinks)
        {
            var m = SdpSessionNameRe.Match(s.Sdp ?? "");
            if (m.Success)
            {
                sinkStreamNames[s.Id] = m.Groups[1].Value;
            }
        }

        var sourcesByName = sources
            .GroupBy(s => s.Name)
            .ToDictionary(g => g.Key, g => g.First().Id);

        var inputSinkIds = new Dictionary<string, int>();
        var inputOptions = new Dictionary<string, List<ChannelOption>>();
        var captureUuid = new Dictionary<int, string>();
        var rxRowSeeds = new List<(string Value, string Label, int SinkId)>();

        foreach (var info in inputInfos)
        {
            var captureMatch = CaptureRe.Match(info.Label);
            if (captureMatch.Success)
            {
                // Recorded only as a capture-source identity - an ALSA Capture channel is never
                // itself a valid choice for the Stream Rx -> Playback column.
                captureUuid[int.Parse(captureMatch.Groups[1].Value)] = info.Id;
                continue;
            }

            var sinkName = info.Label.StartsWith(StreamRxPrefix) ? info.Label[StreamRxPrefix.Length..] : info.Label;
            var hasSink = sinksByName.TryGetValue(sinkName, out var sinkId);
            if (hasSink)
            {
                inputSinkIds[info.Id] = sinkId;
            }

            List<ChannelOption> opts;
            if (hasSink && info.ChannelLabels.Count > 1)
            {
                // A multichannel Sink is one Input resource but N real audio channels - list each
                // one directly instead of a two-step resource-then-channel picker.
                sinkStreamNames.TryGetValue(sinkId, out var streamName);
                var basePrefix = $"Sink {sinkId}" + (string.IsNullOrEmpty(streamName) ? "" : $" · {streamName}");
                opts = info.ChannelLabels.Select((chLabel, i) => new ChannelOption($"{info.Id}::{i}", $"{basePrefix} · {chLabel}")).ToList();
            }
            else
            {
                opts = new List<ChannelOption> { new($"{info.Id}::0", info.Label) };
            }
            inputOptions[info.Id] = opts;

            // Stream-view row: one per real Rx channel, regardless of whether it's currently
            // selected anywhere in the ALSA view. Its real AlsaChannel is filled in below, from
            // the active map, once playbackUuid/channels are known.
            if (hasSink)
            {
                foreach (var opt in opts)
                {
                    rxRowSeeds.Add((opt.Value, opt.Label, sinkId));
                }
            }
        }

        var playbackUuid = new Dictionary<int, string>();
        var txOptions = new List<ChannelOption>();
        var txRowSeeds = new List<(string Value, string Label, int SourceId)>();

        foreach (var o in outputInfos)
        {
            var playbackMatch = PlaybackRe.Match(o.Label);
            if (playbackMatch.Success)
            {
                playbackUuid[int.Parse(playbackMatch.Groups[1].Value)] = o.Id;
                continue;
            }

            // Everything else is a Sender's Tx channel group - flatten into one selectable entry
            // per real channel, shared across every row.
            var sourceName = o.Label.StartsWith(StreamTxPrefix) ? o.Label[StreamTxPrefix.Length..] : o.Label;
            var hasSource = sourcesByName.TryGetValue(sourceName, out var sourceId);

            for (var i = 0; i < o.ChannelLabels.Count; i++)
            {
                var value = $"{o.Id}::{i}";
                var label = $"{o.Label} · {o.ChannelLabels[i]}";
                txOptions.Add(new ChannelOption(value, label));
                if (hasSource)
                {
                    txRowSeeds.Add((value, label, sourceId));
                }
            }
        }

        var channels = playbackUuid.Keys.OrderBy(c => c).ToList();

        // Every option the Stream view's "Capture" dropdown can offer for a Tx-channel row: a raw
        // ALSA Capture channel, or (repeater) any real Sink channel - the same choice the ALSA
        // view's "Stream Tx" column already exposes, just anchored to a fixed output row instead.
        var captureOptions = new List<ChannelOption>();
        captureOptions.AddRange(inputOptions.Values.SelectMany(v => v));
        captureOptions.AddRange(channels.Select(n => new ChannelOption($"{captureUuid.GetValueOrDefault(n, "")}::0", $"ALSA Capture {n}")));

        // --- Everything below reads the active map (ground truth), not sink/source map[] arrays ---

        var map = activeMap.Map;

        // Column 1: which input (and its channel) actually feeds this ALSA Playback output right
        // now, per the daemon's own active map - the one, unambiguous fact about what's patched.
        var playbackSelection = channels.ToDictionary(ch => ch, _ => "");
        var alsaChannelByActiveInputValue = new Dictionary<string, int>();
        foreach (var ch in channels)
        {
            if (!playbackUuid.TryGetValue(ch, out var outputId)) continue;
            if (!map.TryGetValue(outputId, out var chMap)) continue;
            if (!chMap.TryGetValue("0", out var entry) || entry.Input == null) continue;

            var value = $"{entry.Input}::{entry.ChannelIndex ?? 0}";
            playbackSelection[ch] = value;
            alsaChannelByActiveInputValue[value] = ch;
        }

        // Column 4: which output (and its channel) is currently reading from this ALSA Capture
        // channel - the mirror image of column 1, found by scanning every active map entry for one
        // whose input is a known "ALSA Capture N" resource.
        var alsaChannelByCaptureUuid = captureUuid.ToDictionary(kv => kv.Value, kv => kv.Key);
        var captureSelection = channels.ToDictionary(ch => ch, _ => "");
        var alsaChannelByOutputChannelKey = new Dictionary<string, int>();
        foreach (var (outputId, chMap) in map)
        {
            foreach (var (chIndexStr, entry) in chMap)
            {
                if (entry.Input == null || !alsaChannelByCaptureUuid.TryGetValue(entry.Input, out var n))
                {
                    continue;
                }
                captureSelection[n] = $"{outputId}::{chIndexStr}";
                alsaChannelByOutputChannelKey[$"{outputId}::{chIndexStr}"] = n;
            }
        }

        // Fallback when a row's real current ALSA channel can't be found in the active map (e.g.
        // a resource that was just created and hasn't been activated onto any real channel yet) -
        // the first known channel keeps the <select> populated with a real option rather than an
        // unmatched value.
        var fallbackChannel = channels.Count > 0 ? channels[0] : 0;

        var rxChannels = rxRowSeeds
            .Select(seed => new RxChannelRow(
                seed.Value,
                seed.Label,
                seed.SinkId,
                alsaChannelByActiveInputValue.GetValueOrDefault(seed.Value, fallbackChannel)))
            .ToList();

        var txChannels = txRowSeeds
            .Select(seed => new TxChannelRow(
                seed.Value,
                seed.Label,
                seed.SourceId,
                alsaChannelByOutputChannelKey.GetValueOrDefault(seed.Value, fallbackChannel)))
            .ToList();

        return new ChannelMapView
        {
            Channels = channels,
            CaptureUuid = captureUuid,
            PlaybackUuid = playbackUuid,
            InputSinkIds = inputSinkIds,
            InputOptions = inputOptions,
            TxOptions = txOptions,
            CaptureOptions = captureOptions,
            PlaybackSelection = playbackSelection,
            CaptureSelection = captureSelection,
            RxChannels = rxChannels,
            TxChannels = txChannels,
        };
    }
}
