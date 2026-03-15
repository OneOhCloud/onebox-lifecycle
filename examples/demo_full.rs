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

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use onebox_lifecycle::{Sentinel, SystemEvent};

// ─── Logging helpers ─────────────────────────────────────────────────────────

fn hms() -> String {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let s = (ms / 1000) % 86400;
    let h = s / 3600;
    let m = (s % 3600) / 60;
    let sec = s % 60;
    let frac = ms % 1000;
    format!("{h:02}:{m:02}:{sec:02}.{frac:03}")
}

macro_rules! log {
    ($tag:expr, $($arg:tt)*) => {
        println!("[{}] [{:<8}] {}", hms(), $tag, format!($($arg)*))
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

    // ── Banner ────────────────────────────────────────────────────────────────
    let sep = "─".repeat(64);
    println!("{sep}");
    println!("  onebox_lifecycle — full demo");
    println!("  Platform : {}", platform());
    println!("  PID      : {}", std::process::id());
    println!("{sep}");
    println!("  Triggers to try:");
    println!("    • Sleep / wake the machine       → WillSleep / DidWake");
    println!("    • Toggle Wi-Fi / unplug cable    → NetworkDown / NetworkUp");
    println!("    • Shutdown / restart             → ShuttingDown + async cleanup");
    println!("    • Ctrl-C                         → exit immediately");
    println!("{sep}");
    println!();

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
