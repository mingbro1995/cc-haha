import { isTauriRuntime, isBrowserH5Runtime } from '../lib/desktopRuntime'
import { getBaseUrl } from './client'

export type TerminalSpawnResult = {
  session_id: number
  shell: string
  cwd: string
}

export type TerminalOutputPayload = {
  session_id: number
  data: string
}

export type TerminalExitPayload = {
  session_id: number
  code: number
  signal?: string | null
}

type Unlisten = () => void

// ─── Tauri backend ─────────────────────────────────────────────────────────

async function invoke<T>(command: string, args?: Record<string, unknown>): Promise<T> {
  if (!isTauriRuntime()) {
    throw new Error('Terminal is available in the desktop app runtime.')
  }
  const api = await import('@tauri-apps/api/core')
  return api.invoke<T>(command, args)
}

// ─── H5 WebSocket backend ──────────────────────────────────────────────────

let h5Ws: WebSocket | null = null
let h5WsConnecting = false
let h5RequestId = 0
const h5Pending = new Map<string, { resolve: (v: unknown) => void; reject: (e: unknown) => void }>()
const h5OutputHandlers = new Set<(payload: TerminalOutputPayload) => void>()
const h5ExitHandlers = new Set<(payload: TerminalExitPayload) => void>()

function nextH5RequestId(): string {
  h5RequestId += 1
  return `h5-${Date.now()}-${h5RequestId}`
}

async function getTerminalWsUrl(): Promise<string> {
  const baseUrl = getBaseUrl()
  const response = await fetch(`${baseUrl}/api/terminal/ws-info`)
  if (!response.ok) {
    throw new Error('Terminal websocket server is not available')
  }
  const data = await response.json() as { url: string }
  return data.url
}

function ensureH5Ws(): Promise<WebSocket> {
  if (h5Ws?.readyState === WebSocket.OPEN) return Promise.resolve(h5Ws)
  if (h5WsConnecting) {
    // Wait for existing connection attempt
    return new Promise((resolve, reject) => {
      const check = () => {
        if (h5Ws?.readyState === WebSocket.OPEN) {
          resolve(h5Ws)
        } else if (!h5WsConnecting) {
          reject(new Error('WebSocket connection failed'))
        } else {
          setTimeout(check, 50)
        }
      }
      check()
    })
  }

  h5WsConnecting = true
  return new Promise((resolve, reject) => {
    getTerminalWsUrl()
      .then((url) => {
        const ws = new WebSocket(url)
        h5Ws = ws

        ws.onopen = () => {
          h5WsConnecting = false
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
                const resolver = h5Pending.get(msg.requestId!)
                if (resolver) {
                  h5Pending.delete(msg.requestId!)
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
                  h({ session_id: msg.sessionId!, code: msg.code!, signal: msg.signal }),
                )
                break
              }
              case 'bashPath': {
                const resolver = h5Pending.get(msg.requestId!)
                if (resolver) {
                  h5Pending.delete(msg.requestId!)
                  resolver.resolve(msg.path ?? null)
                }
                break
              }
              case 'ok': {
                const resolver = h5Pending.get(msg.requestId!)
                if (resolver) {
                  h5Pending.delete(msg.requestId!)
                  resolver.resolve(undefined)
                }
                break
              }
              case 'error': {
                const resolver = h5Pending.get(msg.requestId!)
                if (resolver) {
                  h5Pending.delete(msg.requestId!)
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
          h5Ws = null
          h5WsConnecting = false
          // Reject all pending requests
          for (const [, { reject }] of h5Pending) {
            reject(new Error('WebSocket connection closed'))
          }
          h5Pending.clear()
        }

        ws.onerror = (err) => {
          h5WsConnecting = false
          reject(err)
        }
      })
      .catch((err) => {
        h5WsConnecting = false
        reject(err)
      })
  })
}

function h5Send<T>(msg: Record<string, unknown>): Promise<T> {
  return new Promise((resolve, reject) => {
    const requestId = nextH5RequestId()
    h5Pending.set(requestId, { resolve: resolve as (v: unknown) => void, reject })

    ensureH5Ws()
      .then((ws) => {
        ws.send(JSON.stringify({ ...msg, requestId }))
      })
      .catch((err) => {
        h5Pending.delete(requestId)
        reject(err)
      })

    setTimeout(() => {
      if (h5Pending.has(requestId)) {
        h5Pending.delete(requestId)
        reject(new Error('Terminal operation timed out'))
      }
    }, 30000)
  })
}

// ─── Unified API ───────────────────────────────────────────────────────────

function isTerminalAvailable(): boolean {
  if (isTauriRuntime()) return true
  // In H5 browser, we assume the websocket server may be available.
  // The actual connection test happens on first use.
  return isBrowserH5Runtime()
}

export const terminalApi = {
  isAvailable: isTerminalAvailable,

  spawn(input: { cols: number; rows: number; cwd?: string }) {
    if (isTauriRuntime()) {
      return invoke<TerminalSpawnResult>('terminal_spawn', input)
    }
    return h5Send<TerminalSpawnResult>({ type: 'spawn', cols: input.cols, rows: input.rows, cwd: input.cwd })
  },

  write(sessionId: number, data: string) {
    if (isTauriRuntime()) {
      return invoke<void>('terminal_write', { sessionId, data })
    }
    return h5Send<void>({ type: 'write', sessionId, data })
  },

  resize(sessionId: number, cols: number, rows: number) {
    if (isTauriRuntime()) {
      return invoke<void>('terminal_resize', { sessionId, cols, rows })
    }
    return h5Send<void>({ type: 'resize', sessionId, cols, rows })
  },

  kill(sessionId: number) {
    if (isTauriRuntime()) {
      return invoke<void>('terminal_kill', { sessionId })
    }
    return h5Send<void>({ type: 'kill', sessionId })
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
      return invoke<string | null>('get_terminal_bash_path', undefined)
    }
    return h5Send<string | null>({ type: 'getBashPath' })
  },

  setBashPath(path: string | null) {
    if (isTauriRuntime()) {
      return invoke<void>('set_terminal_bash_path', { path })
    }
    return h5Send<void>({ type: 'setBashPath', path })
  },
}
