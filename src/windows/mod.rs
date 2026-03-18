//! Windows platform backend.
//!
//! Spawns a dedicated OS thread that owns a hidden `HWND` and runs
//! `GetMessageW` forever. All system events arrive as window messages.
//!
//! ---
//!
//! Windows 平台后端。
//!
//! 启动一个专用 OS 线程，持有隐藏的 `HWND` 并永久运行 `GetMessageW`。
//! 所有系统事件均以窗口消息形式到达。

use std::cell::RefCell;

use windows::{
    Win32::{
        Foundation::{HWND, LPARAM, LRESULT, WPARAM},
        UI::WindowsAndMessaging::{
            CREATESTRUCTW, CreateWindowExW, DefWindowProcW, DispatchMessageW, GWLP_USERDATA,
            GetMessageW, GetWindowLongPtrW, MSG, RegisterClassExW, SetWindowLongPtrW,
            TranslateMessage, UnregisterClassW, WM_DESTROY, WM_NCCREATE,
            WNDCLASSEXW, WS_EX_TOOLWINDOW, WS_POPUP,
        },
    },
    core::{PCWSTR, w},
};

#[cfg(feature = "shutdown")]
use std::sync::{Arc, Condvar, Mutex};

#[cfg(feature = "shutdown")]
use windows::Win32::UI::WindowsAndMessaging::WM_QUERYENDSESSION;

#[cfg(feature = "sleep")]
use windows::Win32::UI::WindowsAndMessaging::WM_POWERBROADCAST;

use crate::common::EventSender;
#[cfg(feature = "sleep")]
use crate::common::SystemEvent;

// ─── Feature submodules ────────────────────────────────────────────────────────

#[cfg(feature = "shutdown")]
mod shutdown;

#[cfg(feature = "network")]
mod network;

// ─── Sleep constants (too small to warrant a submodule) ───────────────────────

/// `PBT_APMSUSPEND` — system is suspending.
/// Ref: <https://learn.microsoft.com/windows/win32/power/pbt-apmsuspend>
#[cfg(feature = "sleep")]
const PBT_APMSUSPEND: u32 = 4;

/// `PBT_APMRESUMESUSPEND` — system has resumed from suspend.
/// Ref: <https://learn.microsoft.com/windows/win32/power/pbt-apmresumesuspend>
#[cfg(feature = "sleep")]
const PBT_APMRESUMESUSPEND: u32 = 7;

// Compile-time UTF-16 class name — no heap allocation.
const CLASS_NAME: PCWSTR = w!("SysSentinelHidden");

// ─── Window state ──────────────────────────────────────────────────────────────

/// Per-window state accessed exclusively from the message-loop thread.
///
/// `RefCell` (not `Mutex`) — wndproc is single-threaded; `RefCell` has zero
/// locking overhead and cannot deadlock the message pump.
///
/// ---
///
/// 仅从消息循环线程访问的窗口状态。
///
/// 使用 `RefCell`（而非 `Mutex`）——wndproc 单线程运行，无锁开销，不会死锁消息泵。
pub(super) struct WindowState {
    pub(super) event_tx: EventSender,

    #[cfg(feature = "shutdown")]
    pub(super) pending_shutdown_hwnd: Option<HWND>,

    #[cfg(feature = "shutdown")]
    pub(super) shutdown_notify: Option<Arc<(Mutex<bool>, Condvar)>>,

    /// `None` = not yet delivered.
    #[cfg(feature = "network")]
    pub(super) last_network_up: Option<bool>,
}

// ─── Entry point ──────────────────────────────────────────────────────────────

/// Spawn the Win32 sentinel thread. Returns immediately.
///
/// ---
///
/// 启动 Win32 哨兵线程，立即返回。
pub fn start(event_tx: EventSender) {
    std::thread::Builder::new()
        .name("onebox_lifecycle_win32".into())
        .spawn(move || unsafe { run_message_loop(event_tx) })
        .expect("onebox_lifecycle: failed to spawn Win32 message thread");
}

// ─── Message loop ──────────────────────────────────────────────────────────────

unsafe fn run_message_loop(event_tx: EventSender) {
    unsafe {
        // ── 1. Register the window class ─────────────────────────────────────
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wndproc),
            lpszClassName: CLASS_NAME,
            ..Default::default()
        };
        let atom = RegisterClassExW(&wc);
        assert!(
            atom != 0,
            "onebox_lifecycle: RegisterClassExW failed: {:?}",
            windows::core::Error::from_thread()
        );

        // ── 2. Create a hidden (message-only) window ──────────────────────────
        let state = Box::new(RefCell::new(WindowState {
            event_tx,
            #[cfg(feature = "shutdown")]
            pending_shutdown_hwnd: None,
            #[cfg(feature = "shutdown")]
            shutdown_notify: None,
            #[cfg(feature = "network")]
            last_network_up: None,
        }));
        let state_ptr = Box::into_raw(state);

        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW, // hidden from taskbar and Alt+Tab
            CLASS_NAME,
            w!("SysSentinel"),
            WS_POPUP,  // no caption or border; zero size, never shown
            0, 0, 0, 0,
            None, // top-level — receives WM_POWERBROADCAST & WM_QUERYENDSESSION
            None,
            None,
            Some(state_ptr as *const _),
        )
        .expect("onebox_lifecycle: CreateWindowExW failed");

        // ── 3. Feature-specific setup ────────────────────────────────────────
        #[cfg(feature = "shutdown")]
        shutdown::setup();

        #[cfg(feature = "network")]
        let net_notify_handle = network::setup(hwnd);

        // ── 4. Pump messages ──────────────────────────────────────────────────
        let mut msg = MSG::default();
        loop {
            let ret = GetMessageW(&mut msg, None, 0, 0);
            if ret.0 == 0 || ret.0 == -1 { break; }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        // ── 5. Cleanup ────────────────────────────────────────────────────────
        #[cfg(feature = "network")]
        network::teardown(net_notify_handle);

        // Guard against double-free: WM_DESTROY already zeroes GWLP_USERDATA.
        let live_ptr =
            GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut RefCell<WindowState>;
        if !live_ptr.is_null() {
            drop(Box::from_raw(live_ptr));
        }
        let _ = UnregisterClassW(CLASS_NAME, None);
    }
}

// ─── Window procedure ──────────────────────────────────────────────────────────

unsafe extern "system" fn wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_NCCREATE {
        let cs = unsafe { &*(lparam.0 as *const CREATESTRUCTW) };
        unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, cs.lpCreateParams as isize) };
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    }

    let state_ptr =
        unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut RefCell<WindowState>;

    if msg == WM_DESTROY {
        if !state_ptr.is_null() {
            unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0) };
            drop(unsafe { Box::from_raw(state_ptr) });
        }
        return LRESULT(0);
    }

    if state_ptr.is_null() {
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    }

    // SAFETY: state_ptr is valid for the window's lifetime; wndproc always runs
    // on the message-loop thread, so RefCell's single-threaded invariant holds.
    let state_cell = unsafe { &*state_ptr };

    match msg {
        // ── Shutdown query ───────────────────────────────────────────────────
        #[cfg(feature = "shutdown")]
        WM_QUERYENDSESSION => shutdown::handle_query(hwnd, state_cell),

        // ── Async cleanup finished ────────────────────────────────────────────
        #[cfg(feature = "shutdown")]
        shutdown::WM_SENTINEL_ALLOW_SHUTDOWN => shutdown::handle_allow(state_cell),

        // ── Power events ─────────────────────────────────────────────────────
        #[cfg(feature = "sleep")]
        WM_POWERBROADCAST => {
            let st = state_cell.borrow();
            match wparam.0 as u32 {
                PBT_APMSUSPEND       => st.event_tx.send(SystemEvent::WillSleep),
                PBT_APMRESUMESUSPEND => st.event_tx.send(SystemEvent::DidWake),
                _ => {}
            }
            LRESULT(1)
        }

        // ── Network change ────────────────────────────────────────────────────
        #[cfg(feature = "network")]
        network::WM_SENTINEL_NETWORK_CHANGE => network::handle_change(wparam, state_cell),

        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}
