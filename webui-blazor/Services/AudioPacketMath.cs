namespace AesDaemonWebUI.Services;

// Ports the packet-size/channel-count domain math from SourceEdit.jsx (getMaxChannels,
// checkMaxSamplesPerPacket, getMaxSamplesPerPacket, getPacketDuration, getnFS) as a small, pure
// static helper rather than inline page logic - this math is used only by the Source edit form.
public static class AudioPacketMath
{
    private const int MaxPacketSizeBytes = 1440;

    public static int GetMaxChannels(string codec, int samplesPerPacket)
    {
        var sampleSize = codec switch
        {
            "L16" => 2,
            "L24" => 3,
            _ => 4, // AM824
        };
        var maxChannels = MaxPacketSizeBytes / (samplesPerPacket * sampleSize);
        return maxChannels > 64 ? 64 : maxChannels;
    }

    // The 1FS multiplier for a given sample rate - 8x at DXF (384k/352.8k), 4x at 4FS, 2x at 2FS,
    // 1x at 1FS (48k/44.1k, and the default for any other rate).
    public static int GetNfs(int sampleRate) => sampleRate switch
    {
        384000 or 352800 => 8,
        192000 or 176400 => 4,
        96000 or 88200 => 2,
        _ => 1,
    };

    public static bool CheckMaxSamplesPerPacket(int samples, int ticFrameSizeAt1Fs, int sampleRate) =>
        samples <= ticFrameSizeAt1Fs * GetNfs(sampleRate);

    // Clamps a source's stored max_samples_per_packet to what the current TIC frame size actually
    // allows - a source loaded from a daemon running at a smaller frame size than when it was
    // originally configured must not offer an out-of-range initial selection.
    public static int GetMaxSamplesPerPacket(int storedMaxSamplesPerPacket, int ticFrameSizeAt1Fs, int sampleRate)
    {
        var limit = ticFrameSizeAt1Fs * GetNfs(sampleRate);
        return storedMaxSamplesPerPacket > limit ? limit : storedMaxSamplesPerPacket;
    }

    // Finds the first contiguous block of `channelsNeeded` unused channels (0-63) given the
    // channel maps already in use by other sources/sinks - e.g. if channels 0-7 (displayed as
    // "ALSA Output 1"-"8") are already taken, this returns 8, so a newly added stream starts at
    // the next free channel instead of a fixed id-based slot that can collide with what's already
    // configured. Falls back to 0 if no free block of that size exists.
    public static int FindNextFreeChannelStart(IEnumerable<IEnumerable<int>> existingMaps, int channelsNeeded)
    {
        var used = new HashSet<int>(existingMaps.SelectMany(m => m));
        for (var start = 0; start <= 64 - channelsNeeded; start++)
        {
            if (Enumerable.Range(start, channelsNeeded).All(c => !used.Contains(c)))
            {
                return start;
            }
        }
        return 0;
    }

    public static string GetPacketDuration(int samples, int sampleRate)
    {
        double duration = samples * 1_000_000.0 / sampleRate;
        if (duration >= 1000)
        {
            duration /= 1000;
            var rounded = Math.Round(duration);
            return duration == rounded
                ? $"{rounded}ms"
                : $"{Math.Round(duration * 1000) / 1000}ms";
        }
        return $"{Math.Round(duration)}μs";
    }
}
