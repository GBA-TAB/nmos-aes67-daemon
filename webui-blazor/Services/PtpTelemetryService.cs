using AesDaemonWebUI.Models;

namespace AesDaemonWebUI.Services;

// Owns PTP status polling and jitter history for the whole app session, not just while the PTP
// page happens to be mounted - registered as a singleton (resolved eagerly in Program.cs) so
// navigating away from /PTP and back doesn't reset the jitter chart to empty, and polling keeps
// running even while another tab is active. Blazor WASM has exactly one app instance per browser
// tab, so a singleton here means "lives for this tab's session" - which is what a returning user
// expects: the history picks up where it left off.
public class PtpTelemetryService : IAsyncDisposable
{
    private const int JitterHistoryLimit = 60;
    private const int PollSeconds = 5;

    private readonly DaemonApiClient _api;
    private readonly List<int> _jitterHistory = new();
    private readonly CancellationTokenSource _cts = new();
    private readonly Task _pollTask;
    private PeriodicTimer? _timer;

    public PtpStatus Status { get; private set; } = new()
    {
        Status = "",
        Gmid = "",
        Leg0Status = "unlocked",
        Leg1Status = "unlocked",
        Leg0Gmid = "",
        Leg1Gmid = "",
        LegsAligned = true,
    };

    public bool HasStatus { get; private set; }

    public IReadOnlyList<int> JitterHistory => _jitterHistory;

    public event Action? Changed;

    public PtpTelemetryService(DaemonApiClient api)
    {
        _api = api;
        _pollTask = PollLoop();
    }

    private async Task PollLoop()
    {
        await Poll();
        _timer = new PeriodicTimer(TimeSpan.FromSeconds(PollSeconds));
        try
        {
            while (await _timer.WaitForNextTickAsync(_cts.Token))
            {
                await Poll();
            }
        }
        catch (OperationCanceledException)
        {
            // Expected on DisposeAsync - the app is shutting down.
        }
    }

    private async Task Poll()
    {
        try
        {
            Status = await _api.GetPtpStatusAsync(_cts.Token);
            HasStatus = true;
            _jitterHistory.Add(Status.Jitter);
            if (_jitterHistory.Count > JitterHistoryLimit)
            {
                _jitterHistory.RemoveAt(0);
            }
            Changed?.Invoke();
        }
        catch (DaemonApiException)
        {
            // Leave the last-known status/history in place rather than blanking it on one failed poll.
        }
        catch (OperationCanceledException)
        {
        }
    }

    public async ValueTask DisposeAsync()
    {
        _cts.Cancel();
        _timer?.Dispose();
        try
        {
            await _pollTask;
        }
        catch (OperationCanceledException)
        {
        }
        _cts.Dispose();
    }
}
