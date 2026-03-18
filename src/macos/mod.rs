/// macOS platform backend.
///
/// # Shutdown flow
///
/// ```text
/// applicationShouldTerminate:
///   → send SystemEvent::ShuttingDown(handle)
///   → return NSTerminateLater               ← AppKit suspends termination
///   → caller calls handle.allow() when done
///       → background thread calls [NSApp replyToApplicationShouldTerminate: YES]
///          (thread-safe per Apple docs — only posts a Mach message internally)
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
use std::sync::Arc;
#[cfg(any(feature = "shutdown", feature = "network"))]
use std::sync::Mutex;

#[cfg(feature = "network")]
use block2::RcBlock;
#[cfg(any(feature = "shutdown", feature = "sleep"))]
use objc2::rc::Retained;
#[cfg(feature = "sleep")]
use objc2::rc::autoreleasepool;
#[cfg(any(feature = "shutdown", feature = "sleep"))]
use objc2::runtime::AnyObject;
#[cfg(feature = "shutdown")]
use objc2::runtime::Sel;
use objc2::MainThreadMarker;
#[cfg(any(feature = "shutdown", feature = "sleep"))]
use objc2::msg_send;
#[cfg(any(feature = "shutdown", feature = "sleep"))]
use objc2::{DefinedClass, MainThreadOnly, define_class};
#[cfg(feature = "shutdown")]
use objc2_app_kit::{NSApplication, NSApplicationTerminateReply};
#[cfg(any(feature = "shutdown", feature = "sleep"))]
use objc2_foundation::NSObject;
#[cfg(feature = "sleep")]
use objc2_foundation::NSString;

use crate::common::EventSender;
#[cfg(any(feature = "shutdown", feature = "sleep", feature = "network"))]
use crate::common::SystemEvent;
#[cfg(feature = "shutdown")]
use crate::common::{ShutdownDecision, ShutdownHandle, ShutdownHandleInner};
#[cfg(feature = "sleep")]
use crate::common::{SleepHandle, SleepMonitorLevel};
#[cfg(feature = "sleep")]
use std::sync::atomic::{AtomicU32, Ordering};

// ─── Constants ────────────────────────────────────────────────────────────────

/// Maximum time the caller has to finish async cleanup before shutdown is
/// allowed anyway.
#[cfg(feature = "shutdown")]
const SHUTDOWN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// `NSWorkspaceWillSleepNotification` — posted before the system sleeps.
#[cfg(feature = "sleep")]
const WILL_SLEEP_NOTIFICATION: &str = "NSWorkspaceWillSleepNotification";

/// `NSWorkspaceDidWakeNotification` — posted after the system wakes.
#[cfg(feature = "sleep")]
const DID_WAKE_NOTIFICATION: &str = "NSWorkspaceDidWakeNotification";

// ─── Delegate state (shutdown) ────────────────────────────────────────────────

#[cfg(feature = "shutdown")]
struct DelegateState {
    event_tx: Arc<EventSender>,
    /// The NSApplicationDelegate that was installed before us (e.g. Tauri's
    /// deep-link delegate).  Kept as a raw pointer because the previous owner
    /// (Tauri / AppKit) retains it for the process lifetime.
    /// `null` if there was no prior delegate.
    previous_delegate: *mut AnyObject,
}

// SAFETY: delegate callbacks always arrive on the main thread; we never free
// the pointer, only call Obj-C methods on it from the same thread.
#[cfg(feature = "shutdown")]
unsafe impl Send for DelegateState {}

// ─── NSApplicationDelegate (shutdown) ─────────────────────────────────────────

#[cfg(feature = "shutdown")]
define_class!(
    // SAFETY: NSObject subclassing requirements are met.
    //         The struct does not implement Drop.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "SysSentinelDelegate"]
    #[ivars = Arc<Mutex<DelegateState>>]
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
            // `replyToApplicationShouldTerminate:` — which is documented by
            // Apple to be callable from any thread.
            std::thread::spawn(move || {
                let decision = decision_rx
                    .recv_timeout(SHUTDOWN_TIMEOUT)
                    .unwrap_or(ShutdownDecision::Allow);

                reply_to_should_terminate(matches!(decision, ShutdownDecision::Allow));
            });

            // TerminateLater — AppKit will wait for our explicit reply.
            NSApplicationTerminateReply::TerminateLater
        }

        /// Hot-start deep-link entry point on macOS.
        ///
        /// AppKit calls this when a URL is opened while the app is already
        /// running.  WRY / Tauri listen for `application:openURLs:` on the
        /// NSApplicationDelegate to generate `RunEvent::Opened`, which is what
        /// drives `tauri-plugin-deep-link`'s `on_open_url` callbacks.
        ///
        /// We cannot rely solely on `forwardingTargetForSelector:` here because
        /// WRY may register the method via `class_addMethod` at runtime, which
        /// can cause `[prev respondsToSelector:]` to return `NO` even when the
        /// method exists, silently breaking the forwarding chain.  Implementing
        /// the method **directly** guarantees AppKit always invokes it.
        #[unsafe(method(application:openURLs:))]
        fn application_open_urls(&self, application: *mut AnyObject, urls: *mut AnyObject) {
            let prev = self.ivars().lock().unwrap().previous_delegate;
            if !prev.is_null() {
                unsafe { let _: () = msg_send![prev, application: application, openURLs: urls]; }
            }
        }

        /// Transparent proxy: forward any other delegate message we don't
        /// implement directly to the previous delegate.
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

#[cfg(feature = "shutdown")]
impl SentinelDelegate {
    fn new(mtm: MainThreadMarker, state: Arc<Mutex<DelegateState>>) -> Retained<Self> {
        let this = mtm.alloc::<Self>().set_ivars(state);
        unsafe { msg_send![super(this), init] }
    }
}

// ─── Power observer (sleep/wake) ─────────────────────────────────────────────

#[cfg(feature = "sleep")]
define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "SysSentinelPowerObserver"]
    // Only needs the sender — decoupled from DelegateState.
    #[ivars = Arc<EventSender>]
    struct PowerObserver;

    impl PowerObserver {
        #[unsafe(method(handleWillSleep:))]
        fn handle_will_sleep(&self, _notification: &AnyObject) {
            self.ivars().send(SystemEvent::WillSleep);
        }

        #[unsafe(method(handleDidWake:))]
        fn handle_did_wake(&self, _notification: &AnyObject) {
            self.ivars().send(SystemEvent::DidWake);
        }
    }
);

#[cfg(feature = "sleep")]
impl PowerObserver {
    fn new(mtm: MainThreadMarker, event_tx: Arc<EventSender>) -> Retained<Self> {
        let this = mtm.alloc::<Self>().set_ivars(event_tx);
        unsafe { msg_send![super(this), init] }
    }
}

// ─── Notification observer guard (sleep/wake) ────────────────────────────────

/// RAII guard that calls `[NSNotificationCenter removeObserver:]` when dropped,
/// preventing stale callbacks if the observer is ever released before the
/// notification centre.
///
/// `NSNotificationCenter` retains observers since macOS 10.x, so failing to
/// call `removeObserver:` keeps the object alive indefinitely (memory leak) and
/// may deliver callbacks to a logically-dead observer.
#[cfg(feature = "sleep")]
struct NotificationObserverGuard {
    /// Non-owning pointer to the `PowerObserver`.  The object is kept alive by
    /// `MacosGuard._power_observer`; this guard must be dropped first.
    observer: *const NSObject,
}

#[cfg(feature = "sleep")]
impl Drop for NotificationObserverGuard {
    fn drop(&mut self) {
        // SAFETY: `NSWorkspace.sharedWorkspace` and its notification centre are
        // process-lifetime singletons.  `removeObserver:` is documented
        // thread-safe by Apple.
        unsafe {
            let workspace: *mut AnyObject = msg_send![objc2::class!(NSWorkspace), sharedWorkspace];
            let nc: *mut AnyObject = msg_send![workspace, notificationCenter];
            let _: () = msg_send![nc, removeObserver: self.observer];
        }
    }
}

// SAFETY: `removeObserver:` on NSNotificationCenter is thread-safe.
#[cfg(feature = "sleep")]
unsafe impl Send for NotificationObserverGuard {}
#[cfg(feature = "sleep")]
unsafe impl Sync for NotificationObserverGuard {}

// ─── NWPathMonitor FFI (network) ─────────────────────────────────────────────
//
// Network.framework exposes a C API since macOS 10.14 / iOS 12.
// nw_* objects are reference-counted via nw_retain / nw_release (not ObjC ARC).

#[cfg(feature = "network")]
mod nw_ffi {
    /// Opaque handle to a `nw_path_monitor_t`.
    pub type NwPathMonitorT = *mut std::ffi::c_void;

    /// Opaque handle to a `nw_path_t` (passed into the update-handler block).
    pub type NwPathT = *mut std::ffi::c_void;

    /// `nw_path_status_satisfied` — at least one interface can carry traffic.
    pub const NW_PATH_STATUS_SATISFIED: u32 = 1;

    // Network.framework C API
    // SAFETY: the function signatures match the Apple-published headers exactly.
    #[link(name = "Network", kind = "framework")]
    unsafe extern "C" {
        /// Create a new path monitor that observes all available interfaces.
        pub fn nw_path_monitor_create() -> NwPathMonitorT;

        /// Set the handler block called on every path change.
        ///
        /// The monitor copies (retains) the block internally.
        pub fn nw_path_monitor_set_update_handler(
            monitor: NwPathMonitorT,
            // `void (^update_handler)(nw_path_t path)`
            update_handler: *const block2::Block<dyn Fn(NwPathT)>,
        );

        /// Set the dispatch queue on which the handler is invoked.
        pub fn nw_path_monitor_set_queue(monitor: NwPathMonitorT, queue: *mut std::ffi::c_void);

        /// Start observing.  Delivers the current path immediately.
        pub fn nw_path_monitor_start(monitor: NwPathMonitorT);

        /// Stop observing and release OS resources.
        pub fn nw_path_monitor_cancel(monitor: NwPathMonitorT);

        /// Query the satisfaction status of a `nw_path_t`.
        pub fn nw_path_get_status(path: NwPathT) -> u32; // nw_path_status_t

        /// Release a reference to any `nw_object_t` (includes `nw_path_monitor_t`).
        pub fn nw_release(obj: *mut std::ffi::c_void);
    }

    // GCD — in libSystem, always linked on macOS.
    unsafe extern "C" {
        /// Returns the global concurrent queue at the given QoS class.
        /// Pass `0` for `QOS_CLASS_DEFAULT`.
        pub fn dispatch_get_global_queue(
            identifier: std::ffi::c_long,
            flags: std::ffi::c_ulong,
        ) -> *mut std::ffi::c_void;
    }
}

// ─── PathMonitorGuard (network) ──────────────────────────────────────────────

/// RAII wrapper — cancels and releases the monitor when dropped.
#[cfg(feature = "network")]
struct PathMonitorGuard {
    monitor: nw_ffi::NwPathMonitorT,
    /// Kept alive so the closure's captures (Arc<EventSender>) survive for the
    /// monitor's lifetime.  The monitor also retains the block internally.
    _block: RcBlock<dyn Fn(nw_ffi::NwPathT)>,
}

#[cfg(feature = "network")]
impl Drop for PathMonitorGuard {
    fn drop(&mut self) {
        // SAFETY: monitor was created by nw_path_monitor_create and has not
        //         been cancelled before.
        unsafe {
            nw_ffi::nw_path_monitor_cancel(self.monitor);
            nw_ffi::nw_release(self.monitor);
        }
    }
}

// SAFETY: nw_path_monitor_t is thread-safe per Apple's Network.framework docs.
#[cfg(feature = "network")]
unsafe impl Send for PathMonitorGuard {}
#[cfg(feature = "network")]
unsafe impl Sync for PathMonitorGuard {}

// ─── NWPathMonitor constructor (network) ─────────────────────────────────────

/// Start a `NWPathMonitor` and forward reachability changes to `event_tx`.
///
/// The monitor delivers an **initial** path update synchronously after `start`,
/// so callers always receive `NetworkUp` or `NetworkDown` shortly after startup
/// without having to poll.
///
/// Change detection: events are only emitted when the satisfied-status
/// actually changes, or on the very first delivery (so callers know the
/// initial state).
#[cfg(feature = "network")]
fn start_nw_path_monitor(event_tx: Arc<EventSender>) -> PathMonitorGuard {
    use nw_ffi::*;

    // `None` = "never seen before" — first delivery always emits an event.
    let last_seen: Arc<Mutex<Option<bool>>> = Arc::new(Mutex::new(None));

    // Build the update-handler block.  Both `last_seen` and `event_tx` are
    // moved directly into the closure — no redundant intermediate clone needed.
    //
    // `*mut c_void` implements `Encode` (via `c_void: RefEncode` + the blanket
    // impl for raw pointers), so `dyn Fn(NwPathT)` satisfies `BlockFn`.
    let block = RcBlock::new(move |path: NwPathT| {
        // SAFETY: `path` is a valid nw_path_t for the duration of this call.
        let satisfied = unsafe { nw_path_get_status(path) } == NW_PATH_STATUS_SATISFIED;

        // Compare with previous state; emit only when it changes (or is first).
        let mut last = last_seen.lock().unwrap();
        if *last == Some(satisfied) {
            return; // no change
        }
        *last = Some(satisfied);
        drop(last); // release lock before touching event_tx

        event_tx.send(if satisfied {
            SystemEvent::NetworkUp
        } else {
            SystemEvent::NetworkDown
        });
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

// ─── IOKit FFI (deep sleep) ───────────────────────────────────────────────────
//
// IOKit's power notification API allows delaying system sleep until the caller
// explicitly calls `IOAllowPowerChange`.  Notifications fire on a dedicated
// CFRunLoop thread, which blocks (recv_timeout) until the SleepHandle is
// resolved or the caller-configured timeout expires.

#[cfg(feature = "sleep")]
#[allow(non_camel_case_types, non_upper_case_globals)]
mod iokit_ffi {
    use std::ffi::c_void;

    /// `mach_port_t` — the type underlying both `io_object_t` and `io_connect_t`.
    pub type io_object_t  = u32;
    pub type io_connect_t = io_object_t;
    /// Opaque Mach-port wrapper returned by `IORegisterForSystemPower`.
    pub type IONotificationPortRef = *mut c_void;

    /// System is going to sleep — acknowledge with `IOAllowPowerChange`.
    pub const kIOMessageSystemWillSleep:    u32 = 0xe000_0280;
    /// System has fully powered on after wake.
    pub const kIOMessageSystemHasPoweredOn: u32 = 0xe000_0300;

    /// `IOServiceInterestCallback`:
    ///   `void (*)(void *refcon, io_service_t, uint32_t messageType, void *messageArgument)`
    pub type IOServiceInterestCallback = unsafe extern "C" fn(
        refcon:           *mut c_void,
        service:          io_object_t,
        message_type:     u32,
        message_argument: *mut c_void,
    );

    #[link(name = "IOKit", kind = "framework")]
    unsafe extern "C" {
        /// Register for sleep/wake messages on the root power domain.
        /// Returns the `io_connect_t` needed for `IOAllowPowerChange`.
        pub fn IORegisterForSystemPower(
            refcon:            *mut c_void,
            notification_port: *mut IONotificationPortRef,
            callback:          IOServiceInterestCallback,
            notifier:          *mut io_object_t,
        ) -> io_connect_t;

        /// Extract the `CFRunLoopSourceRef` from the notification port.
        pub fn IONotificationPortGetRunLoopSource(
            notify: IONotificationPortRef,
        ) -> *mut c_void; // CFRunLoopSourceRef

        /// Allow the power transition to proceed.  Thread-safe per Apple docs.
        pub fn IOAllowPowerChange(
            kernel_port:     io_connect_t,
            notification_id: *mut c_void, // messageArgument
        ) -> i32;

        /// Unregister the notifier from the power domain.
        pub fn IODeregisterForSystemPower(notifier: *mut io_object_t) -> i32;

        /// Decrement the IOKit retain count on an object.
        pub fn IOObjectRelease(object: io_object_t) -> i32;

        /// Destroy the notification port (releases Mach port resources).
        pub fn IONotificationPortDestroy(notify: IONotificationPortRef);
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        /// Returns the `CFRunLoopRef` for the current thread.
        pub fn CFRunLoopGetCurrent() -> *mut c_void;

        /// Adds a source to a run loop in the given mode.
        pub fn CFRunLoopAddSource(
            rl:     *mut c_void,  // CFRunLoopRef
            source: *mut c_void,  // CFRunLoopSourceRef
            mode:   *const c_void, // CFStringRef
        );

        /// Run the current thread's run loop until `CFRunLoopStop` is called.
        pub fn CFRunLoopRun();

        /// Stop a run loop.  Thread-safe per Apple docs.
        pub fn CFRunLoopStop(rl: *mut c_void);

        /// Increment the CF retain count.
        pub fn CFRetain(cf: *const c_void) -> *const c_void;

        /// Decrement the CF retain count; may deallocate.
        pub fn CFRelease(cf: *const c_void);

        /// Default run-loop mode (`"kCFRunLoopDefaultMode"`).
        pub static kCFRunLoopDefaultMode: *const c_void;
    }
}

// ─── IOKit callback context ───────────────────────────────────────────────────

/// Shared state passed to the C callback via `refcon`.
/// Heap-allocated behind an `Arc`; the background thread borrows it as `&Self`.
#[cfg(feature = "sleep")]
struct IoKitCallbackContext {
    event_tx:    Arc<EventSender>,
    /// Set by the background thread right after `IORegisterForSystemPower`
    /// returns, before `CFRunLoopRun()` starts.  Read in the callback.
    kernel_port: AtomicU32,
    timeout:     std::time::Duration,
}

// ─── IOKit power callback ─────────────────────────────────────────────────────

/// C callback fired on the dedicated IOKit CFRunLoop thread.
///
/// For `kIOMessageSystemWillSleep` we send a [`SystemEvent::WillHibernate`]
/// and then block (up to `timeout`) for the caller to call
/// [`SleepHandle::allow`].  `IOAllowPowerChange` is always called afterwards
/// so the OS is never permanently blocked.
#[cfg(feature = "sleep")]
unsafe extern "C" fn iokit_power_callback(
    refcon:           *mut std::ffi::c_void,
    _service:         iokit_ffi::io_object_t,
    message_type:     u32,
    message_argument: *mut std::ffi::c_void,
) {
    // SAFETY: `refcon` points to an `Arc<IoKitCallbackContext>` that is kept
    // alive by `IoKitDeepSleepGuard._ctx` for the entire CFRunLoop lifetime.
    // We borrow it without adjusting the reference count.
    let ctx = unsafe { &*(refcon as *const IoKitCallbackContext) };

    match message_type {
        iokit_ffi::kIOMessageSystemWillSleep => {
            let (tx, rx) = std::sync::mpsc::sync_channel::<()>(1);
            let handle = SleepHandle { inner: Some(tx) };
            ctx.event_tx.send(SystemEvent::WillHibernate(handle));

            // Block the CFRunLoop thread — IOKit waits for IOAllowPowerChange.
            let _ = rx.recv_timeout(ctx.timeout);

            let kernel_port = ctx.kernel_port.load(Ordering::Acquire);
            unsafe { iokit_ffi::IOAllowPowerChange(kernel_port, message_argument); }
        }
        iokit_ffi::kIOMessageSystemHasPoweredOn => {
            ctx.event_tx.send(SystemEvent::DidWake);
        }
        _ => {}
    }
}

// ─── IoKitDeepSleepGuard ──────────────────────────────────────────────────────

/// RAII guard — stops the CFRunLoop thread and releases IOKit resources on drop.
#[cfg(feature = "sleep")]
struct IoKitDeepSleepGuard {
    /// Retained `CFRunLoopRef` of the background thread.
    run_loop:          *mut std::ffi::c_void,
    notifier:          iokit_ffi::io_object_t,
    notification_port: iokit_ffi::IONotificationPortRef,
    /// Joined before IOKit resources are released, ensuring in-flight callbacks
    /// complete before `_ctx` drops and the Arc refcount reaches zero.
    _thread:           Option<std::thread::JoinHandle<()>>,
    /// Keeps `IoKitCallbackContext` alive until the thread exits.
    _ctx:              Arc<IoKitCallbackContext>,
}

#[cfg(feature = "sleep")]
impl Drop for IoKitDeepSleepGuard {
    fn drop(&mut self) {
        unsafe {
            // 1. Signal the run loop to exit (thread-safe).
            iokit_ffi::CFRunLoopStop(self.run_loop);
        }
        // 2. Wait for the thread — ensures any in-progress callback finishes
        //    before we release the IOKit objects it may be using.
        if let Some(thread) = self._thread.take() {
            let _ = thread.join();
        }
        unsafe {
            // 3. Release the retained CFRunLoopRef.
            iokit_ffi::CFRelease(self.run_loop as *const _);
            // 4. Deregister from IOKit.
            iokit_ffi::IODeregisterForSystemPower(&mut self.notifier);
            iokit_ffi::IOObjectRelease(self.notifier);
            // 5. Destroy the Mach notification port.
            iokit_ffi::IONotificationPortDestroy(self.notification_port);
        }
    }
}

// SAFETY: All IOKit / CoreFoundation cleanup APIs used in `Drop` are
// documented as thread-safe by Apple.  Raw pointers are only accessed in
// `Drop`, which runs at most once.
#[cfg(feature = "sleep")]
unsafe impl Send for IoKitDeepSleepGuard {}
#[cfg(feature = "sleep")]
unsafe impl Sync for IoKitDeepSleepGuard {}

// ─── IOKit deep-sleep constructor ─────────────────────────────────────────────

/// Bundle of values the background thread sends back to the spawning thread
/// after `IORegisterForSystemPower` succeeds.
///
/// Pointers are stored as `usize` so the struct is trivially `Send`
/// (`usize: Send`), avoiding an `unsafe impl Send` for a raw-pointer struct.
#[cfg(feature = "sleep")]
struct IoKitInitResult {
    /// `CFRunLoopRef` cast to `usize`.
    run_loop:          usize,
    notifier:          iokit_ffi::io_object_t,
    /// `IONotificationPortRef` (`*mut c_void`) cast to `usize`.
    notification_port: usize,
}

/// Start a dedicated CFRunLoop thread that registers for IOKit power
/// notifications and emits [`SystemEvent::WillHibernate`] with a
/// [`SleepHandle`] before each sleep transition.
#[cfg(feature = "sleep")]
fn start_iokit_deep_sleep(
    event_tx: Arc<EventSender>,
    timeout:  std::time::Duration,
) -> IoKitDeepSleepGuard {
    use iokit_ffi::*;

    let ctx = Arc::new(IoKitCallbackContext {
        event_tx,
        kernel_port: AtomicU32::new(0),
        timeout,
    });
    let ctx_for_guard = Arc::clone(&ctx);
    // Store the raw pointer as `usize` so the closure capture is trivially
    // `Send` (usize: Send).  The Arc is kept alive by `ctx_for_guard`.
    let raw_ctx_usize = Arc::into_raw(ctx) as usize;

    // Rendezvous channel: background thread sends init data before blocking.
    let (init_tx, init_rx) = std::sync::mpsc::sync_channel::<IoKitInitResult>(0);

    let thread = std::thread::Builder::new()
        .name("onebox_lifecycle_iokit".into())
        .spawn(move || unsafe {
            let raw = raw_ctx_usize as *mut std::ffi::c_void;

            let mut notification_port: IONotificationPortRef = std::ptr::null_mut();
            let mut notifier: io_object_t = 0;

            let kernel_port = IORegisterForSystemPower(
                raw,
                &mut notification_port,
                iokit_power_callback,
                &mut notifier,
            );
            assert!(kernel_port != 0,
                "onebox_lifecycle: IORegisterForSystemPower returned null");

            // Write kernel_port before CFRunLoopRun() — callback fires only after.
            let ctx_ref = &*(raw as *const IoKitCallbackContext);
            ctx_ref.kernel_port.store(kernel_port, Ordering::Release);

            // Retain and record this thread's run loop so Drop can stop it.
            let rl = CFRunLoopGetCurrent();
            CFRetain(rl as *const _);

            let source = IONotificationPortGetRunLoopSource(notification_port);
            CFRunLoopAddSource(rl, source, kCFRunLoopDefaultMode);

            // Unblock the spawning thread.  Cast pointers to usize for Send.
            let _ = init_tx.send(IoKitInitResult {
                run_loop:          rl as usize,
                notifier,
                notification_port: notification_port as usize,
            });

            // Block until IoKitDeepSleepGuard::drop() calls CFRunLoopStop(rl).
            CFRunLoopRun();

            // Reclaim and drop the Arc — balances Arc::into_raw above.
            drop(Arc::from_raw(raw as *const IoKitCallbackContext));
        })
        .expect("onebox_lifecycle: failed to spawn IOKit run-loop thread");

    let init = init_rx.recv()
        .expect("onebox_lifecycle: IOKit thread failed to initialise");

    IoKitDeepSleepGuard {
        run_loop:          init.run_loop as *mut std::ffi::c_void,
        notifier:          init.notifier,
        notification_port: init.notification_port as iokit_ffi::IONotificationPortRef,
        _thread:           Some(thread),
        _ctx:              ctx_for_guard,
    }
}

// ─── MacosGuard ───────────────────────────────────────────────────────────────

pub struct MacosGuard {
    #[cfg(feature = "shutdown")]
    _delegate: Retained<SentinelDelegate>,
    /// `Some` in Standard mode; `None` in Deep mode (IOKit handles wake events).
    #[cfg(feature = "sleep")]
    _power_observer: Option<Retained<PowerObserver>>,
    /// `Some` in Standard mode.  Declared after `_power_observer` so that
    /// `removeObserver:` is called while NSNotificationCenter still holds a
    /// strong reference to the observer object.
    #[cfg(feature = "sleep")]
    _notification_guard: Option<NotificationObserverGuard>,
    /// Cancels NWPathMonitor on drop.
    #[cfg(feature = "network")]
    _path_monitor: PathMonitorGuard,
    /// `Some` in Deep mode; `None` in Standard mode.
    /// Declared last so it drops after the NSWorkspace observers above.
    #[cfg(feature = "sleep")]
    _iokit_deep_sleep: Option<IoKitDeepSleepGuard>,
}

impl MacosGuard {
    /// Install all listeners.  **Must be called from the main thread.**
    pub fn new(event_tx: EventSender, config: crate::SentinelConfig) -> Self {
        // Network-only builds don't reference `mtm` directly, but we still
        // assert main-thread for safety (NWPathMonitor setup is documented to
        // be safe from any thread, but future features may not be).
        #[allow(unused_variables)]
        let mtm = MainThreadMarker::new()
            .expect("onebox_lifecycle: MacosGuard::new() must be called from the main thread");

        // Wrap in Arc so it can be shared across delegate, power observer,
        // and the path-monitor block without cloning the channel sender.
        let event_tx = Arc::new(event_tx);

        // ── 1. Install the NSApplication delegate (shutdown) ────────────
        #[cfg(feature = "shutdown")]
        let _delegate = {
            // Capture the existing delegate first so we can forward messages to it.
            let previous_delegate: *mut AnyObject = unsafe {
                let app = NSApplication::sharedApplication(mtm);
                msg_send![&*app, delegate]
            };

            let delegate_state = Arc::new(Mutex::new(DelegateState {
                event_tx: Arc::clone(&event_tx),
                previous_delegate,
            }));

            let delegate = SentinelDelegate::new(mtm, delegate_state);
            unsafe {
                let app = NSApplication::sharedApplication(mtm);
                let delegate_obj = Retained::as_ptr(&delegate) as *const NSObject;
                let _: () = msg_send![&*app, setDelegate: delegate_obj];
            }
            delegate
        };

        // ── 2. Sleep / wake monitoring ──────────────────────────────────
        //
        // Standard: NSWorkspace notifications (WillSleep / DidWake, no delay).
        // Deep:     IOKit CFRunLoop thread (WillHibernate + SleepHandle, DidWake).
        //           NSWorkspace observers are skipped to avoid duplicate DidWake.
        #[cfg(feature = "sleep")]
        let (_power_observer, _notification_guard, _iokit_deep_sleep) =
            match config.sleep_monitor_level {
                SleepMonitorLevel::Standard => {
                    let power_observer = PowerObserver::new(mtm, Arc::clone(&event_tx));
                    let notification_guard = autoreleasepool(|_pool| unsafe {
                        let workspace: *mut AnyObject =
                            msg_send![objc2::class!(NSWorkspace), sharedWorkspace];
                        let nc: *mut AnyObject = msg_send![workspace, notificationCenter];

                        let will_sleep = NSString::from_str(WILL_SLEEP_NOTIFICATION);
                        let did_wake   = NSString::from_str(DID_WAKE_NOTIFICATION);
                        let observer   = Retained::as_ptr(&power_observer) as *const NSObject;

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

                        NotificationObserverGuard { observer }
                    });
                    (Some(power_observer), Some(notification_guard), None)
                }
                SleepMonitorLevel::Deep { timeout } => {
                    let guard = start_iokit_deep_sleep(Arc::clone(&event_tx), timeout);
                    (None, None, Some(guard))
                }
            };

        // ── 3. NWPathMonitor (Network.framework, macOS 10.14+) ──────────
        #[cfg(feature = "network")]
        let _path_monitor = start_nw_path_monitor(Arc::clone(&event_tx));

        MacosGuard {
            #[cfg(feature = "shutdown")]
            _delegate,
            #[cfg(feature = "sleep")]
            _power_observer,
            #[cfg(feature = "sleep")]
            _notification_guard,
            #[cfg(feature = "network")]
            _path_monitor,
            #[cfg(feature = "sleep")]
            _iokit_deep_sleep,
        }
    }
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

/// Post `[NSApp replyToApplicationShouldTerminate: proceed]`.
///
/// Apple documents `replyToApplicationShouldTerminate:` as callable from any
/// thread — it only posts a Mach message internally and has no UI side-effects
/// that require the main thread.  We therefore call it directly from the
/// background waiter thread without dispatching to the main queue.
#[cfg(feature = "shutdown")]
fn reply_to_should_terminate(proceed: bool) {
    // SAFETY: `replyToApplicationShouldTerminate:` is thread-safe per Apple
    // documentation.  We bypass the `MainThreadMarker` guard on
    // `sharedApplication` using a raw class message, which is equivalent but
    // avoids the spurious compile-time main-thread requirement imposed by
    // the typed bindings.
    unsafe {
        let app: *mut AnyObject = msg_send![objc2::class!(NSApplication), sharedApplication];
        let _: () = msg_send![app, replyToApplicationShouldTerminate: proceed];
    }
}
