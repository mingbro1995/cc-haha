# H5 网页端终端支持改造提示词（Tauri v0.3.2 版本）

## 项目背景

本项目（cc-haha v0.3.2）是基于 Tauri + React 的桌面端应用，同时支持通过 H5 网页访问。当前终端功能是 Tauri 桌面端专属能力：
- **Rust 后端**使用 `portable_pty` crate 创建本地 PTY 进程
- **前端**通过 `@tauri-apps/api/core` 的 `invoke` 调用 Tauri 命令，`@tauri-apps/api/event` 的 `listen` 接收事件
- **H5 网页端**由于不在 Tauri 环境中（`isTauriRuntime()` 返回 false），`terminalApi.isAvailable()` 返回 false，终端不可用

**改造目标**：Tauri 桌面端终端逻辑完全不动，让 H5 网页端也能使用终端功能。

---

## 当前终端架构

### 1. Rust 后端（Tauri）

文件：`desktop/src-tauri/src/lib.rs`

终端状态管理：
```rust
#[derive(Default)]
struct TerminalState {
    next_id: AtomicU32,
    sessions: Mutex<HashMap<u32, TerminalSession>>,
}

struct TerminalSession {
    master: Box<dyn MasterPty + Send>,
    writer: Mutex<Box<dyn std::io::Write + Send>>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
}
```

Tauri 命令（约第 915-1135 行）：
```rust
#[tauri::command]
fn terminal_spawn(app: AppHandle, state: State<'_, TerminalState>, cols: u16, rows: u16, cwd: Option<String>)
    -> Result<TerminalSpawnResult, String>

#[tauri::command]
fn terminal_write(state: State<'_, TerminalState>, session_id: u32, data: String)
    -> Result<(), String>

#[tauri::command]
fn terminal_resize(state: State<'_, TerminalState>, session_id: u32, cols: u16, rows: u16)
    -> Result<(), String>

#[tauri::command]
fn terminal_kill(state: State<'_, TerminalState>, session_id: u32)
    -> Result<(), String>

#[tauri::command]
fn get_terminal_bash_path(app: AppHandle) -> Option<String>

#[tauri::command]
fn set_terminal_bash_path(app: AppHandle, path: Option<String>) -> Result<(), String>
```

事件推送（通过 `app.emit`）：
- `terminal-output`： `{ session_id: u32, data: String }`
- `terminal-exit`： `{ session_id: u32, code: u32, signal: Option<String> }`

### 2. 前端终端 API

文件：`desktop/src/api/terminal.ts`

```typescript
import { isTauriRuntime } from '../lib/desktopRuntime'

async function invoke<T>(command: string, args?: Record<string, unknown>): Promise<T> {
  if (!isTauriRuntime()) {
    throw new Error('Terminal is available in the desktop app runtime.')
  }
  const api = await import('@tauri-apps/api/core')
  return api.invoke<T>(command, args)
}

export const terminalApi = {
  isAvailable: isTauriRuntime,

  spawn(input: { cols: number; rows: number; cwd?: string }) {
    return invoke<TerminalSpawnResult>('terminal_spawn', input)
  },

  write(sessionId: number, data: string) {
    return invoke<void>('terminal_write', { sessionId, data })
  },

  resize(sessionId: number, cols: number, rows: number) {
    return invoke<void>('terminal_resize', { sessionId, cols, rows })
  },

  kill(sessionId: number) {
    return invoke<void>('terminal_kill', { sessionId })
  },

  async onOutput(handler: (payload: TerminalOutputPayload) => void): Promise<Unlisten> {
    const events = await import('@tauri-apps/api/event')
    return events.listen<TerminalOutputPayload>('terminal-output', (event) => handler(event.payload))
  },

  async onExit(handler: (payload: TerminalExitPayload) => void): Promise<Unlisten> {
    const events = await import('@tauri-apps/api/event')
    return events.listen<TerminalExitPayload>('terminal-exit', (event) => handler(event.payload))
  },

  getBashPath() {
    return invoke<string | null>('get_terminal_bash_path', undefined)
  },

  setBashPath(path: string | null) {
    return invoke<void>('set_terminal_bash_path', { path })
  },
}
```

### 3. 运行时检测

文件：`desktop/src/lib/desktopRuntime.ts`

```typescript
export function isTauriRuntime() {
  if (typeof window === 'undefined') return false
  return '__TAURI_INTERNALS__' in window || '__TAURI__' in window
}

export function isBrowserH5Runtime() {
  return typeof window !== 'undefined' && !isTauriRuntime()
}
```

### 4. 前端终端组件

文件：`desktop/src/pages/TerminalSettings.tsx`

通过 `terminalApi.isAvailable()` 判断是否可用。当不可用时显示 "Desktop runtime required"。

### 5. H5 对终端的限制

文件：`desktop/src/pages/ActiveSession.tsx`（约第 260、288 行）：

```typescript
const isMobileLayout = useMobileViewport() && !isTauriRuntime()

const showTerminalPanel = useTerminalPanelStore((state) =>
  activeTabId && isSessionTabState(activeTabId, activeTabType) && !isMemberSession && !isMobileLayout
    ? state.isPanelOpen(activeTabId)
    : false,
)
```

`isMobileLayout` 在 H5 浏览器中始终为 true（因为 `!isTauriRuntime()` 为 true），所以终端面板不显示。

### 6. Sidecar 服务器

文件：`src/server/index.ts`

Tauri Rust 启动了一个 Bun.serve HTTP + WebSocket 服务器（`claude-sidecar`）。H5 前端通过该服务器访问 API。

### 7. 项目依赖

文件：`desktop/src-tauri/Cargo.toml`

```toml
[dependencies]
tauri = { version = "=2.10.3", features = ["tray-icon", "unstable"] }
portable-pty = "0.9.0"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
```

---

## 改造方案

### 核心思路

Tauri Rust 后端直接启动一个 **WebSocket 终端代理服务器**，让 H5 前端通过 WebSocket 远程操作桌面端上的 `portable_pty` 终端进程。Tauri 前端保持现有 `invoke`/`listen` 机制不变。

```
H5 浏览器 ←──WebSocket──→ Tauri Rust 后端 ←──portable_pty──→ 本地 Shell
    ↑                              ↑
    │                              │
 xterm.js                    TerminalState
(browserHost)                 (保持不变)
```

### 通信路径

H5 前端不知道 Tauri WebSocket 的端口，需要通过 sidecar 服务器发现：

```
H5 前端 ──HTTP──→ Sidecar 服务器 ──读文件──→ ~/.claude/terminal-ws.json
                                              ↑
H5 前端 ←──WebSocket──→ Tauri WS 服务器 ──────┘
```

### 具体改造点

#### 1. Rust 后端：新增 WebSocket 终端服务器

**新增依赖**：`desktop/src-tauri/Cargo.toml`

```toml
[dependencies]
# 已有依赖保持不变，新增：
tokio-tungstenite = "0.26"
tokio = { version = "1", features = ["rt", "net", "sync"] }
futures = "0.3"
```

**新增文件**：`desktop/src-tauri/src/terminal_websocket.rs`

功能要求：
- 启动一个 WebSocket 服务器，绑定到 `0.0.0.0:0`（动态端口，局域网可访问）
- 启动后将 `{ "port": <端口> }` 写入 `~/.claude/terminal-ws.json`
- 复用已有的 `TerminalState`（通过 `AppHandle.state::<TerminalState>()`）
- 支持 H5 Token 认证（从 WebSocket URL 的 query param 读取 `token`，调用验证逻辑）

WebSocket 消息协议（JSON 格式）：

**客户端 → 服务端（请求）**：
```typescript
type TerminalClientMessage =
  | { type: 'spawn'; requestId: string; cols: number; rows: number; cwd?: string }
  | { type: 'write'; requestId: string; sessionId: number; data: string }
  | { type: 'resize'; requestId: string; sessionId: number; cols: number; rows: number }
  | { type: 'kill'; requestId: string; sessionId: number }
  | { type: 'getBashPath'; requestId: string }
  | { type: 'setBashPath'; requestId: string; path: string | null }
```

**服务端 → 客户端（响应/推送）**：
```typescript
type TerminalServerMessage =
  | { type: 'spawnResult'; requestId: string; sessionId: number; shell: string; cwd: string }
  | { type: 'output'; sessionId: number; data: string }
  | { type: 'exit'; sessionId: number; code: number; signal?: string | null }
  | { type: 'bashPath'; requestId: string; path: string | null }
  | { type: 'ok'; requestId: string }
  | { type: 'error'; requestId: string; message: string }
```

实现要点：
- **spawn**：调用 `terminal_spawn` 的逻辑（复用 `resolve_terminal_cwd`、`resolved_terminal_shell`、`native_pty_system().openpty()` 等），创建 PTY 会话
- **write**：获取 `TerminalSession` 的 writer，写入数据
- **resize**：调用 `session.master.resize()`
- **kill**：调用 `session.killer.kill()`，从 sessions HashMap 中移除
- **输出推送**：spawn 后启动一个线程读取 PTY master 的 reader，通过 WebSocket 发送 `output` 消息
- **退出推送**：spawn 后启动一个线程等待 child 退出，发送 `exit` 消息，清理 session
- **会话隔离**：每个 WebSocket 连接维护独立的 session 映射（客户端 session id → 服务端 session id），连接断开时自动 `kill` 该连接的所有 session
- **Token 认证**：读取 URL query param `?token=xxx`，验证逻辑复用 H5 Token 验证（可以简化：先与 sidecar 的验证逻辑保持一致，或先从 `H5AccessService` 获取验证方式）

**关键：复用已有终端逻辑**

不要复制 `terminal_spawn`/`terminal_write` 等命令的实现。而是把它们的核心逻辑提取为可复用的函数，Tauri 命令和 WebSocket 处理器都调用这些函数。

在 `lib.rs` 中，把 `terminal_spawn` 等命令的实现提取为内部函数：

```rust
// 已有的 Tauri 命令保持不变，但内部调用提取的函数

fn do_terminal_spawn(
    app: &AppHandle,
    state: &TerminalState,
    cols: u16,
    rows: u16,
    cwd: Option<String>,
    output_callback: impl Fn(TerminalOutputPayload) + Send + 'static,
    exit_callback: impl Fn(TerminalExitPayload) + Send + 'static,
) -> Result<TerminalSpawnResult, String> {
    // 原有 terminal_spawn 的实现逻辑
    // ...
}

fn do_terminal_write(state: &TerminalState, session_id: u32, data: String) -> Result<(), String> { ... }
fn do_terminal_resize(state: &TerminalState, session_id: u32, cols: u16, rows: u16) -> Result<(), String> { ... }
fn do_terminal_kill(state: &TerminalState, session_id: u32) -> Result<(), String> { ... }
```

**修改文件**：`desktop/src-tauri/src/lib.rs`

在 `run()` 函数的 `setup` 中启动 WebSocket 服务器：

```rust
.setup(|app| {
    setup_system_tray(app)?;
    macos_notifications::install_click_handler(app.handle().clone());
    restore_main_window_state(&app.handle());

    // === 新增：启动终端 WebSocket 服务器 ===
    let app_handle = app.handle().clone();
    tauri::async_runtime::spawn(async move {
        if let Err(e) = start_terminal_websocket_server(app_handle).await {
            eprintln!("[desktop] failed to start terminal websocket server: {e}");
        }
    });
    // ======================================

    // ... 原有 server sidecar 启动逻辑保持不变 ...
})
```

在 `lib.rs` 的 `mod` 声明中新增：
```rust
mod terminal_websocket;
```

#### 2. Sidecar 服务器：新增终端 WS 发现 API

**文件**：`src/server/api/terminal.ts`（新增）

```typescript
import fs from 'node:fs'
import path from 'node:path'
import os from 'node:os'

function getTerminalWsPort(): number | null {
  const configDir = process.env.CLAUDE_CONFIG_DIR || path.join(os.homedir(), '.claude')
  const portFile = path.join(configDir, 'terminal-ws.json')
  try {
    const data = JSON.parse(fs.readFileSync(portFile, 'utf8'))
    return typeof data.port === 'number' ? data.port : null
  } catch {
    return null
  }
}

export async function handleTerminalApi(req: Request, url: URL): Promise<Response> {
  const sub = url.pathname.split('/').filter(Boolean)[2]

  if (sub === 'ws-info' && req.method === 'GET') {
    const port = getTerminalWsPort()
    if (!port) {
      return Response.json(
        { error: 'Terminal websocket server is not available' },
        { status: 503 }
      )
    }

    // 构建 WebSocket URL
    // 如果请求通过反向代理，使用请求的主机名；否则使用 127.0.0.1
    const host = req.headers.get('X-Forwarded-Host')
      || req.headers.get('Host')
      || `127.0.0.1:${port}`
    const wsHost = host.includes(':') ? host.split(':')[0] : host
    const wsUrl = `ws://${wsHost}:${port}/ws/terminal`

    return Response.json({ url: wsUrl, port })
  }

  return Response.json(
    { error: 'Not Found', message: `Unknown terminal endpoint: ${sub}` },
    { status: 404 }
  )
}
```

**文件**：`src/server/router.ts`

在 `handleApiRequest` 函数的 switch 中新增：

```typescript
import { handleTerminalApi } from './api/terminal.js'

// 在 switch (resource) 中新增：
case 'terminal':
  return handleTerminalApi(req, url)
```

#### 3. 前端：改造 `terminalApi` 支持 H5 WebSocket

**文件**：`desktop/src/api/terminal.ts`

将 `terminalApi` 改造为在 Tauri 环境使用 `invoke`，在 H5 环境使用 WebSocket：

```typescript
import { isTauriRuntime, isBrowserH5Runtime } from '../lib/desktopRuntime'
import { getBaseUrl } from './client'

// ... 类型定义保持不变 ...

// ========== Tauri 端实现（保持不变）==========
async function tauriInvoke<T>(command: string, args?: Record<string, unknown>): Promise<T> {
  if (!isTauriRuntime()) {
    throw new Error('Terminal is available in the desktop app runtime.')
  }
  const api = await import('@tauri-apps/api/core')
  return api.invoke<T>(command, args)
}

// ========== H5 WebSocket 端实现（新增）==========
let h5TerminalWs: WebSocket | null = null
let h5TerminalWsReady = false
let h5RequestIdCounter = 0
const h5PendingResolvers = new Map<string, { resolve: (value: unknown) => void; reject: (reason: unknown) => void }>()
const h5OutputHandlers = new Set<(payload: TerminalOutputPayload) => void>()
const h5ExitHandlers = new Set<(payload: TerminalExitPayload) => void>()

function generateH5RequestId(): string {
  h5RequestIdCounter += 1
  return `h5-${Date.now()}-${h5RequestIdCounter}`
}

async function getTerminalWebSocketUrl(): Promise<string> {
  const baseUrl = getBaseUrl()
  const response = await fetch(`${baseUrl}/api/terminal/ws-info`)
  if (!response.ok) {
    throw new Error('Terminal websocket server is not available')
  }
  const data = await response.json() as { url: string }
  return data.url
}

function ensureH5TerminalWs(): Promise<WebSocket> {
  if (h5TerminalWs?.readyState === WebSocket.OPEN) return Promise.resolve(h5TerminalWs)

  return new Promise((resolve, reject) => {
    getTerminalWebSocketUrl().then((url) => {
      const ws = new WebSocket(url)
      h5TerminalWs = ws

      ws.onopen = () => {
        h5TerminalWsReady = true
        resolve(ws)
      }

      ws.onmessage = (event) => {
        try {
          const msg = JSON.parse(event.data as string) as {
            type: string
            requestId?: string
            sessionId?: number
            data?: string
            code?: number
            signal?: string | null
            shell?: string
            cwd?: string
            path?: string | null
            message?: string
          }

          switch (msg.type) {
            case 'spawnResult': {
              const resolver = h5PendingResolvers.get(msg.requestId!)
              if (resolver) {
                h5PendingResolvers.delete(msg.requestId!)
                resolver.resolve({
                  session_id: msg.sessionId!,
                  shell: msg.shell!,
                  cwd: msg.cwd!,
                })
              }
              break
            }
            case 'output': {
              h5OutputHandlers.forEach((h) => h({ session_id: msg.sessionId!, data: msg.data! }))
              break
            }
            case 'exit': {
              h5ExitHandlers.forEach((h) =>
                h({ session_id: msg.sessionId!, code: msg.code!, signal: msg.signal })
              )
              break
            }
            case 'bashPath': {
              const resolver = h5PendingResolvers.get(msg.requestId!)
              if (resolver) {
                h5PendingResolvers.delete(msg.requestId!)
                resolver.resolve(msg.path)
              }
              break
            }
            case 'ok': {
              const resolver = h5PendingResolvers.get(msg.requestId!)
              if (resolver) {
                h5PendingResolvers.delete(msg.requestId!)
                resolver.resolve(undefined)
              }
              break
            }
            case 'error': {
              const resolver = h5PendingResolvers.get(msg.requestId!)
              if (resolver) {
                h5PendingResolvers.delete(msg.requestId!)
                resolver.reject(new Error(msg.message!))
              }
              break
            }
          }
        } catch {
          // ignore malformed messages
        }
      }

      ws.onclose = () => {
        h5TerminalWs = null
        h5TerminalWsReady = false
      }

      ws.onerror = (err) => {
        reject(err)
      }
    }).catch(reject)
  })
}

function h5SendAndWait<T>(msg: Record<string, unknown>): Promise<T> {
  return new Promise((resolve, reject) => {
    const requestId = generateH5RequestId()
    h5PendingResolvers.set(requestId, { resolve: resolve as (value: unknown) => void, reject })

    ensureH5TerminalWs().then((ws) => {
      ws.send(JSON.stringify({ ...msg, requestId }))
    }).catch((err) => {
      h5PendingResolvers.delete(requestId)
      reject(err)
    })

    // 超时处理
    setTimeout(() => {
      if (h5PendingResolvers.has(requestId)) {
        h5PendingResolvers.delete(requestId)
        reject(new Error('Terminal operation timed out'))
      }
    }, 30000)
  })
}

// ========== 统一的 terminalApi ==========

export const terminalApi = {
  isAvailable: () => {
    // Tauri 环境始终可用；H5 环境尝试检测 sidecar 是否暴露了终端 WS
    if (isTauriRuntime()) return true
    if (!isBrowserH5Runtime()) return false
    // H5 环境中，如果已经连过 WS 或者可以尝试连接，返回 true
    // 简化：始终返回 true，让 TerminalSettings 组件自己处理连接错误
    return true
  },

  spawn(input: { cols: number; rows: number; cwd?: string }) {
    if (isTauriRuntime()) {
      return tauriInvoke<TerminalSpawnResult>('terminal_spawn', input)
    }
    return h5SendAndWait<TerminalSpawnResult>({ type: 'spawn', cols: input.cols, rows: input.rows, cwd: input.cwd })
  },

  write(sessionId: number, data: string) {
    if (isTauriRuntime()) {
      return tauriInvoke<void>('terminal_write', { sessionId, data })
    }
    return h5SendAndWait<void>({ type: 'write', sessionId, data })
  },

  resize(sessionId: number, cols: number, rows: number) {
    if (isTauriRuntime()) {
      return tauriInvoke<void>('terminal_resize', { sessionId, cols, rows })
    }
    return h5SendAndWait<void>({ type: 'resize', sessionId, cols, rows })
  },

  kill(sessionId: number) {
    if (isTauriRuntime()) {
      return tauriInvoke<void>('terminal_kill', { sessionId })
    }
    return h5SendAndWait<void>({ type: 'kill', sessionId })
  },

  async onOutput(handler: (payload: TerminalOutputPayload) => void): Promise<Unlisten> {
    if (isTauriRuntime()) {
      const events = await import('@tauri-apps/api/event')
      return events.listen<TerminalOutputPayload>('terminal-output', (event) => handler(event.payload))
    }
    h5OutputHandlers.add(handler)
    return () => { h5OutputHandlers.delete(handler) }
  },

  async onExit(handler: (payload: TerminalExitPayload) => void): Promise<Unlisten> {
    if (isTauriRuntime()) {
      const events = await import('@tauri-apps/api/event')
      return events.listen<TerminalExitPayload>('terminal-exit', (event) => handler(event.payload))
    }
    h5ExitHandlers.add(handler)
    return () => { h5ExitHandlers.delete(handler) }
  },

  getBashPath() {
    if (isTauriRuntime()) {
      return tauriInvoke<string | null>('get_terminal_bash_path', undefined)
    }
    return h5SendAndWait<string | null>({ type: 'getBashPath' })
  },

  setBashPath(path: string | null) {
    if (isTauriRuntime()) {
      return tauriInvoke<void>('set_terminal_bash_path', { path })
    }
    return h5SendAndWait<void>({ type: 'setBashPath', path })
  },
}
```

#### 4. 前端：移除 H5 终端限制

**文件**：`desktop/src/pages/ActiveSession.tsx`

修改 `showTerminalPanel` 的计算逻辑，移除 `!isMobileLayout` 限制：

```typescript
// 修改前：
const showTerminalPanel = useTerminalPanelStore((state) =>
  activeTabId && isSessionTabState(activeTabId, activeTabType) && !isMemberSession && !isMobileLayout
    ? state.isPanelOpen(activeTabId)
    : false,
)

// 修改后：
const showTerminalPanel = useTerminalPanelStore((state) =>
  activeTabId && isSessionTabState(activeTabId, activeTabType) && !isMemberSession
    ? state.isPanelOpen(activeTabId)
    : false,
)
```

**注意**：如果希望在手机上隐藏终端面板但只在 H5 桌面浏览器中显示，可以更精确地控制：

```typescript
const showTerminalPanel = useTerminalPanelStore((state) =>
  activeTabId && isSessionTabState(activeTabId, activeTabType) && !isMemberSession
    ? state.isPanelOpen(activeTabId)
    : false,
)
```

如果确实需要在手机上也隐藏（因为手机屏幕太小），可以保持 `!isMobileLayout` 但把条件改为只在真正的手机viewport下隐藏，而不是在H5环境下全部隐藏。考虑到用户明确说想在H5网页使用终端，直接移除限制更合理。

#### 5. 前端：适配 H5 终端的 `TerminalSettings.tsx`

文件：`desktop/src/pages/TerminalSettings.tsx`

当前组件中 `isTauri` 的检查可能来自 `terminalApi.isAvailable()` 或 `isTauriRuntime()`。需要确保：

1. 当 `terminalApi.isAvailable()` 在 H5 中返回 true 时，组件尝试启动终端
2. 如果 WebSocket 连接失败，显示适当的错误状态

检查 `TerminalSettings.tsx` 中对 `isTauri` 的使用：
- `BashPathSettings` 组件中 `if (!isTauri) return null` —— Windows bash path 设置只在桌面端有意义，保持此限制合理
- `handleBrowse` 中 `if (!isTauri) return` —— 文件选择对话框只在桌面端可用，保持合理
- 主组件中 `terminalApi.isAvailable()` 决定终端是否可用 —— 修改后 H5 返回 true，会尝试启动

需要确保终端面板的尺寸在 H5 环境下也能正常显示。可能需要调整移动端的高度适配。

#### 6. 测试更新

**文件**：`desktop/src/pages/TerminalSettings.test.tsx`

测试用例 `'shows a desktop-runtime empty state outside Tauri'` 需要更新期望，因为 H5 现在也应该支持终端。

**文件**：`desktop/src/api/terminal.test.ts`

更新 `terminalApi.isAvailable()` 的测试期望。

---

## WebSocket 消息时序示例

### Spawn 流程

```
H5 Client                                  Tauri WS Server
   |                                              |
   |-- { type: "spawn", cols: 80, rows: 24 } --->|
   |                                              |-- 创建 PTY (portable_pty)
   |                                              |-- 启动 shell 进程
   |<-- { type: "spawnResult", sessionId: 1 } ---|
   |                                              |-- [后台线程读取 PTY 输出]
   |<-- { type: "output", sessionId: 1,           |
   |          data: "user@host:~$ " } ------------|
```

### Write + Output 流程

```
H5 Client                                  Tauri WS Server
   |                                              |
   |-- { type: "write", sessionId: 1,             |
   |      data: "ls\n" } ------------------------>|
   |                                              |-- 写入 PTY
   |<-- { type: "output", sessionId: 1,           |
   |          data: "file1.txt\nfile2.txt\n" } ---|
```

### Exit 流程

```
H5 Client                                  Tauri WS Server
   |                                              |
   |-- { type: "kill", sessionId: 1 } ----------->|
   |                                              |-- 杀死 shell 进程
   |<-- { type: "exit", sessionId: 1,             |
   |      code: 0, signal: null } ---------------|
```

---

## 安全考虑

1. **Token 认证**：WebSocket 连接建立时从 URL query param 读取 `token`，验证 H5 Token
2. **来源限制**：WebSocket 服务器绑定到 `0.0.0.0`，但 Token 验证确保只有授权用户能访问
3. **会话隔离**：不同 WebSocket 连接的终端会话互不干扰，连接断开时自动清理 PTY 进程
4. **局域网暴露**：绑定 `0.0.0.0` 使局域网内 H5 客户端可访问，这是符合 H5 使用场景的
5. **端口文件安全**：`~/.claude/terminal-ws.json` 只包含端口号，不包含敏感信息

---

## 依赖变更

### Rust 依赖（`desktop/src-tauri/Cargo.toml`）

新增：
```toml
[dependencies]
# 已有依赖保持不变
# 新增：
tokio-tungstenite = "0.26"
tokio = { version = "1", features = ["rt", "net", "sync"] }
futures = "0.3"
```

### 前端依赖

无新增依赖，使用浏览器原生 `WebSocket` API。

---

## 实现步骤总结

1. **Rust 后端**：
   - [ ] 在 `Cargo.toml` 中添加 `tokio-tungstenite`、`tokio`、`futures` 依赖
   - [ ] 创建 `desktop/src-tauri/src/terminal_websocket.rs`
   - [ ] 在 `lib.rs` 中把终端命令的实现提取为可复用的内部函数
   - [ ] 在 `lib.rs` 的 `run()` setup 中启动 WebSocket 服务器
   - [ ] 在 `lib.rs` 中添加 `mod terminal_websocket;`

2. **Sidecar 服务器**：
   - [ ] 创建 `src/server/api/terminal.ts`
   - [ ] 在 `src/server/router.ts` 中新增 `case 'terminal'` 路由

3. **前端 API**：
   - [ ] 修改 `desktop/src/api/terminal.ts`，实现 H5 WebSocket 分支

4. **前端组件**：
   - [ ] 修改 `desktop/src/pages/ActiveSession.tsx`，移除 `!isMobileLayout` 终端限制
   - [ ] 检查 `TerminalSettings.tsx` 中的 H5 适配

5. **测试**：
   - [ ] 更新 `TerminalSettings.test.tsx`
   - [ ] 更新 `terminal.test.ts`

6. **验证**：
   - [ ] 构建 Tauri 桌面端
   - [ ] 在浏览器中打开 H5 页面
   - [ ] 进入会话，打开终端面板
   - [ ] 验证终端可以 spawn、write、接收 output
   - [ ] 验证桌面端终端不受影响

---

## 注意事项

1. **Tauri 桌面端终端完全不变**：`terminal_spawn` 等 Tauri 命令保持原有实现，只是内部逻辑提取为可复用函数
2. **WebSocket 连接失败处理**：H5 前端在 WebSocket 连接失败时应显示友好的错误提示
3. **重连逻辑**：考虑 WebSocket 断线重连（页面切换、网络波动等）
4. **并发处理**：同一个 H5 客户端可能同时打开多个终端标签，确保 session 隔离
5. **资源清理**：WebSocket 断开时必须 kill 所有该连接的 PTY 会话
6. **移动端适配**：手机屏幕上的终端需要合适的默认尺寸（如 cols: 40, rows: 12）
