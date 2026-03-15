# onebox-lifecycle

跨平台系统生命周期监控库，用 Rust 编写。支持关机阻断、睡眠/唤醒、网络上下线事件。

## 功能矩阵

| 功能 | Windows | macOS |
|------|---------|-------|
| 关机阻断 | ✓ `WM_QUERYENDSESSION` + `ShutdownBlockReasonCreate` | ✓ `NSTerminateLater` |
| 睡眠 / 唤醒 | ✓ `WM_POWERBROADCAST` | ✓ `NSWorkspace` 通知 |
| 网络上下线 | ✓ `NotifyIpInterfaceChange` | ✓ `NWPathMonitor` |
| 异步清理 | ✓ handle-based，兼容 tokio | ✓ |

## 快速开始

```rust
use onebox_lifecycle::{Sentinel, SystemEvent};

fn main() {
    // macOS：必须在主线程调用
    let sentinel = Sentinel::start();

    while let Some(event) = sentinel.recv() {
        match event {
            SystemEvent::ShuttingDown(handle) => {
                // 做清理工作，完成后放行
                handle.allow();
            }
            SystemEvent::WillSleep    => println!("即将睡眠"),
            SystemEvent::DidWake      => println!("已唤醒"),
            SystemEvent::NetworkUp    => println!("网络已连接"),
            SystemEvent::NetworkDown  => println!("网络已断开"),
            _ => {}
        }
    }
}
```

## 异步清理（Tokio）

```rust
use onebox_lifecycle::{Sentinel, SystemEvent};

#[tokio::main]
async fn main() {
    let sentinel = Sentinel::start();

    while let Some(event) = sentinel.recv() {
        if let SystemEvent::ShuttingDown(handle) = event {
            tokio::spawn(async move {
                do_cleanup().await;
                handle.allow();
            });
        }
    }
}
```

## 与 Tauri 集成

Tauri 已托管 `NSApplication` 主线程。`Sentinel::start()` 必须在主线程调用，
但 `Sentinel` 本身因含 `MainThreadOnly` 的 macOS guard 无法跨线程移动。

使用 `into_receiver()` 拆分：平台 guard（NSApplicationDelegate、NSWorkspace
observer、NWPathMonitor）被 leak 并持续运行，`EventReceiver`（实现了 `Send`）
则移交后台线程处理事件。

**`src-tauri/Cargo.toml`**

```toml
[dependencies]
onebox_lifecycle = { path = "../onebox-lifecycle" }
# 发布后：
# onebox_lifecycle = "0.1"
```

**`src-tauri/src/lib.rs`**

```rust
use std::time::{SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Emitter};

#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct LifecycleEvent {
    kind: &'static str,
    message: String,
    timestamp_ms: u64,
}

fn emit(handle: &AppHandle, kind: &'static str, message: impl Into<String>) {
    let _ = handle.emit("lifecycle-event", LifecycleEvent {
        kind,
        message: message.into(),
        timestamp_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH).unwrap_or_default()
            .as_millis() as u64,
    });
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .setup(|app| {
            let handle = app.handle().clone();

            // setup 在主线程执行，满足 macOS 要求。
            // into_receiver() 泄漏 macOS guard（保持活跃），返回可跨线程的接收器。
            let rx = onebox_lifecycle::Sentinel::start().into_receiver();

            std::thread::Builder::new()
                .name("lifecycle-events".into())
                .spawn(move || {
                    while let Some(event) = rx.recv() {
                        use onebox_lifecycle::SystemEvent;
                        match event {
                            SystemEvent::WillSleep => {
                                emit(&handle, "WillSleep", "系统即将睡眠");
                            }
                            SystemEvent::DidWake => {
                                emit(&handle, "DidWake", "系统已从睡眠恢复");
                            }
                            SystemEvent::NetworkUp => {
                                emit(&handle, "NetworkUp", "网络已连接");
                            }
                            SystemEvent::NetworkDown => {
                                emit(&handle, "NetworkDown", "网络已断开");
                            }
                            SystemEvent::ShuttingDown(shutdown_handle) => {
                                emit(&handle, "ShuttingDown", "收到关机请求，正在清理…");
                                let h = handle.clone();
                                std::thread::spawn(move || {
                                    // 在此执行实际清理工作…
                                    std::thread::sleep(std::time::Duration::from_secs(3));
                                    emit(&h, "ShutdownComplete", "清理完成，允许系统关机");
                                    shutdown_handle.allow();
                                });
                            }
                            _ => {}
                        }
                    }
                })
                .expect("无法启动 lifecycle 线程");

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("tauri 运行失败");
}
```

**前端（JavaScript）**

```js
const { listen } = window.__TAURI__.event;

await listen('lifecycle-event', ({ payload }) => {
    const { kind, message, timestampMs } = payload;
    console.log(`[${new Date(timestampMs).toLocaleTimeString()}] ${kind}: ${message}`);
    // 在此更新 UI
});
```

> **为什么关机阻断需要 Tauri 而非 CLI？**
> 纯 CLI 进程没有 `.app` bundle，macOS 不会向其发送 `applicationShouldTerminate:`，
> 关机时直接 `SIGKILL`，无法阻断或写日志。Tauri 打包为真正的 `.app`，
> macOS 会显示"等待应用程序…"提示并等待 `replyToApplicationShouldTerminate:` 后才继续。

## 运行演示

```bash
# 构建并运行（日志写入 ./onebox_lifecycle_demo.log）
make run

# 脱离 Terminal 运行（用于测试关机阻断）
make run-detached
make log      # 另开终端查看日志
make stop     # 结束进程

# 注册为 launchd 用户代理（推荐测试关机）
make install-agent
make uninstall-agent
```

## 为什么需要脱离 Terminal 运行

macOS 关机时先询问 Terminal 是否退出。若 Terminal 杀死子进程，
`replyToApplicationShouldTerminate:` 永远不会被调用，系统将永久挂起。
使用 `make run-detached` 或 `make install-agent` 可绕过此问题。

## 模块结构

```
src/
├── lib.rs           公共 API：Sentinel、SystemEvent
├── common/mod.rs    共享类型：ShutdownHandle、EventReceiver/Sender
├── macos/mod.rs     macOS 后端：NSApplicationDelegate、NSWorkspace、NWPathMonitor
└── windows/mod.rs   Windows 后端：隐藏 Win32 窗口消息循环

examples/
└── demo_full.rs     完整演示（含文件日志、异步清理）
```
