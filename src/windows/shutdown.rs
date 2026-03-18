//! Windows shutdown backend — compiled only when the `shutdown` feature is enabled.
//!
//! Flow / 流程:
//! ```text
//! WM_QUERYENDSESSION
//!   → send SystemEvent::ShuttingDown(handle)
//!   → caller calls handle.allow()   → return TRUE  (OS proceeds immediately)
//!   → caller calls handle.block(s)  → ShutdownBlockReasonCreate, return FALSE
//!       → background thread waits on condvar
//!       → caller later calls post_allow_shutdown(hwnd)
//!           → WM_SENTINEL_ALLOW_SHUTDOWN → signals condvar
//!           → background thread: ShutdownBlockReasonDestroy
//!           → OS re-sends WM_QUERYENDSESSION; allow() → TRUE
//! ```

use std::cell::RefCell;
use std::sync::{Arc, Condvar, Mutex};

use windows::{
    Win32::{
        Foundation::{HWND, LPARAM, LRESULT, WPARAM},
        System::{
            Shutdown::{ShutdownBlockReasonCreate, ShutdownBlockReasonDestroy},
            Threading::SetProcessShutdownParameters,
        },
        UI::WindowsAndMessaging::{PostMessageW, WM_APP},
    },
    core::PCWSTR,
};

use crate::common::SystemEvent;
use crate::common::shutdown::{ShutdownDecision, ShutdownHandle, ShutdownHandleInner};

/// Custom window message: posted by `post_allow_shutdown` to signal that the
/// caller has finished cleanup and the blocked shutdown may proceed.
///
/// ---
///
/// 自定义窗口消息：由 `post_allow_shutdown` 发送，通知已完成清理，阻塞的关机可以继续。
pub(super) const WM_SENTINEL_ALLOW_SHUTDOWN: u32 = WM_APP + 1;

// ─── Setup ────────────────────────────────────────────────────────────────────

/// Set the process shutdown priority so we are notified before most user-space apps.
///
/// Level 0x3FF (1023) places us near the front of the shutdown notification queue.
/// Flags = 0 means `SHUTDOWN_NORETRY` is **not** set, allowing the OS to re-send
/// `WM_QUERYENDSESSION` after we call `ShutdownBlockReasonDestroy`.
///
/// Ref: [`SetProcessShutdownParameters`](
/// https://learn.microsoft.com/windows/win32/api/processthreadsapi/nf-processthreadsapi-setprocessshutdownparameters)
///
/// ---
///
/// 设置进程关机优先级，确保在大多数用户态应用前收到通知。
///
/// 级别 0x3FF (1023) 将进程置于关机通知队列前端。Flags = 0 表示未设置
/// `SHUTDOWN_NORETRY`，允许 OS 在我们调用 `ShutdownBlockReasonDestroy` 后
/// 重新发送 `WM_QUERYENDSESSION`。
pub(super) fn setup() {
    unsafe { let _ = SetProcessShutdownParameters(0x3FF, 0); }
}

// ─── WM_QUERYENDSESSION handler ────────────────────────────────────────────────

/// Handle `WM_QUERYENDSESSION` — blocks up to 2 s for the caller's decision.
///
/// Returns `LRESULT(1)` (TRUE = allow) or `LRESULT(0)` (FALSE = block this round).
/// If no decision arrives within 2 seconds, defaults to blocking (safer than
/// silently allowing shutdown mid-cleanup).
///
/// Ref: [`WM_QUERYENDSESSION`](
/// https://learn.microsoft.com/windows/win32/shutdown/wm-queryendsession)
///
/// ---
///
/// 处理 `WM_QUERYENDSESSION`——最多等待 2 秒等待调用方决策。
///
/// 返回 `LRESULT(1)`（TRUE = 允许）或 `LRESULT(0)`（FALSE = 本轮阻塞）。
/// 2 秒内无决策则默认阻塞（比默默允许关机更安全）。
pub(super) fn handle_query(
    hwnd: HWND,
    state_cell: &RefCell<super::WindowState>,
) -> LRESULT {
    let (decision_tx, decision_rx) = std::sync::mpsc::sync_channel::<ShutdownDecision>(1);

    let handle = ShutdownHandle {
        inner: Some(ShutdownHandleInner::Mpsc(decision_tx)),
    };

    {
        let mut st = state_cell.borrow_mut();
        st.pending_shutdown_hwnd = Some(hwnd);
        st.event_tx.send(SystemEvent::ShuttingDown(handle));
    } // borrow released before recv_timeout blocks

    let decision = decision_rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .unwrap_or(ShutdownDecision::Block {
            reason: Some("onebox_lifecycle: cleanup in progress".into()),
        });

    match decision {
        ShutdownDecision::Allow => {
            state_cell.borrow_mut().pending_shutdown_hwnd = None;
            LRESULT(1) // TRUE → allow this round
        }
        ShutdownDecision::Block { reason } => {
            let reason_str = reason.unwrap_or_else(|| "Cleanup in progress…".into());
            let wide: Vec<u16> = reason_str
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect();
            // ShutdownBlockReasonCreate copies the string internally.
            // Ref: https://learn.microsoft.com/windows/win32/api/shutdownreason/nf-shutdownreason-shutdownblockreasoncreate
            unsafe { let _ = ShutdownBlockReasonCreate(hwnd, PCWSTR(wide.as_ptr())); }

            let notify = Arc::new((Mutex::new(false), Condvar::new()));
            let notify_clone = Arc::clone(&notify);
            state_cell.borrow_mut().shutdown_notify = Some(notify);

            // Cast hwnd.0 to usize so the closure is `Send`.
            let hwnd_usize = hwnd.0 as usize;
            std::thread::spawn(move || {
                let (lock, cvar) = &*notify_clone;
                let guard = lock.lock().unwrap();
                // Block until allow() is signalled or the 5-minute safety timeout.
                let _ = cvar.wait_timeout_while(
                    guard,
                    std::time::Duration::from_secs(300),
                    |&mut done| !done,
                );
                // Only this thread calls ShutdownBlockReasonDestroy.
                // Ref: https://learn.microsoft.com/windows/win32/api/shutdownreason/nf-shutdownreason-shutdownblockreasondestroy
                unsafe { let _ = ShutdownBlockReasonDestroy(HWND(hwnd_usize as *mut _)); }
            });

            LRESULT(0) // FALSE → block this round
        }
    }
}

// ─── WM_SENTINEL_ALLOW_SHUTDOWN handler ───────────────────────────────────────

/// Handle `WM_SENTINEL_ALLOW_SHUTDOWN` — signals the background watcher thread.
///
/// ---
///
/// 处理 `WM_SENTINEL_ALLOW_SHUTDOWN`——向后台等待线程发出信号。
pub(super) fn handle_allow(state_cell: &RefCell<super::WindowState>) -> LRESULT {
    let mut st = state_cell.borrow_mut();
    st.pending_shutdown_hwnd = None;
    if let Some(notify) = st.shutdown_notify.take() {
        let (lock, cvar) = &*notify;
        *lock.lock().unwrap() = true;
        cvar.notify_one();
    }
    LRESULT(0)
}

// ─── Public helper ─────────────────────────────────────────────────────────────

/// Post the "allow shutdown" signal to the hidden sentinel window from any thread.
///
/// Call this once async cleanup is complete. This triggers `ShutdownBlockReasonDestroy`
/// on the watcher thread, after which the OS re-issues `WM_QUERYENDSESSION` and the
/// next [`ShutdownHandle::allow`] call returns `TRUE`.
///
/// Equivalent to `[NSApp replyToApplicationShouldTerminate: YES]` on macOS.
///
/// Ref: [`PostMessageW`](
/// https://learn.microsoft.com/windows/win32/api/winuser/nf-winuser-postmessagew)
///
/// ---
///
/// 从任意线程向隐藏哨兵窗口发送"允许关机"信号。
///
/// 异步清理完成后调用。触发等待线程的 `ShutdownBlockReasonDestroy`，之后 OS
/// 重新发送 `WM_QUERYENDSESSION`，下次 [`ShutdownHandle::allow`] 返回 `TRUE`。
///
/// 等效于 macOS 上的 `[NSApp replyToApplicationShouldTerminate: YES]`。
#[allow(dead_code)]
pub fn post_allow_shutdown(hwnd: HWND) {
    unsafe {
        let _ = PostMessageW(
            Some(hwnd),
            WM_SENTINEL_ALLOW_SHUTDOWN,
            WPARAM(0),
            LPARAM(0),
        );
    }
}
