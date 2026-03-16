/// Windows platform backend.
///
/// Spawns a dedicated OS thread that owns a hidden `HWND` and runs
/// `GetMessageW` forever.  All system events arrive as window messages.
///
/// # Shutdown flow
///
/// ```text
/// WM_QUERYENDSESSION
///   → send SystemEvent::ShuttingDown(handle)
///   → caller calls handle.block("reason")   ← we return FALSE + ShutdownBlockReasonCreate
///                                              background thread waits on condvar
///   OR caller calls handle.allow()           ← we return TRUE (OS proceeds)
///   → when cleanup done: call post_allow_shutdown(hwnd)
///       → WM_SENTINEL_ALLOW_SHUTDOWN handler signals condvar
///       → background thread wakes and calls ShutdownBlockReasonDestroy
///       → OS re-sends WM_QUERYENDSESSION; this time allow() → TRUE
/// ```
use std::cell::RefCell;
#[cfg(feature = "shutdown")]
use std::sync::{Arc, Condvar, Mutex};

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

#[cfg(any(feature = "shutdown", feature = "network"))]
use windows::Win32::{
    Foundation::HANDLE,
    UI::WindowsAndMessaging::{PostMessageW, WM_APP},
};

#[cfg(feature = "shutdown")]
use windows::Win32::{
    System::{
        Shutdown::{ShutdownBlockReasonCreate, ShutdownBlockReasonDestroy},
        Threading::SetProcessShutdownParameters,
    },
    UI::WindowsAndMessaging::WM_QUERYENDSESSION,
};

#[cfg(feature = "sleep")]
use windows::Win32::UI::WindowsAndMessaging::WM_POWERBROADCAST;

#[cfg(feature = "network")]
use windows::Win32::{
    NetworkManagement::IpHelper::{
        CancelMibChangeNotify2, NotifyNetworkConnectivityHintChange,
    },
    Networking::WinSock::{
        NL_NETWORK_CONNECTIVITY_HINT, NetworkConnectivityLevelHintConstrainedInternetAccess,
        NetworkConnectivityLevelHintInternetAccess,
    },
};

use crate::common::EventSender;
#[cfg(any(feature = "shutdown", feature = "sleep", feature = "network"))]
use crate::common::SystemEvent;
#[cfg(feature = "shutdown")]
use crate::common::{ShutdownDecision, ShutdownHandle, ShutdownHandleInner};

// Custom messages sent back to the hidden window from other threads/callbacks.
#[cfg(feature = "shutdown")]
const WM_SENTINEL_ALLOW_SHUTDOWN: u32 = WM_APP + 1;
#[cfg(feature = "network")]
const WM_SENTINEL_NETWORK_CHANGE: u32 = WM_APP + 2;

// These power-event codes are not re-exported as named constants in windows-rs.
#[cfg(feature = "sleep")]
const PBT_APMSUSPEND: u32 = 4;
#[cfg(feature = "sleep")]
const PBT_APMRESUMESUSPEND: u32 = 7;

// Compile-time UTF-16 class name — no heap allocation.
const CLASS_NAME: PCWSTR = w!("SysSentinelHidden");

// ─── Shared state passed into the window procedure ───────────────────────────

struct WindowState {
    event_tx: EventSender,
    /// Set while we are blocking a pending shutdown query.
    #[cfg(feature = "shutdown")]
    pending_shutdown_hwnd: Option<HWND>,
    /// Signals the background watcher thread that `allow()` has been called,
    /// so it can invoke `ShutdownBlockReasonDestroy` and exit.
    #[cfg(feature = "shutdown")]
    shutdown_notify: Option<Arc<(Mutex<bool>, Condvar)>>,
    /// Last known network reachability; `None` means not yet delivered.
    /// Used to suppress duplicate NetworkUp/NetworkDown events.
    #[cfg(feature = "network")]
    last_network_up: Option<bool>,
}

// ─── Entry point ─────────────────────────────────────────────────────────────

/// Start the Windows sentinel on a background thread.
///
/// Returns immediately.  Events arrive on the [`EventSender`] channel.
pub fn start(event_tx: EventSender) {
    std::thread::Builder::new()
        .name("onebox_lifecycle_win32".into())
        .spawn(move || {
            // SAFETY: whole message loop is on this single thread.
            unsafe { run_message_loop(event_tx) };
        })
        .expect("onebox_lifecycle: failed to spawn Win32 message thread");
}

unsafe fn run_message_loop(event_tx: EventSender) {
    unsafe {
        // ── 1. Register a window class ──────────────────────────────────────
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            // No CS_HREDRAW/CS_VREDRAW — those only matter for visible windows.
            style: Default::default(),
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

        // ── 2. Create a message-only (hidden) window ────────────────────────
        // wndproc runs exclusively on this thread, so RefCell (not Mutex) is
        // the correct primitive: single-threaded interior mutability with no
        // locking overhead and no possibility of deadlocking the message pump.
        let state = Box::new(RefCell::new(WindowState {
            event_tx,
            #[cfg(feature = "shutdown")]
            pending_shutdown_hwnd: None,
            #[cfg(feature = "shutdown")]
            shutdown_notify: None,
            #[cfg(feature = "network")]
            last_network_up: None,
        }));
        let state_ptr = Box::into_raw(state); // freed in WM_DESTROY or on loop exit

        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW, // hidden from taskbar and Alt+Tab
            CLASS_NAME,
            w!("SysSentinel"),
            WS_POPUP, // no caption or border; zero-size, never shown
            0,
            0,
            0,
            0,
            None, // top-level window — receives WM_POWERBROADCAST & WM_QUERYENDSESSION
            None, // no menu
            None,
            Some(state_ptr as *const _),
        )
        .expect("onebox_lifecycle: CreateWindowExW failed");

        // ── 3. Prioritise shutdown notification ─────────────────────────────
        #[cfg(feature = "shutdown")]
        {
            // Level 0x3FF = 1023: our process is notified before most user apps.
            // Flags = 0: SHUTDOWN_NORETRY is NOT set (we want OS to retry after we unblock).
            let _ = SetProcessShutdownParameters(0x3FF, 0);
        }

        // ── 4. Register NCSI connectivity-hint change callback ──────────────
        #[cfg(feature = "network")]
        let net_notify_handle = {
            // NotifyNetworkConnectivityHintChange fires when Windows NCSI flips
            // between None / LocalAccess / InternetAccess, matching the taskbar
            // network icon.  initialnotification=true delivers the current state
            // immediately so last_network_up is set before the first real change.
            let mut handle: HANDLE = std::mem::zeroed();
            let _ = NotifyNetworkConnectivityHintChange(
                Some(net_change_callback),
                // Pass HWND.0 (*mut c_void) as the opaque context pointer.
                Some(hwnd.0 as *const _),
                true, // deliver current state immediately
                &mut handle,
            );
            handle
        };

        // ── 5. Pump messages ────────────────────────────────────────────────
        let mut msg = MSG::default();
        loop {
            let ret = GetMessageW(&mut msg, None, 0, 0);
            if ret.0 == 0 || ret.0 == -1 {
                break;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        // ── 6. Cleanup ──────────────────────────────────────────────────────
        // Reached only when WM_QUIT is posted.  WM_DESTROY (which also frees
        // state_ptr) is dispatched only when DestroyWindow is called — in the
        // current design that never happens, so we free the state here instead.
        // If a future caller posts WM_QUIT *after* DestroyWindow, the state
        // will already be null and Box::from_raw must not be called; guard that
        // case by re-reading GWLP_USERDATA which WM_DESTROY zeroes out.
        #[cfg(feature = "network")]
        {
            let _ = CancelMibChangeNotify2(net_notify_handle);
        }
        let live_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut RefCell<WindowState>;
        if !live_ptr.is_null() {
            drop(Box::from_raw(live_ptr));
        }
        let _ = UnregisterClassW(CLASS_NAME, None);
    }
}

// ─── Window procedure ─────────────────────────────────────────────────────────

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // ── Stash state pointer on first call ────────────────────────────────────
    if msg == WM_NCCREATE {
        let cs = unsafe { &*(lparam.0 as *const CREATESTRUCTW) };
        unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, cs.lpCreateParams as isize) };
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    }

    let state_ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut RefCell<WindowState>;

    // ── Teardown: free state and zero GWLP_USERDATA to prevent double-free ──
    if msg == WM_DESTROY {
        if !state_ptr.is_null() {
            // Zero the slot before dropping so the cleanup section in
            // run_message_loop can detect that WM_DESTROY already ran.
            unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0) };
            drop(unsafe { Box::from_raw(state_ptr) });
        }
        return LRESULT(0);
    }

    if state_ptr.is_null() {
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    }

    // SAFETY: state_ptr is valid for the lifetime of the window.  wndproc is
    // only ever called from the message-loop thread (the same thread that
    // created the Box), so RefCell's single-threaded invariant is upheld.
    let state_cell = unsafe { &*state_ptr };

    match msg {
        // ── Shutdown query ──────────────────────────────────────────────────
        #[cfg(feature = "shutdown")]
        WM_QUERYENDSESSION => {
            let (decision_tx, decision_rx) = std::sync::mpsc::sync_channel::<ShutdownDecision>(1);

            let handle = ShutdownHandle {
                inner: Some(ShutdownHandleInner::Mpsc(decision_tx)),
            };

            {
                let mut st = state_cell.borrow_mut();
                st.pending_shutdown_hwnd = Some(hwnd);
                st.event_tx.send(SystemEvent::ShuttingDown(handle));
            } // borrow released before recv_timeout blocks

            // Wait up to 2 s for the caller to decide.  Default to blocking
            // (safer than silently allowing shutdown mid-cleanup).
            let decision = decision_rx
                .recv_timeout(std::time::Duration::from_secs(2))
                .unwrap_or(ShutdownDecision::Block {
                    reason: Some("onebox_lifecycle: cleanup in progress".into()),
                });

            match decision {
                ShutdownDecision::Allow => {
                    state_cell.borrow_mut().pending_shutdown_hwnd = None;
                    LRESULT(1) // TRUE → allow
                }
                ShutdownDecision::Block { reason } => {
                    let reason_str = reason.unwrap_or_else(|| "Cleanup in progress…".into());
                    let wide: Vec<u16> = reason_str
                        .encode_utf16()
                        .chain(std::iter::once(0))
                        .collect();
                    // ShutdownBlockReasonCreate copies the string internally;
                    // `wide` can be dropped after this call.
                    let _ = unsafe { ShutdownBlockReasonCreate(hwnd, PCWSTR(wide.as_ptr())) };

                    // Set up a condvar so WM_SENTINEL_ALLOW_SHUTDOWN can wake
                    // the background thread without polling.
                    let notify = Arc::new((Mutex::new(false), Condvar::new()));
                    let notify_clone = Arc::clone(&notify);
                    state_cell.borrow_mut().shutdown_notify = Some(notify);

                    // hwnd.0 is *mut c_void; cast to usize for Send across threads.
                    let hwnd_usize = hwnd.0 as usize;
                    std::thread::spawn(move || {
                        let (lock, cvar) = &*notify_clone;
                        let guard = lock.lock().unwrap();
                        // Block until allow() is signalled or 5-minute safety timeout.
                        let _ = cvar.wait_timeout_while(
                            guard,
                            std::time::Duration::from_secs(300),
                            |&mut done| !done,
                        );
                        // Only this thread calls ShutdownBlockReasonDestroy.
                        unsafe {
                            let _ = ShutdownBlockReasonDestroy(HWND(hwnd_usize as *mut _));
                        }
                    });

                    LRESULT(0) // FALSE → block this round
                }
            }
        }

        // ── Async cleanup finished → signal the watcher thread ─────────────
        #[cfg(feature = "shutdown")]
        WM_SENTINEL_ALLOW_SHUTDOWN => {
            let mut st = state_cell.borrow_mut();
            st.pending_shutdown_hwnd = None;
            if let Some(notify) = st.shutdown_notify.take() {
                let (lock, cvar) = &*notify;
                *lock.lock().unwrap() = true;
                cvar.notify_one();
            }
            LRESULT(0)
        }

        // ── Power events ────────────────────────────────────────────────────
        #[cfg(feature = "sleep")]
        WM_POWERBROADCAST => {
            let event_code = wparam.0 as u32;
            // send takes &self; immutable borrow is sufficient.
            let st = state_cell.borrow();
            match event_code {
                PBT_APMSUSPEND => st.event_tx.send(SystemEvent::WillSleep),
                PBT_APMRESUMESUSPEND => st.event_tx.send(SystemEvent::DidWake),
                _ => {}
            }
            LRESULT(1)
        }

        // ── Network change (custom message posted by the OS callback) ───────
        #[cfg(feature = "network")]
        WM_SENTINEL_NETWORK_CHANGE => {
            let up = wparam.0 != 0;
            let mut st = state_cell.borrow_mut();
            if st.last_network_up != Some(up) {
                st.last_network_up = Some(up);
                if up {
                    st.event_tx.send(SystemEvent::NetworkUp);
                } else {
                    st.event_tx.send(SystemEvent::NetworkDown);
                }
            }
            LRESULT(0)
        }

        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

// ─── Network change callback (called by the OS on a system thread) ────────────

#[cfg(feature = "network")]
unsafe extern "system" fn net_change_callback(
    caller_context: *const core::ffi::c_void,
    hint: NL_NETWORK_CONNECTIVITY_HINT,
) {
    // InternetAccess (3)            – full internet
    // ConstrainedInternetAccess (4) – captive portal; some connectivity
    let up = hint.ConnectivityLevel == NetworkConnectivityLevelHintInternetAccess
        || hint.ConnectivityLevel == NetworkConnectivityLevelHintConstrainedInternetAccess;
    let hwnd = HWND(caller_context as *mut _);
    let _ = unsafe {
        PostMessageW(
            Some(hwnd),
            WM_SENTINEL_NETWORK_CHANGE,
            WPARAM(up as usize),
            LPARAM(0),
        )
    };
}

// ─── Public helper: post the "allow shutdown" message from any thread ─────────

/// Call this once your async cleanup is complete to let the OS proceed with shutdown.
///
/// This posts [`WM_SENTINEL_ALLOW_SHUTDOWN`] to the hidden sentinel window, which
/// wakes the background watcher thread and causes it to call
/// `ShutdownBlockReasonDestroy`.  The OS will then re-issue `WM_QUERYENDSESSION`
/// and the next [`ShutdownHandle::allow`] call will return `TRUE`.
///
/// Equivalent to `[NSApp replyToApplicationShouldTerminate: YES]` on macOS.
#[cfg(feature = "shutdown")]
#[allow(dead_code)]
pub fn post_allow_shutdown(hwnd: HWND) {
    unsafe {
        let _ = PostMessageW(Some(hwnd), WM_SENTINEL_ALLOW_SHUTDOWN, WPARAM(0), LPARAM(0));
    }
}
