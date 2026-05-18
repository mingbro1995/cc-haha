import * as fs from 'fs'
import * as path from 'path'
import memoize from 'lodash-es/memoize.js'
import { getSecureStorage } from '../../utils/secureStorage/index.js'
import { errorMessage } from '../../utils/errors.js'
import { logError } from '../../utils/log.js'
import type { OpenAIOAuthTokens } from './types.js'

const STORAGE_KEY = 'openaiCodexOauth'
export const OPENAI_CODEX_OAUTH_FILE_ENV_KEY = 'OPENAI_CODEX_OAUTH_FILE'

type SecureStorageShape = Record<string, unknown> & {
  openaiCodexOauth?: OpenAIOAuthTokens
}

function getDesktopTokenFilePath(): string | null {
  const filePath = process.env[OPENAI_CODEX_OAUTH_FILE_ENV_KEY]?.trim()
  return filePath ? filePath : null
}

function normalizeTokenFile(value: unknown): OpenAIOAuthTokens | null {
  if (!value || typeof value !== 'object' || Array.isArray(value)) return null

  const record = value as Record<string, unknown>
  if (
    typeof record.accessToken !== 'string' ||
    typeof record.refreshToken !== 'string' ||
    typeof record.expiresAt !== 'number'
  ) {
    return null
  }

  return {
    accessToken: record.accessToken,
    refreshToken: record.refreshToken,
    expiresAt: record.expiresAt,
    ...(typeof record.idToken === 'string' && { idToken: record.idToken }),
    ...(typeof record.accountId === 'string' && {
      accountId: record.accountId,
    }),
    ...(typeof record.email === 'string' && { email: record.email }),
    ...(typeof record.clientId === 'string' && { clientId: record.clientId }),
  }
}

function readDesktopTokenFileSync(): OpenAIOAuthTokens | null {
  const filePath = getDesktopTokenFilePath()
  if (!filePath) return null

  try {
    return normalizeTokenFile(JSON.parse(fs.readFileSync(filePath, 'utf-8')))
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code !== 'ENOENT') {
      logError(error)
    }
    return null
  }
}

async function readDesktopTokenFileAsync(): Promise<OpenAIOAuthTokens | null> {
  const filePath = getDesktopTokenFilePath()
  if (!filePath) return null

  try {
    const raw = await fs.promises.readFile(filePath, 'utf-8')
    return normalizeTokenFile(JSON.parse(raw))
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code !== 'ENOENT') {
      logError(error)
    }
    return null
  }
}

function writeDesktopTokenFileSync(tokens: OpenAIOAuthTokens): boolean {
  const filePath = getDesktopTokenFilePath()
  if (!filePath) return false

  fs.mkdirSync(path.dirname(filePath), { recursive: true })
  const tmpFile = `${filePath}.tmp.${process.pid}.${Date.now()}`
  fs.writeFileSync(tmpFile, JSON.stringify(tokens, null, 2) + '\n', {
    mode: 0o600,
  })
  fs.renameSync(tmpFile, filePath)
  return true
}

export function saveOpenAIOAuthTokens(tokens: OpenAIOAuthTokens): {
  success: boolean
  warning?: string
} {
  try {
    if (writeDesktopTokenFileSync(tokens)) {
      clearOpenAIOAuthTokenCache()
      return { success: true }
    }

    const storage = getSecureStorage()
    const data = (storage.read() ?? {}) as SecureStorageShape
    data[STORAGE_KEY] = tokens
    const result = storage.update(data)
    clearOpenAIOAuthTokenCache()
    return result
  } catch (error) {
    logError(error)
    return {
      success: false,
      warning: `Failed to save OpenAI OAuth tokens: ${errorMessage(error)}`,
    }
  }
}

export const getOpenAIOAuthTokens = memoize((): OpenAIOAuthTokens | null => {
  const desktopTokens = readDesktopTokenFileSync()
  if (desktopTokens) return desktopTokens

  try {
    const storage = getSecureStorage()
    const data = storage.read() as SecureStorageShape | null
    return data?.openaiCodexOauth ?? null
  } catch (error) {
    logError(error)
    return null
  }
})

export async function getOpenAIOAuthTokensAsync(): Promise<OpenAIOAuthTokens | null> {
  const desktopTokens = await readDesktopTokenFileAsync()
  if (desktopTokens) return desktopTokens

  try {
    const storage = getSecureStorage()
    const data = (await storage.readAsync()) as SecureStorageShape | null
    return data?.openaiCodexOauth ?? null
  } catch (error) {
    logError(error)
    return null
  }
}

export function clearOpenAIOAuthTokenCache(): void {
  getOpenAIOAuthTokens.cache?.clear?.()
}

export function deleteOpenAIOAuthTokens(): boolean {
  try {
    const filePath = getDesktopTokenFilePath()
    if (filePath) {
      fs.rmSync(filePath, { force: true })
      clearOpenAIOAuthTokenCache()
      return true
    }

    const storage = getSecureStorage()
    const data = (storage.read() ?? {}) as SecureStorageShape
    delete data[STORAGE_KEY]
    const result = storage.update(data)
    clearOpenAIOAuthTokenCache()
    return result.success
  } catch (error) {
    logError(error)
    return false
  }
}
