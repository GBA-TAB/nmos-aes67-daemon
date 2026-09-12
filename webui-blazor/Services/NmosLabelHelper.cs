namespace AesDaemonWebUI.Services;

// Mirrors nmos_manager.cpp's make_nmos_label(map) exactly: "<hostname> ALSA <first>-<last>"
// (1-indexed channel numbers), so the daemon's own Sources/Sinks tables can show the same name
// that appears for this resource in the NMOS registry/orchestrator, without a new API endpoint -
// the daemon's own hostname is already reported as DaemonConfig.NmosLabel.
public static class NmosLabelHelper
{
    public static string MakeLabel(string hostname, List<int> map)
    {
        if (map.Count == 0)
        {
            return $"{hostname} ALSA";
        }
        return $"{hostname} ALSA {map[0] + 1}-{map[^1] + 1}";
    }
}
