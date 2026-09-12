namespace AesDaemonWebUI.Services;

// Thrown by DaemonApiClient/ChannelMappingApiClient on any non-2xx response, carrying the
// daemon's own real error body text (format "<action> : (<category>) <message>" for /api/*,
// verified live e.g. "failed to get sink 999 status : (daemon) invalid stream id"; IS-08 errors
// are JSON {"error": "..."} instead - ChannelMappingApiClient unwraps that before throwing this).
// Mirrors Services.js's own toast-on-error behavior: callers catch this and show Message to the
// user instead of a generic "request failed".
public class DaemonApiException(int statusCode, string message) : Exception(message)
{
    public int StatusCode { get; } = statusCode;
}
