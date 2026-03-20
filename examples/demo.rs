//! onebox_lifecycle demo
//!
//! Run with:
//!   cargo run --example demo
//!
//! Then trigger a shutdown (Windows: Start → Shut Down; macOS: Apple → Shut Down)
//! or put the machine to sleep to see the events fire.

use onebox_lifecycle::{Sentinel, SystemEvent};

fn main() {
    println!("[onebox_lifecycle demo] Starting sentinel…");

    // macOS: this must be called from the main thread.
    let sentinel = Sentinel::start();

    println!("[onebox_lifecycle demo] Listening for events. Press Ctrl+C to exit.\n");

    while let Some(event) = sentinel.recv() {
        println!("EVENT: {:?}", event);

        if let SystemEvent::ShuttingDown(handle) = event {
            println!("  → Received shutdown request. Simulating 3s of cleanup work…");

            // In a real app, spawn an async task here.
            // For the demo we sleep on this thread.
            std::thread::sleep(std::time::Duration::from_secs(3));

            println!("  → Cleanup done. Allowing shutdown.");
            handle.allow();
        }
    }
}
