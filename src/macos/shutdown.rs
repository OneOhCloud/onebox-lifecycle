//! macOS shutdown backend — compiled only when the `shutdown` feature is enabled.
//!
//! Flow / 流程:
//! ```text
//! applicationShouldTerminate:
//!   → send SystemEvent::ShuttingDown(handle)
//!   → return NSTerminateLater          ← AppKit suspends the termination sequence
//!   → caller calls handle.allow()
//!       → background thread calls [NSApp replyToApplicationShouldTerminate: YES]
//!          (thread-safe: only posts a Mach message internally per Apple docs)
//! ```

use std::sync::{Arc, Mutex};

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Sel};
use objc2::{DefinedClass, MainThreadOnly, MainThreadMarker, define_class, msg_send};
use objc2_app_kit::{NSApplication, NSApplicationTerminateReply};
use objc2_foundation::NSObject;

use crate::common::EventSender;
use crate::common::SystemEvent;
use crate::common::shutdown::{ShutdownDecision, ShutdownHandle, ShutdownHandleInner};

/// Maximum time to wait for the caller's shutdown decision before allowing anyway.
///
/// ---
///
/// 等待调用方关机决策的最长时间，超时后默认允许关机。
const SHUTDOWN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

// ─── Delegate state ────────────────────────────────────────────────────────────

pub(super) struct DelegateState {
    pub(super) event_tx: Arc<EventSender>,
    /// The `NSApplicationDelegate` that was set before us (e.g. Tauri's deep-link
    /// delegate). Stored as a raw pointer because the previous owner retains it
    /// for the process lifetime. `null` if no prior delegate was set.
    ///
    /// ---
    ///
    /// 安装前已有的 `NSApplicationDelegate`（如 Tauri 的深链接 delegate）。
    /// 以原始指针保存，前任持有者负责其生命周期。若无前任则为 null。
    pub(super) previous_delegate: *mut AnyObject,
}

// SAFETY: delegate callbacks always arrive on the main thread;
// the pointer is never freed here, only used for ObjC messages on the same thread.
unsafe impl Send for DelegateState {}

// ─── NSApplicationDelegate ─────────────────────────────────────────────────────

define_class!(
    // SAFETY: NSObject subclassing requirements are met; struct does not implement Drop.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "SysSentinelDelegate"]
    #[ivars = Arc<Mutex<DelegateState>>]
    pub(super) struct SentinelDelegate;

    impl SentinelDelegate {
        /// Called by AppKit when the user or OS requests termination.
        ///
        /// Returns `NSTerminateLater` to suspend the termination sequence while the
        /// caller performs async cleanup. A background thread calls
        /// `replyToApplicationShouldTerminate:` once the decision arrives (or times out).
        ///
        /// Ref: [`NSApplicationDelegate.applicationShouldTerminate(_:)`](
        /// https://developer.apple.com/documentation/appkit/nsapplicationdelegate/applicationshouldterminate(_:))
        ///
        /// ---
        ///
        /// AppKit 在用户或 OS 请求终止时调用。返回 `NSTerminateLater` 挂起终止序列，
        /// 后台线程在收到决策（或超时）后调用 `replyToApplicationShouldTerminate:`。
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

            std::thread::spawn(move || {
                let _decision = decision_rx
                    .recv_timeout(SHUTDOWN_TIMEOUT)
                    .unwrap_or(ShutdownDecision::Allow);
                // Only Allow exists; always proceed with termination.
                reply_to_should_terminate(true);
            });

            NSApplicationTerminateReply::TerminateLater
        }

        /// Implements `application:openURLs:` directly to ensure AppKit always
        /// invokes it for hot-start deep links.
        ///
        /// We cannot rely solely on `forwardingTargetForSelector:` because WRY / Tauri
        /// may register the method via `class_addMethod` at runtime, causing
        /// `[prev respondsToSelector:]` to return `NO` even when the method exists.
        /// Direct implementation guarantees correct forwarding to the previous delegate.
        ///
        /// ---
        ///
        /// 直接实现 `application:openURLs:` 以确保 AppKit 始终在热启动深链接时调用它。
        ///
        /// 不能单靠 `forwardingTargetForSelector:`，因为 WRY/Tauri 可能在运行时通过
        /// `class_addMethod` 注册该方法，导致 `[prev respondsToSelector:]` 错误地
        /// 返回 `NO`，静默破坏转发链。直接实现可确保正确转发给前任 delegate。
        #[unsafe(method(application:openURLs:))]
        fn application_open_urls(&self, application: *mut AnyObject, urls: *mut AnyObject) {
            let prev = self.ivars().lock().unwrap().previous_delegate;
            if !prev.is_null() {
                unsafe { let _: () = msg_send![prev, application: application, openURLs: urls]; }
            }
        }

        /// Forwards any unhandled delegate message to the previous delegate.
        ///
        /// Ref: [`NSObject.forwardingTarget(for:)`](
        /// https://developer.apple.com/documentation/objectivec/nsobject/forwardingtarget(for:))
        ///
        /// ---
        ///
        /// 将未处理的 delegate 消息转发给前任 delegate。
        #[unsafe(method(forwardingTargetForSelector:))]
        fn forwarding_target_for_selector(&self, sel: Sel) -> *mut AnyObject {
            let prev = self.ivars().lock().unwrap().previous_delegate;
            if !prev.is_null() {
                let responds: bool = unsafe { msg_send![prev, respondsToSelector: sel] };
                if responds { return prev; }
            }
            std::ptr::null_mut()
        }

        /// Reports effective selector support: own methods plus whatever the previous
        /// delegate handles. AppKit consults this before sending optional delegate
        /// messages, so it must return `true` for selectors the previous delegate
        /// handles (e.g. `application:openURLs:`).
        ///
        /// ---
        ///
        /// 上报有效选择器支持：自身方法 + 前任 delegate 支持的方法。
        /// AppKit 在发送可选 delegate 消息前会检查此方法，必须对前任 delegate
        /// 处理的选择器（如 `application:openURLs:`）返回 `true`。
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
    fn new(mtm: MainThreadMarker, state: Arc<Mutex<DelegateState>>) -> Retained<Self> {
        let this = mtm.alloc::<Self>().set_ivars(state);
        unsafe { msg_send![super(this), init] }
    }
}

// ─── install ───────────────────────────────────────────────────────────────────

/// Install `SentinelDelegate` as the `NSApplication` delegate.
///
/// Captures the existing delegate (if any) and sets up transparent forwarding so
/// that other frameworks (e.g. Tauri) continue to receive delegate callbacks.
/// **Must be called from the main thread.**
///
/// ---
///
/// 将 `SentinelDelegate` 安装为 `NSApplication` 的 delegate。
///
/// 捕获已有 delegate（如有），设置透明转发以确保其他框架（如 Tauri）仍能收到
/// delegate 回调。**必须从主线程调用。**
pub(super) fn install(
    mtm: MainThreadMarker,
    event_tx: &Arc<EventSender>,
) -> Retained<SentinelDelegate> {
    let previous_delegate: *mut AnyObject = unsafe {
        let app = NSApplication::sharedApplication(mtm);
        msg_send![&*app, delegate]
    };

    let state = Arc::new(Mutex::new(DelegateState {
        event_tx: Arc::clone(event_tx),
        previous_delegate,
    }));

    let delegate = SentinelDelegate::new(mtm, state);
    unsafe {
        let app = NSApplication::sharedApplication(mtm);
        let delegate_obj = Retained::as_ptr(&delegate) as *const NSObject;
        let _: () = msg_send![&*app, setDelegate: delegate_obj];
    }
    delegate
}

// ─── Helper ────────────────────────────────────────────────────────────────────

/// Post `[NSApp replyToApplicationShouldTerminate: proceed]` from any thread.
///
/// Apple documents `replyToApplicationShouldTerminate:` as thread-safe — it only
/// posts a Mach message internally and has no UI side-effects that require the
/// main thread. We therefore call it from the background waiter thread directly.
///
/// Ref: [`NSApplication.reply(toApplicationShouldTerminate:)`](
/// https://developer.apple.com/documentation/appkit/nsapplication/reply(toapplicationshouldterminate:))
///
/// ---
///
/// 从任意线程发送 `[NSApp replyToApplicationShouldTerminate: proceed]`。
///
/// Apple 文档明确 `replyToApplicationShouldTerminate:` 是线程安全的——内部仅发送
/// Mach 消息，无需主线程。可直接在后台等待线程中调用。
fn reply_to_should_terminate(proceed: bool) {
    unsafe {
        let app: *mut AnyObject =
            msg_send![objc2::class!(NSApplication), sharedApplication];
        let _: () = msg_send![app, replyToApplicationShouldTerminate: proceed];
    }
}
