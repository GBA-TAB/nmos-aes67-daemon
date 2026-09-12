using AesDaemonWebUI;
using AesDaemonWebUI.Services;
using Microsoft.AspNetCore.Components.Web;
using Microsoft.AspNetCore.Components.WebAssembly.Hosting;

var builder = WebAssemblyHostBuilder.CreateDefault(args);
builder.RootComponents.Add<App>("#app");
builder.RootComponents.Add<HeadOutlet>("head::after");

// Same-origin client for the daemon's own /api/* surface - BaseAddress is fixed at the app's own
// origin, matching how it's actually deployed (served as static files by the same daemon whose
// API it calls).
builder.Services.AddScoped<DaemonApiClient>(_ =>
    new DaemonApiClient(new HttpClient { BaseAddress = new Uri(builder.HostEnvironment.BaseAddress) }));

// Separate client for the IS-08 Channel Mapping API - deliberately no fixed BaseAddress here (see
// ChannelMappingApiClient's own doc comment): it targets a second origin only known after reading
// nmos_node_port from the daemon's own config, resolved at runtime via EnsureInitialized.
builder.Services.AddScoped<ChannelMappingApiClient>(sp =>
    new ChannelMappingApiClient(new HttpClient(), sp.GetRequiredService<Microsoft.AspNetCore.Components.NavigationManager>()));

// App-session-lifetime PTP polling/jitter history - singleton so it survives navigating away from
// and back to the PTP page (see PtpTelemetryService's own doc comment).
builder.Services.AddSingleton<PtpTelemetryService>();

var host = builder.Build();

// Resolve eagerly so polling starts immediately at app launch, not on first visit to /PTP - the
// jitter history should reflect the whole session, not just time spent on that tab.
host.Services.GetRequiredService<PtpTelemetryService>();

await host.RunAsync();
