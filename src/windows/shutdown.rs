//! Windows shutdown backend — compiled only when the `shutdown` feature is enabled.
//!
//! Flow / 流程:
//! ```text
//! WM_QUERYENDSESSION
//!   → send SystemEvent::ShuttingDown(handle)
//!   → caller calls handle.allow()   → return TRUE  (OS proceeds immediately)
//!   → timeout (2 s, no decision)    → default to Allow, return TRUE
//! ```

use std::cell::RefCell;

use windows::{
    Win32::{
        Foundation::{HWND, LRESULT},
        System::Threading::SetProcessShutdownParameters,
    },
};

use crate::common::SystemEvent;
use crate::common::shutdown::{ShutdownDecision, ShutdownHandle, ShutdownHandleInner};

// ─── Setup ────────────────────────────────────────────────────────────────────

/// Set the process shutdown priority so we are notified before most user-space apps.
///
/// Level 0x3FF (1023) places us near the front of the shutdown notification queue.
/// Flags = 0 means `SHUTDOWN_NORETRY` is **not** set, allowing the OS to re-send
/// `WM_QUERYENDSESSION`.
///
/// Ref: [`SetProcessShutdownParameters`](
/// https://learn.microsoft.com/windows/win32/api/processthreadsapi/nf-processthreadsapi-setprocessshutdownparameters)
///
/// ---
///
/// 设置进程关机优先级，确保在大多数用户态应用前收到通知。
///
/// 级别 0x3FF (1023) 将进程置于关机通知队列前端。Flags = 0 表示未设置
/// `SHUTDOWN_NORETRY`，允许 OS 重新发送 `WM_QUERYENDSESSION`。
pub(super) fn setup() {
    unsafe { let _ = SetProcessShutdownParameters(0x3FF, 0); }
}

// ─── WM_QUERYENDSESSION handler ────────────────────────────────────────────────

/// Handle `WM_QUERYENDSESSION` — waits up to 2 s for the caller's decision.
///
/// Returns `LRESULT(1)` (TRUE = allow). If no decision arrives within 2 seconds,
/// defaults to allowing shutdown.
///
/// Ref: [`WM_QUERYENDSESSION`](
/// https://learn.microsoft.com/windows/win32/shutdown/wm-queryendsession)
///
/// ---
///
/// 处理 `WM_QUERYENDSESSION`——最多等待 2 秒等待调用方决策。
///
/// 返回 `LRESULT(1)`（TRUE = 允许）。2 秒内无决策则默认允许关机。
pub(super) fn handle_query(
    _hwnd: HWND,
    state_cell: &RefCell<super::WindowState>,
) -> LRESULT {
    let (decision_tx, decision_rx) = std::sync::mpsc::sync_channel::<ShutdownDecision>(1);

    let handle = ShutdownHandle {
        inner: Some(ShutdownHandleInner::Mpsc(decision_tx)),
    };

    {
        let st = state_cell.borrow();
        st.event_tx.send(SystemEvent::ShuttingDown(handle));
    }

    let _decision = decision_rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .unwrap_or(ShutdownDecision::Allow);

    LRESULT(1) // TRUE → allow
}
