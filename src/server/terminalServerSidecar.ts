import fs from 'node:fs'
import path from 'node:path'
import { spawn } from 'node:child_process'
import os from 'node:os'

const TERMINAL_WS_PORT_FILE = 'terminal-ws.json'
const PORT_FILE_POLL_MS = 100
const PORT_FILE_TIMEOUT_MS = 60_000

function claudeConfigDir(): string {
  return process.env.CLAUDE_CONFIG_DIR || path.join(os.homedir(), '.claude')
}

function terminalPortFilePath(): string {
  return path.join(claudeConfigDir(), TERMINAL_WS_PORT_FILE)
}

function findTerminalServerExecutable(): string | null {
  const isWindows = process.platform === 'win32'
  const ext = isWindows ? '.exe' : ''
  const candidates = [
    path.resolve(process.cwd(), 'desktop', 'src-tauri', 'target', 'release', `terminal-server${ext}`),
    path.resolve(process.cwd(), 'desktop', 'src-tauri', 'target', 'debug', `terminal-server${ext}`),
    path.resolve(process.cwd(), 'src-tauri', 'target', 'release', `terminal-server${ext}`),
    path.resolve(process.cwd(), 'src-tauri', 'target', 'debug', `terminal-server${ext}`),
  ]
  for (const candidate of candidates) {
    try {
      fs.accessSync(candidate, fs.constants.X_OK)
      return candidate
    } catch {
      // try next
    }
  }
  return null
}

function readTerminalWsPort(): number | null {
  try {
    const data = JSON.parse(fs.readFileSync(terminalPortFilePath(), 'utf8')) as { port?: number }
    return typeof data.port === 'number' ? data.port : null
  } catch {
    return null
  }
}

async function waitForTerminalWsPort(): Promise<number> {
  const deadline = Date.now() + PORT_FILE_TIMEOUT_MS
  return new Promise((resolve, reject) => {
    const check = () => {
      const port = readTerminalWsPort()
      if (port) {
        resolve(port)
        return
      }
      if (Date.now() > deadline) {
        reject(new Error('timed out waiting for terminal websocket server port file'))
        return
      }
      setTimeout(check, PORT_FILE_POLL_MS)
    }
    check()
  })
}

export async function startTerminalServerSidecar(): Promise<() => void> {
  // If a port file already exists and points to a live server, reuse it.
  const existingPort = readTerminalWsPort()
  if (existingPort) {
    try {
      const socket = await import('node:net')
      await new Promise<void>((resolve, reject) => {
        const conn = socket.createConnection(existingPort, '127.0.0.1')
        conn.on('connect', () => {
          conn.destroy()
          resolve()
        })
        conn.on('error', (err) => {
          reject(err)
        })
      })
      console.log(`[terminal-sidecar] reusing existing terminal websocket server on port ${existingPort}`)
      return () => {}
    } catch {
      // stale port file; fall through and spawn a new server
    }
  }

  const executable = findTerminalServerExecutable()
  let child: ReturnType<typeof spawn> | null = null

  if (executable) {
    console.log(`[terminal-sidecar] spawning ${executable}`)
    child = spawn(executable, [], {
      detached: false,
      stdio: 'inherit',
      env: { ...process.env, CLAUDE_CONFIG_DIR: claudeConfigDir() },
    })
  } else {
    const cargoDir = path.resolve(process.cwd(), 'desktop', 'src-tauri')
    if (!fs.existsSync(path.join(cargoDir, 'Cargo.toml'))) {
      console.warn('[terminal-sidecar] terminal-server executable not found and cargo workspace not detected; terminal support disabled')
      return () => {}
    }
    console.log('[terminal-sidecar] building and spawning terminal-server via cargo')
    child = spawn('cargo', ['run', '--bin', 'terminal-server', '--quiet'], {
      cwd: cargoDir,
      detached: false,
      stdio: 'inherit',
      env: { ...process.env, CLAUDE_CONFIG_DIR: claudeConfigDir() },
    })
  }

  if (!child) {
    return () => {}
  }

  const cleanup = () => {
    if (child && !child.killed) {
      child.kill()
    }
  }

  child.on('error', (err) => {
    console.error('[terminal-sidecar] failed to spawn terminal server:', err.message)
  })

  child.on('exit', (code) => {
    if (code && code !== 0) {
      console.error(`[terminal-sidecar] terminal server exited with code ${code}`)
    }
  })

  try {
    const port = await waitForTerminalWsPort()
    console.log(`[terminal-sidecar] terminal websocket server ready on port ${port}`)
  } catch (err) {
    console.error('[terminal-sidecar]', err instanceof Error ? err.message : String(err))
    cleanup()
    return () => {}
  }

  return cleanup
}
