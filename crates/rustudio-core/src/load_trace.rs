//! RUSTUDIO_LOAD_TRACE=1: where a (headless) load spends its time. Prints
//! `LOADTRACE <seconds since the first rustudio process started> <what>` to
//! stderr: process starts (the headless wrapper hands its start time to the
//! child in RUSTUDIO_LOAD_T0), the place read / parsed / opened, the first
//! frame, Play's DataModels, every frame over 100 ms with its biggest bench
//! sections, and the asset queues each time they change.

use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("RUSTUDIO_LOAD_TRACE").is_some_and(|v| v != "0"))
}

fn epoch() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

/// The first process's start (seconds since the epoch).
fn t0() -> f64 {
    static T0: OnceLock<f64> = OnceLock::new();
    *T0.get_or_init(|| {
        std::env::var("RUSTUDIO_LOAD_T0").ok().and_then(|s| s.parse().ok()).unwrap_or_else(epoch)
    })
}

/// At process start: fixes t0 and passes it on to child processes.
pub fn init() {
    if !on() {
        return;
    }
    let t = t0();
    // SAFETY: set before any other thread starts (first thing in main).
    std::env::set_var("RUSTUDIO_LOAD_T0", format!("{t:.6}"));
    let who = if std::env::var_os("RUSTUDIO_HEADLESS_CHILD").is_some() { "child" } else { "main" };
    mark(&format!("process start ({who}, pid {})", std::process::id()));
}

pub fn mark(what: &str) {
    if on() {
        eprintln!("LOADTRACE {:8.3} {what}", epoch() - t0());
    }
}

/// At the start of each frame, for the frame before (`ms` = the time
/// between the two starts): one over 100 ms is printed with the bench
/// sections that grew most during it.
pub fn frame(frame_no: u64, ms: f64) {
    if !on() {
        return;
    }
    use std::sync::Mutex;
    static PREV: Mutex<Vec<(&'static str, f64)>> = Mutex::new(Vec::new());
    let mut prev = PREV.lock().unwrap_or_else(|e| e.into_inner());
    let before = std::mem::replace(&mut *prev, crate::bench::totals());
    let after = &*prev;
    static FIRST: OnceLock<()> = OnceLock::new();
    if FIRST.set(()).is_ok() {
        mark("first frame");
    }
    if ms < 100.0 {
        return;
    }
    let mut grew: Vec<(&str, f64)> = after
        .iter()
        .map(|(k, v)| (*k, v - before.iter().find(|(b, _)| b == k).map(|b| b.1).unwrap_or(0.0)))
        .filter(|(_, d)| *d > 5.0)
        .collect();
    grew.sort_by(|a, b| b.1.total_cmp(&a.1));
    let top: Vec<String> = grew.iter().take(8).map(|(k, d)| format!("{k}={d:.0}")).collect();
    mark(&format!("slow frame {frame_no}: {ms:.0} ms | {}", top.join(" ")));
}

/// The asset queues, printed when they change (at most every 250 ms).
pub fn assets(meshes: usize, textures_pending: usize, textures: usize) {
    if !on() {
        return;
    }
    use std::sync::Mutex;
    static LAST: Mutex<(usize, usize, usize, f64)> = Mutex::new((usize::MAX, usize::MAX, usize::MAX, 0.0));
    let mut l = LAST.lock().unwrap_or_else(|e| e.into_inner());
    let now = epoch();
    if (l.0, l.1, l.2) != (meshes, textures_pending, textures) && now - l.3 >= 0.25 {
        *l = (meshes, textures_pending, textures, now);
        mark(&format!("assets: meshes {meshes}, textures {textures} (+{textures_pending} decoding)"));
    }
}
