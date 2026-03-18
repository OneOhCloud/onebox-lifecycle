//! macOS sleep/wake backend — compiled only when the `sleep` feature is enabled.
//!
//! Two monitoring modes / 两种监控模式:
//!
//! **Standard** — `NSWorkspace` notification-centre observers.
//!   No dedicated thread; no ability to delay sleep.
//!
//! **Deep** — IOKit `IORegisterForSystemPower` on a dedicated CFRunLoop thread.
//!   Fires `kIOMessageSystemWillSleep` before any sleep transition (including
//!   hibernation), allowing the caller to delay sleep via [`SleepHandle`].
//!   Fires `kIOMessageSystemHasPoweredOn` on wake.

use std::sync::{Arc, atomic::{AtomicU32, Ordering}};

use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::AnyObject;
use objc2::{DefinedClass, MainThreadOnly, MainThreadMarker, define_class, msg_send};
use objc2_foundation::{NSObject, NSString};

use crate::common::{EventSender, SystemEvent};
use crate::common::sleep::{SleepHandle, SleepMonitorLevel};

// Notification name constants used with NSWorkspace.notificationCenter.
// Ref: https://developer.apple.com/documentation/appkit/nsworkspace
const WILL_SLEEP_NOTIFICATION: &str = "NSWorkspaceWillSleepNotification";
const DID_WAKE_NOTIFICATION:   &str = "NSWorkspaceDidWakeNotification";

// ─── PowerObserver (Standard mode) ────────────────────────────────────────────

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "SysSentinelPowerObserver"]
    #[ivars = Arc<EventSender>]
    pub(super) struct PowerObserver;

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

impl PowerObserver {
    fn new(mtm: MainThreadMarker, event_tx: Arc<EventSender>) -> Retained<Self> {
        let this = mtm.alloc::<Self>().set_ivars(event_tx);
        unsafe { msg_send![super(this), init] }
    }
}

// ─── NotificationObserverGuard ─────────────────────────────────────────────────

/// Calls `[NSNotificationCenter removeObserver:]` on drop to prevent stale callbacks.
///
/// `NSNotificationCenter` retains observers since macOS 10.x; failing to call
/// `removeObserver:` keeps the object alive indefinitely and may fire callbacks
/// into a logically-dead observer.
///
/// ---
///
/// 析构时调用 `[NSNotificationCenter removeObserver:]`，防止过期回调。
///
/// macOS 10.x 起 `NSNotificationCenter` 会持有观察者；不移除会导致内存泄漏，
/// 且可能向已逻辑失效的观察者发送回调。
pub(super) struct NotificationObserverGuard {
    /// Non-owning pointer; kept alive by `MacosGuard._power_observer`.
    observer: *const NSObject,
}

impl Drop for NotificationObserverGuard {
    fn drop(&mut self) {
        // SAFETY: NSWorkspace and its notification centre are process-lifetime
        // singletons. `removeObserver:` is documented thread-safe by Apple.
        unsafe {
            let workspace: *mut AnyObject =
                msg_send![objc2::class!(NSWorkspace), sharedWorkspace];
            let nc: *mut AnyObject = msg_send![workspace, notificationCenter];
            let _: () = msg_send![nc, removeObserver: self.observer];
        }
    }
}

// SAFETY: `removeObserver:` on NSNotificationCenter is documented thread-safe.
unsafe impl Send for NotificationObserverGuard {}
unsafe impl Sync for NotificationObserverGuard {}

// ─── IOKit FFI (Deep mode) ────────────────────────────────────────────────────
//
// IOKit's power-notification API lets the caller delay system sleep until it
// explicitly calls `IOAllowPowerChange`.
//
// Ref: https://developer.apple.com/documentation/iokit/iokit_functions
// Ref: IOKit/pwr_mgt/IOPMLib.h

#[allow(non_camel_case_types, non_upper_case_globals)]
mod iokit_ffi {
    use std::ffi::c_void;

    /// Underlying type for both `io_object_t` and `io_connect_t`.
    pub type io_object_t  = u32;
    pub type io_connect_t = io_object_t;
    /// Opaque handle returned by `IORegisterForSystemPower`.
    pub type IONotificationPortRef = *mut c_void;

    /// `kIOMessageSystemWillSleep` — system is about to sleep; must acknowledge
    /// with `IOAllowPowerChange`. Value: `iokit_common_msg(0x280)`.
    ///
    /// ---
    ///
    /// 系统即将睡眠，必须调用 `IOAllowPowerChange` 确认。值：`iokit_common_msg(0x280)`。
    pub const kIOMessageSystemWillSleep:    u32 = 0xe000_0280;

    /// `kIOMessageSystemHasPoweredOn` — system has fully woken.
    /// Value: `iokit_common_msg(0x300)`.
    ///
    /// ---
    ///
    /// 系统已完全唤醒。值：`iokit_common_msg(0x300)`。
    pub const kIOMessageSystemHasPoweredOn: u32 = 0xe000_0300;

    /// `IOServiceInterestCallback` — signature for the IOKit power callback.
    pub type IOServiceInterestCallback = unsafe extern "C" fn(
        refcon:           *mut c_void,
        service:          io_object_t,
        message_type:     u32,
        message_argument: *mut c_void,
    );

    #[link(name = "IOKit", kind = "framework")]
    unsafe extern "C" {
        /// Register for sleep/wake messages on the root power domain.
        /// Returns the `io_connect_t` (kernel port) needed by `IOAllowPowerChange`.
        ///
        /// ---
        ///
        /// 向根电源域注册睡眠/唤醒消息。返回 `io_connect_t`（内核端口），
        /// 供 `IOAllowPowerChange` 使用。
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

        /// Allow the power transition to proceed. Thread-safe per Apple docs.
        ///
        /// ---
        ///
        /// 允许电源转换继续。Apple 文档明确其线程安全。
        pub fn IOAllowPowerChange(
            kernel_port:     io_connect_t,
            notification_id: *mut c_void, // messageArgument
        ) -> i32;

        /// Unregister the notifier from the power domain.
        pub fn IODeregisterForSystemPower(notifier: *mut io_object_t) -> i32;

        /// Decrement the IOKit retain count on an object.
        pub fn IOObjectRelease(object: io_object_t) -> i32;

        /// Destroy the notification port and release Mach port resources.
        pub fn IONotificationPortDestroy(notify: IONotificationPortRef);
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        /// Returns the `CFRunLoopRef` for the calling thread.
        pub fn CFRunLoopGetCurrent() -> *mut c_void;

        /// Add a source to a run loop in the given mode.
        pub fn CFRunLoopAddSource(
            rl:     *mut c_void,   // CFRunLoopRef
            source: *mut c_void,   // CFRunLoopSourceRef
            mode:   *const c_void, // CFStringRef
        );

        /// Run the current thread's run loop until `CFRunLoopStop` is called.
        pub fn CFRunLoopRun();

        /// Stop the given run loop. Thread-safe per Apple docs.
        ///
        /// ---
        ///
        /// 停止指定 run loop。Apple 文档明确其线程安全。
        pub fn CFRunLoopStop(rl: *mut c_void);

        pub fn CFRetain(cf: *const c_void)  -> *const c_void;
        pub fn CFRelease(cf: *const c_void);

        /// `kCFRunLoopDefaultMode` — the default run-loop mode string.
        pub static kCFRunLoopDefaultMode: *const c_void;
    }
}

// ─── IOKit callback context ────────────────────────────────────────────────────

/// Shared state passed to the C callback via the `refcon` pointer.
/// Heap-allocated behind an `Arc`; the background thread borrows it as `&Self`.
///
/// ---
///
/// 通过 `refcon` 传递给 C 回调的共享状态，由 `Arc` 持有；后台线程以 `&Self` 借用。
struct IoKitCallbackContext {
    event_tx:    Arc<EventSender>,
    /// Written by the background thread right after `IORegisterForSystemPower`
    /// returns, before `CFRunLoopRun()` starts. Read inside the callback.
    ///
    /// ---
    ///
    /// 后台线程在 `IORegisterForSystemPower` 返回后、`CFRunLoopRun()` 启动前写入，
    /// 在回调中读取。
    kernel_port: AtomicU32,
    timeout:     std::time::Duration,
}

// ─── IOKit power callback ──────────────────────────────────────────────────────

/// C callback fired on the dedicated IOKit CFRunLoop thread.
///
/// For `kIOMessageSystemWillSleep` the callback sends [`SystemEvent::WillHibernate`]
/// and blocks (up to `timeout`) waiting for the caller to call
/// [`SleepHandle::allow`]. `IOAllowPowerChange` is always called afterwards so
/// the OS is never permanently blocked.
///
/// ---
///
/// 在 IOKit 专用 CFRunLoop 线程上触发的 C 回调。
///
/// 收到 `kIOMessageSystemWillSleep` 时发送 [`SystemEvent::WillHibernate`] 并
/// 阻塞（最多 `timeout`）等待调用方调用 [`SleepHandle::allow`]。之后始终调用
/// `IOAllowPowerChange`，OS 不会被永久阻塞。
unsafe extern "C" fn iokit_power_callback(
    refcon:           *mut std::ffi::c_void,
    _service:         iokit_ffi::io_object_t,
    message_type:     u32,
    message_argument: *mut std::ffi::c_void,
) {
    // SAFETY: `refcon` points to an `Arc<IoKitCallbackContext>` kept alive by
    // `IoKitDeepSleepGuard._ctx` for the entire CFRunLoop lifetime.
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

/// RAII guard — stops the CFRunLoop thread and releases all IOKit resources on drop.
///
/// ---
///
/// RAII 守卫——析构时停止 CFRunLoop 线程并释放所有 IOKit 资源。
pub(super) struct IoKitDeepSleepGuard {
    /// Retained `CFRunLoopRef` of the background thread.
    run_loop:          *mut std::ffi::c_void,
    notifier:          iokit_ffi::io_object_t,
    notification_port: iokit_ffi::IONotificationPortRef,
    /// Joined before IOKit resources are released so any in-flight callback
    /// completes before `_ctx` drops and the `Arc` refcount reaches zero.
    ///
    /// ---
    ///
    /// 在释放 IOKit 资源前先 join，确保进行中的回调在 `_ctx` 析构（Arc 引用归零）前完成。
    _thread: Option<std::thread::JoinHandle<()>>,
    /// Keeps `IoKitCallbackContext` alive until the thread exits.
    _ctx:    Arc<IoKitCallbackContext>,
}

impl Drop for IoKitDeepSleepGuard {
    fn drop(&mut self) {
        unsafe { iokit_ffi::CFRunLoopStop(self.run_loop); }
        if let Some(t) = self._thread.take() { let _ = t.join(); }
        unsafe {
            iokit_ffi::CFRelease(self.run_loop as *const _);
            iokit_ffi::IODeregisterForSystemPower(&mut self.notifier);
            iokit_ffi::IOObjectRelease(self.notifier);
            iokit_ffi::IONotificationPortDestroy(self.notification_port);
        }
    }
}

// SAFETY: All IOKit/CoreFoundation cleanup APIs used in `Drop` are documented
// thread-safe by Apple. Raw pointers are accessed only in `Drop` (runs once).
unsafe impl Send for IoKitDeepSleepGuard {}
unsafe impl Sync for IoKitDeepSleepGuard {}

// ─── IOKit thread init result ──────────────────────────────────────────────────

/// Values sent from the background thread to the spawner after successful init.
/// Raw pointers are stored as `usize` so the struct is trivially `Send`.
///
/// ---
///
/// 后台线程初始化成功后发给调用方的数据。原始指针存为 `usize` 使结构体满足 `Send`。
struct IoKitInitResult {
    run_loop:          usize,
    notifier:          iokit_ffi::io_object_t,
    notification_port: usize,
}

// ─── Deep-sleep constructor ────────────────────────────────────────────────────

/// Spawn the dedicated CFRunLoop thread and register for IOKit power notifications.
///
/// Emits [`SystemEvent::WillHibernate`] with a [`SleepHandle`] before each sleep
/// transition. The thread blocks in `CFRunLoopRun()` until the guard is dropped.
///
/// ---
///
/// 启动专用 CFRunLoop 线程并注册 IOKit 电源通知。
///
/// 每次睡眠转换前发送带 [`SleepHandle`] 的 [`SystemEvent::WillHibernate`]。
/// 线程阻塞在 `CFRunLoopRun()` 中，直到守卫被析构。
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
    // Store the raw pointer as `usize` so the closure capture is `Send`.
    // The Arc is kept alive by `ctx_for_guard`.
    let raw_ctx_usize = Arc::into_raw(ctx) as usize;

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

            // Write kernel_port before CFRunLoopRun() — the callback fires only after.
            let ctx_ref = &*(raw as *const IoKitCallbackContext);
            ctx_ref.kernel_port.store(kernel_port, Ordering::Release);

            // Retain the run loop reference so Drop can stop it from any thread.
            let rl = CFRunLoopGetCurrent();
            CFRetain(rl as *const _);

            let source = IONotificationPortGetRunLoopSource(notification_port);
            CFRunLoopAddSource(rl, source, kCFRunLoopDefaultMode);

            let _ = init_tx.send(IoKitInitResult {
                run_loop:          rl as usize,
                notifier,
                notification_port: notification_port as usize,
            });

            // Blocks until IoKitDeepSleepGuard::drop() calls CFRunLoopStop(rl).
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

// ─── install ───────────────────────────────────────────────────────────────────

/// Set up sleep/wake monitoring according to `level` and return the RAII guards.
///
/// - `Standard` → registers two `NSWorkspace` notification observers.
/// - `Deep` → spawns the IOKit CFRunLoop thread.
///
/// **Must be called from the main thread.**
///
/// ---
///
/// 按 `level` 建立睡眠/唤醒监控并返回 RAII 守卫。
///
/// - `Standard`：注册两个 `NSWorkspace` 通知观察者。
/// - `Deep`：启动 IOKit CFRunLoop 线程。
///
/// **必须从主线程调用。**
pub(super) fn install(
    mtm: MainThreadMarker,
    event_tx: &Arc<EventSender>,
    level: SleepMonitorLevel,
) -> (
    Option<Retained<PowerObserver>>,
    Option<NotificationObserverGuard>,
    Option<IoKitDeepSleepGuard>,
) {
    match level {
        SleepMonitorLevel::Standard => {
            let observer = PowerObserver::new(mtm, Arc::clone(event_tx));
            let guard = autoreleasepool(|_pool| unsafe {
                let workspace: *mut AnyObject =
                    msg_send![objc2::class!(NSWorkspace), sharedWorkspace];
                let nc: *mut AnyObject = msg_send![workspace, notificationCenter];

                let will_sleep = NSString::from_str(WILL_SLEEP_NOTIFICATION);
                let did_wake   = NSString::from_str(DID_WAKE_NOTIFICATION);
                let observer_ptr = Retained::as_ptr(&observer) as *const NSObject;

                let _: () = msg_send![nc,
                    addObserver: observer_ptr,
                    selector: objc2::sel!(handleWillSleep:),
                    name: &*will_sleep,
                    object: std::ptr::null::<AnyObject>()
                ];
                let _: () = msg_send![nc,
                    addObserver: observer_ptr,
                    selector: objc2::sel!(handleDidWake:),
                    name: &*did_wake,
                    object: std::ptr::null::<AnyObject>()
                ];

                NotificationObserverGuard { observer: observer_ptr }
            });
            (Some(observer), Some(guard), None)
        }
        SleepMonitorLevel::Deep { timeout } => {
            let guard = start_iokit_deep_sleep(Arc::clone(event_tx), timeout);
            (None, None, Some(guard))
        }
    }
}
