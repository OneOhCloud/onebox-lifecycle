//! macOS sleep/wake backend — compiled only when the `sleep` feature is enabled.
//!
//! Uses `NSWorkspace` notification-centre observers on the main thread.
//!
//! When the `shutdown` feature is also enabled, additionally observes
//! `NSWorkspaceWillPowerOffNotification` to provide an early power-off signal
//! that fires before `applicationShouldTerminate:`.

use std::sync::Arc;

use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::AnyObject;
use objc2::{DefinedClass, MainThreadOnly, MainThreadMarker, define_class, msg_send};
use objc2_foundation::{NSObject, NSString};

use crate::common::{EventSender, SystemEvent};

// Notification name constants used with NSWorkspace.notificationCenter.
// Ref: https://developer.apple.com/documentation/appkit/nsworkspace
const WILL_SLEEP_NOTIFICATION: &str = "NSWorkspaceWillSleepNotification";
const DID_WAKE_NOTIFICATION:   &str = "NSWorkspaceDidWakeNotification";

/// Posted when the user requests shutdown, restart, or logout.
/// Fires **before** `applicationShouldTerminate:`, while the system is still
/// fully operational — ideal for best-effort cleanup.
///
/// Ref: <https://developer.apple.com/documentation/appkit/nsworkspace/willpoweroffnotification>
///
/// ---
///
/// 用户请求关机、重启或注销时发送。在 `applicationShouldTerminate:` 之前触发，
/// 此时系统仍完全可用——适合执行尽力清理。
#[cfg(feature = "shutdown")]
const WILL_POWER_OFF_NOTIFICATION: &str = "NSWorkspaceWillPowerOffNotification";

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
            // Standard mode cannot delay sleep; emit an inert handle.
            // Standard 模式无法延迟睡眠，发出惰性 handle。
            self.ivars().send(SystemEvent::WillSleep);
        }

        #[unsafe(method(handleDidWake:))]
        fn handle_did_wake(&self, _notification: &AnyObject) {
            self.ivars().send(SystemEvent::DidWake);
        }

        /// Fires when the user initiates system shutdown, restart, or logout.
        /// This is an early notification — the OS may still cancel the operation.
        ///
        /// ---
        ///
        /// 用户发起关机、重启或注销时触发。这是一个早期通知——操作系统可能仍会取消操作。
        #[cfg(feature = "shutdown")]
        #[unsafe(method(handleWillPowerOff:))]
        fn handle_will_power_off(&self, _notification: &AnyObject) {
            self.ivars().send(SystemEvent::WillPowerOff);
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

// ─── install ───────────────────────────────────────────────────────────────────

/// Register `NSWorkspace` notification observers for sleep, wake, and
/// (when the `shutdown` feature is enabled) power-off.
///
/// **Must be called from the main thread.**
///
/// ---
///
/// 注册 `NSWorkspace` 通知观察者，监听睡眠、唤醒，以及
/// （当启用 `shutdown` feature 时）关机事件。
///
/// **必须从主线程调用。**
pub(super) fn install(
    mtm: MainThreadMarker,
    event_tx: &Arc<EventSender>,
) -> (Retained<PowerObserver>, NotificationObserverGuard) {
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

        // When the shutdown feature is enabled, also observe the power-off
        // notification. This fires *before* applicationShouldTerminate: and
        // works reliably for all apps including agent/accessory apps.
        //
        // 启用 shutdown feature 时，同时监听关机通知。该通知在
        // applicationShouldTerminate: 之前触发，对所有应用（包括 agent 应用）可靠。
        #[cfg(feature = "shutdown")]
        {
            let will_power_off = NSString::from_str(WILL_POWER_OFF_NOTIFICATION);
            let _: () = msg_send![nc,
                addObserver: observer_ptr,
                selector: objc2::sel!(handleWillPowerOff:),
                name: &*will_power_off,
                object: std::ptr::null::<AnyObject>()
            ];
        }

        NotificationObserverGuard { observer: observer_ptr }
    });
    (observer, guard)
}
