//! Automated sequential audio-path check (SESSION-2026-09-16-AUDIO-PATH-CHECK-DESIGN.md). Walks
//! the *entire* routing graph -- input grid -> track-in patch -> track send -> bus -> master ->
//! output grid patch -- by passively listening to the plain `amixer` WebSocket protocol (the same
//! one `ws.rs` serves and the dashboard already speaks), and reports every hop of every discovered
//! input-to-output path, sequentially, with real dB values.
//!
//! A standalone binary in this crate on purpose: it only needs the plain JSON wire protocol, not
//! any of `mxl-test-app`'s own internal types, so it doesn't need a `[lib]` target added.
//!
//! Passive by design -- never PUTs anything, safe to run against a live instance at any time. See
//! the design doc's own "Isolation" section for the documented-but-not-yet-built `--isolate` mode
//! this doesn't attempt.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use clap::Parser;
use futures_util::StreamExt;
use serde_json::Value;

#[derive(Parser, Debug)]
#[command(about = "Walks the full input-grid -> output-grid routing graph and checks signal presence at every hop, sequentially.")]
struct Args {
    #[arg(long, default_value = "127.0.0.1")]
    host: String,
    #[arg(long, default_value_t = 3214)]
    port: u16,
    #[arg(long, default_value_t = 0)]
    mixer_id: u32,
    /// A meter reading at or above this (dBFS) counts as "signal present".
    #[arg(long, default_value_t = -50.0)]
    threshold_db: f64,
    /// How long to listen to the periodic broadcast before building the graph -- needs at least
    /// one full tick (meter_hz-dependent) to have seen every resource's current patch/meter state.
    #[arg(long, default_value_t = 2.5)]
    listen_secs: f64,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Point {
    Input(String),
    TrackIn(u32),
    TrackOut(u32),
    BusIn(u32),
    BusOut(u32),
    MasterIn(u32),
    MasterOut(u32),
    Output(String),
}

impl Point {
    /// The WS path whose broadcast value is this point's own live meter -- `None` for a plain
    /// "In"/"Out" passthrough point that has no meter of its own distinct from its pair (e.g.
    /// `TrackIn`'s own reading *is* `channel/{id}/input-meter`, already covered by `TrackIn` itself
    /// so `TrackOut` doesn't re-check the same value under a different name).
    fn meter_path(&self, mixer_id: u32) -> Option<String> {
        match self {
            Point::Input(id) => Some(format!("amixer/{mixer_id}/input/{id}/peakmeter")),
            Point::TrackIn(id) => Some(format!("amixer/{mixer_id}/channel/{id}/input-meter")),
            Point::TrackOut(id) => Some(format!("amixer/{mixer_id}/channel/{id}/peakmeter")),
            Point::BusIn(id) => Some(format!("amixer/{mixer_id}/sum/{id}/input-meter")),
            Point::BusOut(id) => Some(format!("amixer/{mixer_id}/sum/{id}/peakmeter")),
            Point::MasterIn(id) => Some(format!("amixer/{mixer_id}/master/{id}/input-meter")),
            Point::MasterOut(id) => Some(format!("amixer/{mixer_id}/master/{id}/peakmeter")),
            Point::Output(id) => Some(format!("amixer/{mixer_id}/output/{id}/peakmeter")),
        }
    }

    fn label(&self) -> String {
        match self {
            Point::Input(id) => format!("input:{id}"),
            Point::TrackIn(id) => format!("channel:{id} (in)"),
            Point::TrackOut(id) => format!("channel:{id} (out)"),
            Point::BusIn(id) => format!("sum:{id} (in)"),
            Point::BusOut(id) => format!("sum:{id} (out)"),
            Point::MasterIn(id) => format!("master:{id} (in)"),
            Point::MasterOut(id) => format!("master:{id} (out)"),
            Point::Output(id) => format!("output:{id}"),
        }
    }

    fn is_sink(&self) -> bool {
        matches!(self, Point::Output(_))
    }
}

/// Parses a `SourceRef::to_json` value (`{"source": "...", "channel": N}`) into the `Point` it
/// names -- the inverse of `patch.rs`'s own `SourceRef::to_json`/`parse`, kept here rather than
/// shared since this binary has no access to `mxl-test-app`'s internal types.
fn source_point(v: &Value) -> Option<Point> {
    let source = v.get("source")?.as_str()?;
    if let Some(id) = source.strip_prefix("input:") {
        Some(Point::Input(id.to_string()))
    } else if let Some(id) = source.strip_prefix("track-out:") {
        Some(Point::TrackOut(id.parse().ok()?))
    } else if let Some(id) = source.strip_prefix("bus-out:") {
        Some(Point::BusOut(id.parse().ok()?))
    } else if let Some(id) = source.strip_prefix("master-out:") {
        Some(Point::MasterOut(id.parse().ok()?))
    } else {
        None
    }
}

/// Every real source point referenced anywhere in an exclusive-destination patch array
/// (`[{"source",...} | null, ...]`, one slot per channel -- track-in/output's own shape).
fn exclusive_sources(v: &Value) -> Vec<Point> {
    v.as_array().into_iter().flatten().filter_map(source_point).collect()
}

/// Every real source point referenced anywhere in a summing-destination patch array
/// (`[[{"source",...}, ...], ...]`, one array of sources per channel -- bus-in/master-in's shape).
fn summing_sources(v: &Value) -> Vec<Point> {
    v.as_array().into_iter().flatten().flat_map(|slot| slot.as_array().into_iter().flatten().filter_map(source_point)).collect()
}

struct Snapshot {
    latest: HashMap<String, Value>,
}

impl Snapshot {
    fn get(&self, path: &str) -> Option<&Value> {
        self.latest.get(path)
    }

    fn ids(&self, list_path: &str) -> Vec<Value> {
        self.get(list_path).and_then(|v| v.as_array()).cloned().unwrap_or_default()
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let url = format!("ws://{}:{}/amixer/api/socket", args.host, args.port);
    println!("Connecting to {url} ...");

    let (ws, _) = tokio_tungstenite::connect_async(&url).await?;
    let (_write, mut read) = ws.split();

    let mut latest: HashMap<String, Value> = HashMap::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs_f64(args.listen_secs);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, read.next()).await {
            Ok(Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text)))) => {
                if let Ok(msg) = serde_json::from_str::<Value>(&text) {
                    if let Some(path) = msg.get("path").and_then(|p| p.as_str()) {
                        latest.insert(path.to_string(), msg.get("value").cloned().unwrap_or(Value::Null));
                    }
                }
            }
            Ok(Some(Ok(_))) => {}
            Ok(Some(Err(e))) => {
                eprintln!("WS error: {e}");
                break;
            }
            Ok(None) => break,
            Err(_) => break, // timeout, listen window is up
        }
    }
    println!("Captured {} distinct paths over {:.1}s.\n", latest.len(), args.listen_secs);
    let snap = Snapshot { latest };

    // --- Build the graph -----------------------------------------------------------------------
    let mut edges: HashMap<Point, Vec<Point>> = HashMap::new();
    // Deduplicates on insert -- a multi-channel patch (e.g. an 8-channel track patched 1:1 from
    // one 8-channel input entry) names the same (source, dest) pair once per channel; without this
    // the BFS below would branch into N identical paths for N channels sharing one real source.
    let add_edge = |from: Point, to: Point, edges: &mut HashMap<Point, Vec<Point>>| {
        let dests = edges.entry(from).or_default();
        if !dests.contains(&to) {
            dests.push(to);
        }
    };

    let mixer_id = args.mixer_id;
    let track_ids: Vec<u32> = snap.ids(&format!("amixer/{mixer_id}/channel-list")).iter().filter_map(|c| c.get("id")?.as_u64()).map(|n| n as u32).collect();
    let bus_ids: Vec<u32> = snap.ids(&format!("amixer/{mixer_id}/sum-list")).iter().filter_map(|c| c.get("id")?.as_u64()).map(|n| n as u32).collect();
    let master_ids: Vec<u32> = snap.ids(&format!("amixer/{mixer_id}/master-list")).iter().filter_map(|c| c.get("id")?.as_u64()).map(|n| n as u32).collect();
    let input_ids: Vec<String> = snap.ids(&format!("amixer/{mixer_id}/input-grid")).iter().filter_map(|e| e.get("id")?.as_str().map(str::to_string)).collect();
    let output_ids: Vec<String> = snap.ids(&format!("amixer/{mixer_id}/output-grid")).iter().filter_map(|e| e.get("id")?.as_str().map(str::to_string)).collect();

    for &id in &track_ids {
        // input-patch (exclusive): every real source -> this track's own TrackIn.
        if let Some(v) = snap.get(&format!("amixer/{mixer_id}/channel/{id}/input-patch")) {
            for src in exclusive_sources(v) {
                add_edge(src, Point::TrackIn(id), &mut edges);
            }
        }
        // A track's own input always reaches its own output (gain/chain/fader don't remove signal
        // for this tool's purposes -- mute isn't modeled, same "presence" granularity as the
        // design doc's own scope).
        add_edge(Point::TrackIn(id), Point::TrackOut(id), &mut edges);
        // sends: this track's own output -> every bus it sends to. Goes straight to that bus's
        // own BusOut, *not* through BusIn -- a send and a direct bus-in patch are two independent
        // contributors to bus-out (PICKOFFS.md), not sequential stages, and only the bus-in patch
        // is what sum/{id}/input-meter actually measures. Routing a send through BusIn would make
        // this tool check the wrong meter and report a real, working send-only bus as broken.
        if let Some(v) = snap.get(&format!("amixer/{mixer_id}/channel/{id}/sends")) {
            for send in v.as_array().into_iter().flatten() {
                if let Some(bus_id) = send.get("bus_id").and_then(|v| v.as_u64()) {
                    add_edge(Point::TrackOut(id), Point::BusOut(bus_id as u32), &mut edges);
                }
            }
        }
    }
    for &id in &bus_ids {
        // Only a *direct* bus-in patch entry reaches BusIn -- see the send-routing comment above
        // for why sends bypass this node entirely.
        if let Some(v) = snap.get(&format!("amixer/{mixer_id}/sum/{id}/input-patch")) {
            for src in summing_sources(v) {
                add_edge(src, Point::BusIn(id), &mut edges);
            }
        }
        add_edge(Point::BusIn(id), Point::BusOut(id), &mut edges);
    }
    for &id in &master_ids {
        if let Some(v) = snap.get(&format!("amixer/{mixer_id}/master/{id}/input-patch")) {
            for src in summing_sources(v) {
                add_edge(src, Point::MasterIn(id), &mut edges);
            }
        }
        add_edge(Point::MasterIn(id), Point::MasterOut(id), &mut edges);
    }
    for id in &output_ids {
        if let Some(v) = snap.get(&format!("amixer/{mixer_id}/output/{id}/patch")) {
            for src in exclusive_sources(v) {
                add_edge(src, Point::Output(id.clone()), &mut edges);
            }
        }
    }

    // --- Walk every path from every real input, sequentially, reporting each in full -----------
    let mut any_path = false;
    let mut any_fail = false;
    for input_id in &input_ids {
        let start = Point::Input(input_id.clone());
        let mut stack = vec![vec![start.clone()]];
        let mut visited_from_here: HashSet<Point> = HashSet::new();
        while let Some(path) = stack.pop() {
            let last = path.last().unwrap().clone();
            if last.is_sink() {
                any_path = true;
                if !check_and_report_path(&path, &snap, mixer_id, args.threshold_db) {
                    any_fail = true;
                }
                continue;
            }
            let Some(next_points) = edges.get(&last) else { continue };
            for next in next_points {
                if path.contains(next) {
                    continue; // cycle guard (a bus/master feeding itself, allowed by the mixer, not a new path here)
                }
                let mut extended = path.clone();
                extended.push(next.clone());
                visited_from_here.insert(next.clone());
                stack.push(extended);
            }
        }
    }

    if !any_path {
        // No complete route at all is just as much a "not working" outcome as a patched-but-silent
        // one, from the operator's own "is my routing correct" perspective this tool exists to
        // answer -- exit non-zero here too, not just when a discovered path fails its meters.
        println!("No complete input -> output paths discovered. Nothing is patched all the way through yet.");
        std::process::exit(1);
    } else if any_fail {
        println!("=== One or more paths are broken. See above. ===");
        std::process::exit(1);
    } else {
        println!("=== All discovered paths OK. ===");
    }
    Ok(())
}

fn peak_db(v: Option<&Value>) -> Option<f64> {
    let arr = v?.as_array()?;
    arr.iter().filter_map(|c| c.as_f64()).fold(None, |acc, x| Some(acc.map_or(x, |a: f64| a.max(x))))
}

fn check_and_report_path(path: &[Point], snap: &Snapshot, mixer_id: u32, threshold_db: f64) -> bool {
    let names: Vec<String> = path.iter().map(Point::label).collect();
    println!("Path: {}", names.join(" -> "));
    let mut ok = true;
    let mut upstream_broken = false;
    for point in path {
        let Some(meter_path) = point.meter_path(mixer_id) else { continue };
        let value = snap.get(&meter_path);
        let db = peak_db(value);
        if upstream_broken {
            println!("  [SKIP] {:<38} (upstream already silent)", meter_path);
            continue;
        }
        match db {
            Some(db) if db >= threshold_db => println!("  [PASS] {:<38} {db:.1} dBFS", meter_path),
            Some(db) => {
                println!("  [FAIL] {:<38} {db:.1} dBFS (below {threshold_db:.1} dBFS threshold)", meter_path);
                ok = false;
                upstream_broken = true;
            }
            None => {
                println!("  [FAIL] {:<38} silent or never broadcast", meter_path);
                ok = false;
                upstream_broken = true;
            }
        }
    }
    if ok {
        println!("  => PATH OK ({} hops confirmed)\n", path.len());
    } else {
        println!("  => PATH BROKEN\n");
    }
    ok
}
