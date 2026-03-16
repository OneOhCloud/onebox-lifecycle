/// macOS platform backend.
///
/// # Shutdown flow
///
/// ```text
/// applicationShouldTerminate:
///   → send SystemEvent::ShuttingDown(handle)
///   → return NSTerminateLater               ← AppKit suspends termination
///   → caller calls handle.allow() when done
///       → background thread posts [NSApp replyToApplicationShouldTerminate: YES]
///          onto the main GCD queue
/// ```
///
/// # Sleep / wake
///
/// `NSWorkspaceWillSleepNotification` / `NSWorkspaceDidWakeNotification` via
/// the workspace notification centre.
///
/// # Network
///
/// Uses `NWPathMonitor` (Network.framework, macOS 10.14+) — no polling, no
/// TCP probes.  The monitor delivers an initial snapshot synchronously after
/// `nw_path_monitor_start`, then fires again on every reachability change.
use std::sync::{Arc, Mutex};

use block2::RcBlock;
use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::{AnyObject, Sel};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{NSApplication, NSApplicationTerminateReply};
use objc2_foundation::{NSObject, NSString};

use crate::common::{
    EventSender, ShutdownDecision, ShutdownHandle, ShutdownHandleInner, SystemEvent,
};

// ─── Shared state ─────────────────────────────────────────────────────────────

struct SentinelState {
    event_tx: EventSender,
    /// The NSApplicationDelegate that was installed before us (e.g. Tauri's
    /// deep-link delegate).  Kept as a raw pointer because the previous owner
    /// (Tauri / AppKit) retains it for the process lifetime.
    /// null if there was no prior delegate.
    previous_delegate: *mut AnyObject,
}

// SAFETY: delegate callbacks always arrive on the main thread; we never
// free the pointer, only call Obj-C methods on it from the same thread.
unsafe impl Send for SentinelState {}

// ─── NSApplicationDelegate ────────────────────────────────────────────────────

define_class!(
    // SAFETY: NSObject subclassing requirements are met.
    //         The struct does not implement Drop.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "SysSentinelDelegate"]
    #[ivars = Arc<Mutex<SentinelState>>]
    struct SentinelDelegate;

    impl SentinelDelegate {
        /// Called by AppKit when the user (or OS) requests termination.
        /// Returning `TerminateLater` suspends the termination sequence.
        #[unsafe(method(applicationShouldTerminate:))]
        fn application_should_terminate(
            &self,
            _sender: &NSApplication,
        ) -> NSApplicationTerminateReply {
            let (decision_tx, decision_rx) =
                std::sync::mpsc::sync_channel::<ShutdownDecision>(1);

            let handle = ShutdownHandle {
                inner: Some(ShutdownHandleInner::Mpsc(decision_tx)),
            };

            self.ivars().lock().unwrap().event_tx.send(SystemEvent::ShuttingDown(handle));

            // Spawn a thread that waits for the caller's decision, then calls
            // `replyToApplicationShouldTerminate:` back on the main thread via GCD.
            std::thread::spawn(move || {
                // Wait up to 5 minutes for async cleanup to finish.
                let decision = decision_rx
                    .recv_timeout(std::time::Duration::from_secs(300))
                    .unwrap_or(ShutdownDecision::Allow);

                let proceed = matches!(decision, ShutdownDecision::Allow);
                reply_to_should_terminate(proceed);
            });

            // TerminateLater — AppKit will wait for our explicit reply.
            NSApplicationTerminateReply::TerminateLater
        }

        /// Transparent proxy: forward any delegate message we don't implement
        /// to the previous delegate.  This covers `application:openURLs:` for
        /// deep links, `applicationDidFinishLaunching:`, and every other
        /// optional NSApplicationDelegate method — without enumerating them.
        #[unsafe(method(forwardingTargetForSelector:))]
        fn forwarding_target_for_selector(&self, sel: Sel) -> *mut AnyObject {
            let prev = self.ivars().lock().unwrap().previous_delegate;
            if !prev.is_null() {
                let responds: bool = unsafe { msg_send![prev, respondsToSelector: sel] };
                if responds {
                    return prev;
                }
            }
            std::ptr::null_mut()
        }

        /// Report our effective capabilities: own methods + whatever the
        /// previous delegate supports.  AppKit checks this before sending
        /// optional delegate messages, so it must return `true` for any
        /// selector the previous delegate handles (e.g. `application:openURLs:`).
        #[unsafe(method(respondsToSelector:))]
        fn responds_to_selector(&self, sel: Sel) -> bool {
            let self_responds: bool =
                unsafe { msg_send![super(self), respondsToSelector: sel] };
            if self_responds {
                return true.into();
            }
            let prev = self.ivars().lock().unwrap().previous_delegate;
            let prev_responds: bool =
                !prev.is_null() && unsafe { msg_send![prev, respondsToSelector: sel] };
            prev_responds.into()
        }
    }
);

impl SentinelDelegate {
    fn new(mtm: MainThreadMarker, state: Arc<Mutex<SentinelState>>) -> Retained<Self> {
        let this = mtm.alloc::<Self>().set_ivars(state);
        unsafe { msg_send![super(this), init] }
    }
}

// ─── Power observer ───────────────────────────────────────────────────────────

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "SysSentinelPowerObserver"]
    #[ivars = Arc<Mutex<SentinelState>>]
    struct PowerObserver;

    impl PowerObserver {
        #[unsafe(method(handleWillSleep:))]
        fn handle_will_sleep(&self, _notification: &AnyObject) {
            self.ivars().lock().unwrap().event_tx.send(SystemEvent::WillSleep);
        }

        #[unsafe(method(handleDidWake:))]
        fn handle_did_wake(&self, _notification: &AnyObject) {
            self.ivars().lock().unwrap().event_tx.send(SystemEvent::DidWake);
        }
    }
);

impl PowerObserver {
    fn new(mtm: MainThreadMarker, state: Arc<Mutex<SentinelState>>) -> Retained<Self> {
        let this = mtm.alloc::<Self>().set_ivars(state);
        unsafe { msg_send![super(this), init] }
    }
}

// ─── NWPathMonitor FFI ────────────────────────────────────────────────────────
//
// Network.framework exposes a C API since macOS 10.14 / iOS 12.
// nw_* objects are reference-counted via nw_retain / nw_release (not ObjC ARC).

/// Opaque handle to a `nw_path_monitor_t`.
type NwPathMonitorT = *mut std::ffi::c_void;

/// Opaque handle to a `nw_path_t` (passed into the update-handler block).
type NwPathT = *mut std::ffi::c_void;

/// `nw_path_status_satisfied` — at least one interface can carry traffic.
const NW_PATH_STATUS_SATISFIED: u32 = 1;

// Network.framework C API
// SAFETY: the function signatures match the Apple-published headers exactly.
#[link(name = "Network", kind = "framework")]
unsafe extern "C" {
    /// Create a new path monitor that observes all available interfaces.
    fn nw_path_monitor_create() -> NwPathMonitorT;

    /// Set the handler block called on every path change.
    ///
    /// The monitor copies (retains) the block internally.
    fn nw_path_monitor_set_update_handler(
        monitor: NwPathMonitorT,
        // `void (^update_handler)(nw_path_t path)`
        update_handler: *const block2::Block<dyn Fn(NwPathT)>,
    );

    /// Set the dispatch queue on which the handler is invoked.
    fn nw_path_monitor_set_queue(monitor: NwPathMonitorT, queue: *mut std::ffi::c_void);

    /// Start observing.  Delivers the current path immediately.
    fn nw_path_monitor_start(monitor: NwPathMonitorT);

    /// Stop observing and release OS resources.
    fn nw_path_monitor_cancel(monitor: NwPathMonitorT);

    /// Query the satisfaction status of a `nw_path_t`.
    fn nw_path_get_status(path: NwPathT) -> u32; // nw_path_status_t

    /// Release a reference to any `nw_object_t` (includes `nw_path_monitor_t`).
    fn nw_release(obj: *mut std::ffi::c_void);
}

// GCD — in libSystem, always linked on macOS.
unsafe extern "C" {
    /// Returns the global concurrent queue at the given QoS class.
    /// Pass `0` for `QOS_CLASS_DEFAULT`.
    fn dispatch_get_global_queue(
        identifier: std::ffi::c_long,
        flags: std::ffi::c_ulong,
    ) -> *mut std::ffi::c_void;
}

// ─── PathMonitorGuard ─────────────────────────────────────────────────────────

/// RAII wrapper — cancels and releases the monitor when dropped.
struct PathMonitorGuard {
    monitor: NwPathMonitorT,
    /// Kept alive so the closure's captures (Arc<Mutex<…>>) survive for the
    /// monitor's lifetime.  The monitor also retains the block internally.
    _block: RcBlock<dyn Fn(NwPathT)>,
}

impl Drop for PathMonitorGuard {
    fn drop(&mut self) {
        // SAFETY: monitor was created by nw_path_monitor_create and has not
        //         been cancelled before.
        unsafe {
            nw_path_monitor_cancel(self.monitor);
            nw_release(self.monitor);
        }
    }
}

// SAFETY: nw_path_monitor_t is thread-safe per Apple's Network.framework docs.
unsafe impl Send for PathMonitorGuard {}
unsafe impl Sync for PathMonitorGuard {}

// ─── NWPathMonitor constructor ────────────────────────────────────────────────

/// Start a `NWPathMonitor` and forward reachability changes to `state`.
///
/// The monitor delivers an **initial** path update synchronously after `start`,
/// so callers always receive `NetworkUp` or `NetworkDown` shortly after startup
/// without having to poll.
///
/// Change detection: events are only emitted when the satisfied-status
/// actually changes, or on the very first delivery (so callers know the
/// initial state).
fn start_nw_path_monitor(state: Arc<Mutex<SentinelState>>) -> PathMonitorGuard {
    // `None` = "never seen before" — first delivery always emits an event.
    let last_state: Arc<Mutex<Option<bool>>> = Arc::new(Mutex::new(None));

    // Clone for the block closure.
    let last_state_in_block = Arc::clone(&last_state);

    // Build the update-handler block.
    //
    // `*mut c_void` implements `Encode` (via `c_void: RefEncode` + the blanket
    // impl for raw pointers), so `dyn Fn(NwPathT)` satisfies `BlockFn`.
    let block = RcBlock::new(move |path: NwPathT| {
        // SAFETY: `path` is a valid nw_path_t for the duration of this call.
        let raw_status = unsafe { nw_path_get_status(path) };
        let satisfied = raw_status == NW_PATH_STATUS_SATISFIED;

        // Compare with previous state; emit only when it changes (or is first).
        let mut last = last_state_in_block.lock().unwrap();
        if last.as_ref() == Some(&satisfied) {
            return; // no change
        }
        *last = Some(satisfied);
        drop(last); // release lock before touching event_tx

        let ev = if satisfied {
            SystemEvent::NetworkUp
        } else {
            SystemEvent::NetworkDown
        };
        state.lock().unwrap().event_tx.send(ev);
    });

    // SAFETY: all nw_ functions are called with valid, non-null arguments.
    let monitor = unsafe {
        let monitor = nw_path_monitor_create();
        assert!(!monitor.is_null(), "nw_path_monitor_create returned null");

        // Pass a pointer to our block. The monitor copies (retains) it.
        nw_path_monitor_set_update_handler(monitor, RcBlock::as_ptr(&block));

        // Deliver callbacks on the default global concurrent queue.
        // QOS_CLASS_DEFAULT = 0.
        let queue = dispatch_get_global_queue(0, 0);
        nw_path_monitor_set_queue(monitor, queue);

        // Start observing. Fires the handler immediately with the current path.
        nw_path_monitor_start(monitor);

        monitor
    };

    PathMonitorGuard {
        monitor,
        _block: block,
    }
}

// ─── MacosGuard ───────────────────────────────────────────────────────────────

pub struct MacosGuard {
    _delegate: Retained<SentinelDelegate>,
    _power_observer: Retained<PowerObserver>,
    /// Cancels NWPathMonitor on drop.
    _path_monitor: PathMonitorGuard,
}

impl MacosGuard {
    /// Install all listeners.  **Must be called from the main thread.**
    pub fn new(event_tx: EventSender) -> Self {
        let mtm = MainThreadMarker::new()
            .expect("onebox_lifecycle: MacosGuard::new() must be called from the main thread");

        // ── 1. Install the NSApplication delegate ─────────────────────────
        // Capture the existing delegate first so we can forward messages to it.
        let previous_delegate: *mut AnyObject = unsafe {
            let app = NSApplication::sharedApplication(mtm);
            msg_send![&*app, delegate]
        };

        let state = Arc::new(Mutex::new(SentinelState { event_tx, previous_delegate }));

        let delegate = SentinelDelegate::new(mtm, Arc::clone(&state));
        unsafe {
            let app = NSApplication::sharedApplication(mtm);
            let delegate_obj = Retained::as_ptr(&delegate) as *const NSObject;
            let _: () = msg_send![&*app, setDelegate: delegate_obj];
        }

        // ── 2. NSWorkspace sleep / wake notifications ──────────────────────
        let power_observer = PowerObserver::new(mtm, Arc::clone(&state));
        autoreleasepool(|_pool| unsafe {
            let workspace: *mut AnyObject = msg_send![objc2::class!(NSWorkspace), sharedWorkspace];
            let nc: *mut AnyObject = msg_send![workspace, notificationCenter];

            let will_sleep = NSString::from_str("NSWorkspaceWillSleepNotification");
            let did_wake = NSString::from_str("NSWorkspaceDidWakeNotification");
            let observer = Retained::as_ptr(&power_observer) as *const NSObject;

            let _: () = msg_send![nc,
                addObserver: observer,
                selector: objc2::sel!(handleWillSleep:),
                name: &*will_sleep,
                object: std::ptr::null::<AnyObject>()
            ];
            let _: () = msg_send![nc,
                addObserver: observer,
                selector: objc2::sel!(handleDidWake:),
                name: &*did_wake,
                object: std::ptr::null::<AnyObject>()
            ];
        });

        // ── 3. NWPathMonitor (Network.framework, macOS 10.14+) ────────────
        let path_monitor = start_nw_path_monitor(Arc::clone(&state));

        MacosGuard {
            _delegate: delegate,
            _power_observer: power_observer,
            _path_monitor: path_monitor,
        }
    }
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

/// Post `[NSApp replyToApplicationShouldTerminate: proceed]`.
///
/// For CLI apps the main thread runs no NSRunLoop, so dispatching to the main
/// GCD queue would deadlock (the block never executes).  We call directly from
/// the background thread instead — AppKit accepts this for non-GUI processes.
fn reply_to_should_terminate(proceed: bool) {
    // SAFETY: MainThreadMarker::new_unchecked() bypasses the compile-time thread
    // check.  For a CLI process with no AppKit event loop this is safe because
    // `replyToApplicationShouldTerminate:` only posts a Mach message internally
    // and has no UI side-effects that require the main thread.
    unsafe {
        let mtm = MainThreadMarker::new_unchecked();
        let app = NSApplication::sharedApplication(mtm);
        let _: () = msg_send![&*app, replyToApplicationShouldTerminate: proceed];
    }
}
