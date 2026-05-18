import { afterEach, beforeEach, describe, expect, test } from 'bun:test'
import * as fs from 'fs'
import * as fsp from 'fs/promises'
import * as os from 'os'
import * as path from 'path'
import {
  clearOpenAIOAuthTokenCache,
  deleteOpenAIOAuthTokens,
  getOpenAIOAuthTokens,
  getOpenAIOAuthTokensAsync,
  saveOpenAIOAuthTokens,
} from './storage.js'

describe('OpenAI OAuth desktop token file storage', () => {
  let tmpDir: string
  let tokenPath: string
  let originalTokenFile: string | undefined

  beforeEach(async () => {
    tmpDir = await fsp.mkdtemp(path.join(os.tmpdir(), 'openai-oauth-storage-'))
    tokenPath = path.join(tmpDir, 'openai-oauth.json')
    originalTokenFile = process.env.OPENAI_CODEX_OAUTH_FILE
    process.env.OPENAI_CODEX_OAUTH_FILE = tokenPath
    clearOpenAIOAuthTokenCache()
  })

  afterEach(async () => {
    if (originalTokenFile === undefined) {
      delete process.env.OPENAI_CODEX_OAUTH_FILE
    } else {
      process.env.OPENAI_CODEX_OAUTH_FILE = originalTokenFile
    }
    clearOpenAIOAuthTokenCache()
    await fsp.rm(tmpDir, { recursive: true, force: true })
  })

  test('reads desktop token file synchronously', async () => {
    await fsp.writeFile(
      tokenPath,
      JSON.stringify({
        accessToken: 'desktop-access',
        refreshToken: 'desktop-refresh',
        expiresAt: 4_100_000_000_000,
        idToken: 'desktop-id-token',
        email: 'user@example.com',
        accountId: 'acct_desktop',
      }),
      'utf-8',
    )

    const tokens = getOpenAIOAuthTokens()

    expect(tokens).toMatchObject({
      accessToken: 'desktop-access',
      refreshToken: 'desktop-refresh',
      expiresAt: 4_100_000_000_000,
      idToken: 'desktop-id-token',
      email: 'user@example.com',
      accountId: 'acct_desktop',
    })
  })

  test('reads desktop token file asynchronously', async () => {
    await fsp.writeFile(
      tokenPath,
      JSON.stringify({
        accessToken: 'async-access',
        refreshToken: 'async-refresh',
        expiresAt: 4_100_000_000_000,
        email: null,
        accountId: null,
      }),
      'utf-8',
    )

    const tokens = await getOpenAIOAuthTokensAsync()

    expect(tokens?.accessToken).toBe('async-access')
    expect(tokens?.refreshToken).toBe('async-refresh')
  })

  test('writes refreshed tokens back to the desktop token file', async () => {
    const result = saveOpenAIOAuthTokens({
      accessToken: 'fresh-access',
      refreshToken: 'fresh-refresh',
      expiresAt: 4_100_000_000_000,
      idToken: 'fresh-id-token',
      email: 'fresh@example.com',
      accountId: 'acct_fresh',
    })

    expect(result).toEqual({ success: true })
    const raw = JSON.parse(
      fs.readFileSync(tokenPath, 'utf-8'),
    ) as Record<string, unknown>
    expect(raw).toMatchObject({
      accessToken: 'fresh-access',
      refreshToken: 'fresh-refresh',
      idToken: 'fresh-id-token',
      email: 'fresh@example.com',
      accountId: 'acct_fresh',
    })
    if (process.platform !== 'win32') {
      expect(fs.statSync(tokenPath).mode & 0o777).toBe(0o600)
    }
  })

  test('deletes the desktop token file when the env override is set', async () => {
    await fsp.writeFile(
      tokenPath,
      JSON.stringify({
        accessToken: 'desktop-access',
        refreshToken: 'desktop-refresh',
        expiresAt: 4_100_000_000_000,
      }),
      'utf-8',
    )

    expect(deleteOpenAIOAuthTokens()).toBe(true)
    expect(fs.existsSync(tokenPath)).toBe(false)
  })
})
