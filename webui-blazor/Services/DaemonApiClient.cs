using System.Net.Http.Json;
using AesDaemonWebUI.Models;

namespace AesDaemonWebUI.Services;

// Typed client for the daemon's own /api/* REST surface (daemon/http_server.cpp) - same-origin
// when served for real (the WASM app and the daemon's static-file serving share one origin), but
// CORS is wide open regardless (Access-Control-Allow-Origin: *, http_server.cpp:35-44) so this
// also works unmodified against a daemon on a different port during `dotnet watch` development.
//
// Deliberately only wraps the endpoints the original React app actually calls (Services.js) -
// /api/streams and the /api/streamer/* endpoints are real daemon routes but confirmed unused by
// the UI, so they're not ported here (see the migration plan's own note on this).
public class DaemonApiClient(HttpClient http)
{
    public async Task<string> GetVersionAsync(CancellationToken ct = default) =>
        (await GetAsync<VersionInfo>("/api/version", ct)).Version;

    public async Task<DaemonConfig> GetConfigAsync(CancellationToken ct = default) =>
        await GetAsync<DaemonConfig>("/api/config", ct);

    public async Task SetConfigAsync(ConfigUpdateRequest config, CancellationToken ct = default) =>
        await PostAsync("/api/config", config, ct);

    public async Task<PtpStatus> GetPtpStatusAsync(CancellationToken ct = default) =>
        await GetAsync<PtpStatus>("/api/ptp/status", ct);

    public async Task<PtpConfig> GetPtpConfigAsync(CancellationToken ct = default) =>
        await GetAsync<PtpConfig>("/api/ptp/config", ct);

    public async Task SetPtpConfigAsync(PtpConfig config, CancellationToken ct = default) =>
        await PostAsync("/api/ptp/config", config, ct);

    public async Task<List<SourceInfo>> GetSourcesAsync(CancellationToken ct = default) =>
        (await GetAsync<SourcesResponse>("/api/sources", ct)).Sources;

    public async Task<List<SinkInfo>> GetSinksAsync(CancellationToken ct = default) =>
        (await GetAsync<SinksResponse>("/api/sinks", ct)).Sinks;

    public async Task<string> GetSourceSdpAsync(int id, CancellationToken ct = default)
    {
        using var response = await http.GetAsync($"/api/source/sdp/{id}", ct);
        await EnsureSuccessAsync(response, ct);
        return await response.Content.ReadAsStringAsync(ct);
    }

    public async Task<SourceStatus> GetSourceStatusAsync(int id, CancellationToken ct = default) =>
        await GetAsync<SourceStatus>($"/api/source/status/{id}", ct);

    public async Task PutSourceAsync(int id, SourceInfo source, CancellationToken ct = default) =>
        await PutAsync($"/api/source/{id}", source, ct);

    public async Task DeleteSourceAsync(int id, CancellationToken ct = default) =>
        await DeleteAsync($"/api/source/{id}", ct);

    public async Task<SinkStatus> GetSinkStatusAsync(int id, CancellationToken ct = default) =>
        await GetAsync<SinkStatus>($"/api/sink/status/{id}", ct);

    public async Task PutSinkAsync(int id, SinkInfo sink, CancellationToken ct = default) =>
        await PutAsync($"/api/sink/{id}", sink, ct);

    public async Task DeleteSinkAsync(int id, CancellationToken ct = default) =>
        await DeleteAsync($"/api/sink/{id}", ct);

    public async Task<List<RemoteSource>> GetRemoteSourcesAsync(CancellationToken ct = default) =>
        (await GetAsync<RemoteSourcesResponse>("/api/browse/sources/all", ct)).RemoteSources;

    private async Task<T> GetAsync<T>(string path, CancellationToken ct)
    {
        using var response = await http.GetAsync(path, ct);
        await EnsureSuccessAsync(response, ct);
        return await response.Content.ReadFromJsonAsync<T>(cancellationToken: ct)
            ?? throw new DaemonApiException((int)response.StatusCode, $"{path} returned an empty body");
    }

    private async Task PostAsync<T>(string path, T body, CancellationToken ct)
    {
        using var response = await http.PostAsJsonAsync(path, body, ct);
        await EnsureSuccessAsync(response, ct);
    }

    private async Task PutAsync<T>(string path, T body, CancellationToken ct)
    {
        using var response = await http.PutAsJsonAsync(path, body, ct);
        await EnsureSuccessAsync(response, ct);
    }

    private async Task DeleteAsync(string path, CancellationToken ct)
    {
        using var response = await http.DeleteAsync(path, ct);
        await EnsureSuccessAsync(response, ct);
    }

    private static async Task EnsureSuccessAsync(HttpResponseMessage response, CancellationToken ct)
    {
        if (response.IsSuccessStatusCode)
        {
            return;
        }
        // Real error bodies are text/plain, "<action> : (<category>) <message>" - verified live
        // (http_server.cpp's set_error helpers). Surface that text directly rather than a generic
        // "request failed" message.
        var body = await response.Content.ReadAsStringAsync(ct);
        throw new DaemonApiException((int)response.StatusCode, string.IsNullOrWhiteSpace(body) ? response.ReasonPhrase ?? "request failed" : body);
    }
}
