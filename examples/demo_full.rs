//! onebox_lifecycle — full-featured continuous demo
//!
//! Tests all features listed in the matrix:
//!
//! | Feature            | Windows | macOS |
//! |--------------------|---------|-------|
//! | Shutdown blocking  | ✓       | ✓     |
//! | Sleep / wake       | ✓       | ✓     |
//! | Network up / down  | ✓       | ✓     |
//! | Async cleanup      | ✓       | ✓     |
//!
//! Run with:
//!   cargo run --example demo_full
//!
//! Then:
//!   • Sleep / wake the machine to see WillSleep / DidWake
//!   • Toggle Wi-Fi or unplug Ethernet to see NetworkUp / NetworkDown
//!   • Trigger shutdown/restart to see ShuttingDown + 5-second async cleanup
//!   • Press Ctrl+C to exit normally

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use onebox_lifecycle::{Sentinel, SystemEvent};

// ─── Global log file (append, O_SYNC-equivalent via explicit flush) ───────────

static LOG_FILE: OnceLock<Mutex<File>> = OnceLock::new();

/// Open (or create) the log file in append mode.
/// Path: ~/onebox_lifecycle_demo.log  — survives reboots.
fn init_log_file() -> std::path::PathBuf {
    let path = dirs_home().join("onebox_lifecycle_demo.log");
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .expect("cannot open log file");
    LOG_FILE.set(Mutex::new(file)).ok();
    path
}

/// Resolve the user's home directory without pulling in extra crates.
fn dirs_home() -> std::path::PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("."))
}

// ─── Logging helpers ─────────────────────────────────────────────────────────

fn timestamp() -> String {
    // Full ISO-8601-ish timestamp so the log file is unambiguous across reboots.
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let total_s = ms / 1000;
    // Days since epoch → calendar date (Gregorian, good for ~year 9999)
    let (y, mo, d) = days_to_ymd((total_s / 86400) as u32);
    let s = total_s % 86400;
    let h = s / 3600;
    let m = (s % 3600) / 60;
    let sec = s % 60;
    let frac = ms % 1000;
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{m:02}:{sec:02}.{frac:03}")
}

/// Tomohiko Sakamoto's algorithm — days since Unix epoch → (year, month, day).
fn days_to_ymd(days: u32) -> (u32, u32, u32) {
    let z = days + 719468;
    let era = z / 146097;
    let doe = z % 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };
    (y, mo, d)
}

/// Write one log line to both stdout and the log file (flushed immediately).
fn emit(tag: &str, msg: &str) {
    let line = format!("[{}] [{:<8}] {}\n", timestamp(), tag, msg);
    print!("{line}");
    if let Some(mutex) = LOG_FILE.get() {
        if let Ok(mut f) = mutex.lock() {
            let _ = f.write_all(line.as_bytes());
            let _ = f.flush(); // ensure the line survives a sudden power-off
        }
    }
}

macro_rules! log {
    ($tag:expr, $($arg:tt)*) => {
        emit($tag, &format!($($arg)*))
    };
}

// ─── Platform string ─────────────────────────────────────────────────────────

fn platform() -> &'static str {
    if cfg!(target_os = "macos") {
        "macOS"
    } else if cfg!(target_os = "windows") {
        "Windows"
    } else {
        "unsupported"
    }
}

// ─── Async cleanup simulation ─────────────────────────────────────────────────

/// Simulate multi-step async cleanup work.
/// In a real app this would flush write buffers, close DB connections, etc.
async fn async_cleanup(event_counter: Arc<AtomicU64>) {
    let total_events = event_counter.load(Ordering::Relaxed);
    log!("CLEANUP", "Beginning async cleanup (total events seen so far: {})", total_events);

    log!("CLEANUP", "Step 1/4 — flushing write buffers …");
    tokio::time::sleep(Duration::from_millis(800)).await;
    log!("CLEANUP", "Step 1/4 — done");

    log!("CLEANUP", "Step 2/4 — closing database connections …");
    tokio::time::sleep(Duration::from_millis(1200)).await;
    log!("CLEANUP", "Step 2/4 — done");

    log!("CLEANUP", "Step 3/4 — draining message queues …");
    tokio::time::sleep(Duration::from_millis(1500)).await;
    log!("CLEANUP", "Step 3/4 — done");

    log!("CLEANUP", "Step 4/4 — persisting application state …");
    tokio::time::sleep(Duration::from_millis(600)).await;
    log!("CLEANUP", "Step 4/4 — done");

    log!("CLEANUP", "All cleanup steps finished. Signalling OS to proceed.");
}

// ─── Main ─────────────────────────────────────────────────────────────────────

fn main() {
    // Build a Tokio runtime for async cleanup tasks.
    // We keep it alive for the duration of the process so spawned tasks can finish.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("failed to build tokio runtime");

    // Counter shared between event loop and async tasks.
    let event_counter: Arc<AtomicU64> = Arc::new(AtomicU64::new(0));

    // ── Log file ──────────────────────────────────────────────────────────────
    let log_path = init_log_file();

    // ── Banner ────────────────────────────────────────────────────────────────
    let sep = "─".repeat(64);
    println!("{sep}");
    println!("  onebox_lifecycle — full demo");
    println!("  Platform : {}", platform());
    println!("  PID      : {}", std::process::id());
    println!("  Log file : {}", log_path.display());
    println!("{sep}");
    println!("  Triggers to try:");
    println!("    • Sleep / wake the machine       → WillSleep / DidWake");
    println!("    • Toggle Wi-Fi / unplug cable    → NetworkDown / NetworkUp");
    println!("    • Shutdown / restart             → ShuttingDown + async cleanup");
    println!("    • Ctrl-C                         → exit immediately");
    println!("{sep}");
    println!();

    // Write a session-start marker so multiple runs are clearly separated.
    emit("SESSION", &format!(
        "════ NEW SESSION  pid={}  platform={}  log={} ════",
        std::process::id(), platform(), log_path.display()
    ));

    log!("INIT", "Starting Sentinel …");

    // NOTE: on macOS this must be called from the main thread.
    let sentinel = Sentinel::start();
    let start = Instant::now();

    log!("INIT", "Sentinel running. Waiting for system events …");
    log!("INIT", "Network initial state will be reported momentarily.");
    println!();

    // ── Event loop ────────────────────────────────────────────────────────────
    while let Some(event) = sentinel.recv() {
        let n = event_counter.fetch_add(1, Ordering::Relaxed) + 1;
        let uptime = start.elapsed();

        match event {
            // ── Sleep / wake ──────────────────────────────────────────────────
            SystemEvent::WillSleep => {
                log!(
                    "SLEEP",
                    "#{n} System is going to sleep  (uptime {:.1}s)",
                    uptime.as_secs_f64()
                );
                log!("SLEEP", "  → Return from sleep quickly; there is very little time.");
            }

            SystemEvent::DidWake => {
                log!(
                    "WAKE",
                    "#{n} System resumed from sleep  (uptime {:.1}s)",
                    uptime.as_secs_f64()
                );
                log!("WAKE", "  → Re-establish connections, refresh caches, etc.");
            }

            // ── Network ───────────────────────────────────────────────────────
            SystemEvent::NetworkUp => {
                log!(
                    "NET↑",
                    "#{n} Network is UP — at least one interface is reachable"
                );
                log!("NET↑", "  → Safe to reconnect to remote services.");
            }

            SystemEvent::NetworkDown => {
                log!(
                    "NET↓",
                    "#{n} Network is DOWN — all interfaces gone or unreachable"
                );
                log!("NET↓", "  → Queue outgoing requests; switch to offline mode.");
            }

            // ── Shutdown blocking (async cleanup demo) ────────────────────────
            SystemEvent::ShuttingDown(handle) => {
                log!(
                    "SHUTDOWN",
                    "#{n} Shutdown / restart / logout requested!  (uptime {:.1}s)",
                    uptime.as_secs_f64()
                );
                log!("SHUTDOWN", "  → Blocking OS shutdown while async cleanup runs …");

                // Tell the OS we need time (Windows: shows a "waiting for <app>" dialog;
                // macOS: NSTerminateLater suspends the termination sequence).
                // We spawn an async task that does the real work, then calls handle.allow().
                let counter_clone = Arc::clone(&event_counter);
                rt.spawn(async move {
                    let cleanup_start = Instant::now();
                    async_cleanup(counter_clone).await;
                    let elapsed = cleanup_start.elapsed();
                    log!(
                        "SHUTDOWN",
                        "Cleanup took {:.2}s — calling handle.allow()",
                        elapsed.as_secs_f64()
                    );
                    handle.allow();
                    log!("SHUTDOWN", "handle.allow() sent. OS may now proceed.");
                });
            }

            // Future variants added by the library won't cause a compile error.
            _ => {
                log!("EVENT", "#{n} Unknown event (ignored)");
            }
        }

        println!(); // blank line between events for readability
    }

    // ── Sentinel closed ───────────────────────────────────────────────────────
    log!(
        "EXIT",
        "Sentinel channel closed after {} event(s). Exiting.",
        event_counter.load(Ordering::Relaxed)
    );
}
