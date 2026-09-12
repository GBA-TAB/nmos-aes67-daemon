using System.Net.Http.Json;
using System.Text.Json;
using AesDaemonWebUI.Models;
using Microsoft.AspNetCore.Components;

namespace AesDaemonWebUI.Services;

// Typed client for the daemon's IS-08 (NMOS Channel Mapping) API - a real architectural wrinkle
// found while porting Services.js: unlike every other endpoint (same-origin /api/*), this hits a
// SECOND origin, http://{hostname}:{nmos_node_port}, only known after reading nmos_node_port from
// DaemonApiClient's own GET /api/config - not something a DI-registered HttpClient can have a
// fixed BaseAddress for. EnsureInitializedAsync must be called (with the already-resolved config)
// before any other method - every page that uses this client calls it once in OnInitializedAsync,
// same as the original resolving getNmosBaseUrl() before every IS-08 call.
//
// Uses the browser's own current hostname (via NavigationManager, not JS interop) with the port
// swapped to nmos_node_port - matches the original's window.location.hostname-based resolution
// rather than trusting the daemon's own configured ip_addr, which may differ from whatever
// network path the browser actually used to reach it.
public class ChannelMappingApiClient(HttpClient http, NavigationManager navigation)
{
    private bool _initialized;

    public void EnsureInitialized(DaemonConfig config)
    {
        if (_initialized)
        {
            return;
        }
        var host = new Uri(navigation.BaseUri).Host;
        http.BaseAddress = new Uri($"http://{host}:{config.NmosNodePort}");
        _initialized = true;
    }

    public Task<List<string>> GetInputIdsAsync(CancellationToken ct = default) =>
        GetIdListAsync("/x-nmos/channelmapping/v1.0/inputs/", ct);

    public Task<List<string>> GetOutputIdsAsync(CancellationToken ct = default) =>
        GetIdListAsync("/x-nmos/channelmapping/v1.0/outputs/", ct);

    public Task<Is08ResourceProperties> GetInputPropertiesAsync(string id, CancellationToken ct = default) =>
        GetAsync<Is08ResourceProperties>($"/x-nmos/channelmapping/v1.0/inputs/{id}/properties/", ct);

    public Task<Is08ResourceProperties> GetOutputPropertiesAsync(string id, CancellationToken ct = default) =>
        GetAsync<Is08ResourceProperties>($"/x-nmos/channelmapping/v1.0/outputs/{id}/properties/", ct);

    public Task<List<Is08Channel>> GetInputChannelsAsync(string id, CancellationToken ct = default) =>
        GetAsync<List<Is08Channel>>($"/x-nmos/channelmapping/v1.0/inputs/{id}/channels/", ct);

    public Task<List<Is08Channel>> GetOutputChannelsAsync(string id, CancellationToken ct = default) =>
        GetAsync<List<Is08Channel>>($"/x-nmos/channelmapping/v1.0/outputs/{id}/channels/", ct);

    // Ground truth for "what is actually patched right now" - reading this directly (rather than
    // re-deriving the same fact from /api/sinks and /api/sources' map[] arrays matched to IS-08
    // resources by label) is what fixes real staleness: two sinks/sources whose ALSA channel
    // ranges happen to overlap can otherwise make that indirect derivation pick the wrong
    // "currently selected" value, while this endpoint is authoritative and immune to that drift.
    public Task<Is08ActiveMapResponse> GetActiveMapAsync(CancellationToken ct = default) =>
        GetAsync<Is08ActiveMapResponse>("/x-nmos/channelmapping/v1.0/map/active/", ct);

    // Applies one channel-map activation: outputId/outputChannel -> inputId/inputChannel (either
    // null to unmap that output channel entirely). Matches the original's single-assignment POST
    // shape exactly (ChannelMap.jsx never batches more than one change per activation).
    public async Task ActivateAsync(string outputId, int outputChannel, string? inputId, int? inputChannel, CancellationToken ct = default)
    {
        var body = new Is08MapActivation
        {
            Action = new()
            {
                [outputId] = new()
                {
                    [outputChannel.ToString()] = new Is08ChannelAssignment { Input = inputId, ChannelIndex = inputChannel },
                },
            },
        };
        using var response = await http.PostAsJsonAsync("/x-nmos/channelmapping/v1.0/map/activations/", body, ct);
        await EnsureSuccessAsync(response, ct);
    }

    private async Task<List<string>> GetIdListAsync(string path, CancellationToken ct)
    {
        // The IS-08 list endpoints return ["<uuid>/", ...] (trailing slash per NMOS convention) -
        // strip it so callers work with plain ids, matching every other id-taking method here.
        var raw = await GetAsync<List<string>>(path, ct);
        return raw.Select(s => s.TrimEnd('/')).ToList();
    }

    private async Task<T> GetAsync<T>(string path, CancellationToken ct)
    {
        using var response = await http.GetAsync(path, ct);
        await EnsureSuccessAsync(response, ct);
        return await response.Content.ReadFromJsonAsync<T>(cancellationToken: ct)
            ?? throw new DaemonApiException((int)response.StatusCode, $"{path} returned an empty body");
    }

    private static async Task EnsureSuccessAsync(HttpResponseMessage response, CancellationToken ct)
    {
        if (response.IsSuccessStatusCode)
        {
            return;
        }
        // IS-08 error bodies are JSON {"error": "..."} (unlike /api/*'s plain text) - unwrap it,
        // matching Services.js's doFetchRaw special-casing of this shape.
        var body = await response.Content.ReadAsStringAsync(ct);
        var message = body;
        try
        {
            using var doc = JsonDocument.Parse(body);
            if (doc.RootElement.TryGetProperty("error", out var errorProp))
            {
                message = errorProp.GetString() ?? body;
            }
        }
        catch (JsonException)
        {
            // Not JSON - fall back to the raw body text.
        }
        throw new DaemonApiException((int)response.StatusCode, message);
    }
}
