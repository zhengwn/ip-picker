import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import { mkdtemp, readFile, rm, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { fileURLToPath } from 'node:url'

const root = fileURLToPath(new URL('../', import.meta.url))
const build = spawnSync('cargo', [
  'build', '--locked', '--manifest-path', 'src-tauri/Cargo.toml', '--example', 'desktop_smoke',
  '--features', 'desktop-smoke', '--message-format=json-render-diagnostics',
], { cwd: root, encoding: 'utf8', stdio: ['ignore', 'pipe', 'inherit'] })
if (build.error) throw build.error
const messages = build.stdout.trim().split('\n').filter(Boolean).map(line => JSON.parse(line))
for (const message of messages) {
  if (message.reason === 'compiler-message' && message.message.rendered) process.stderr.write(message.message.rendered)
}
assert.equal(build.status, 0, 'desktop smoke build failed')
const executable = messages.find(message => message.target?.name === 'desktop_smoke' && message.executable)?.executable
assert.ok(executable, 'desktop smoke executable not found')

const directory = await mkdtemp(join(tmpdir(), 'ip-picker-smoke-'))
const statePath = join(directory, 'state.json')
try {
  await writeFile(statePath, JSON.stringify({
    config: { source: 'custom', customIps: '127.0.0.1', maxCandidates: 1, regions: ['HK', 'JP'] },
    lastRun: null,
  }))

  for (const phase of ['exercise', 'reopen']) {
    const app = spawnSync(executable, [], {
      cwd: root,
      encoding: 'utf8',
      stdio: ['ignore', 'pipe', 'inherit'],
      timeout: 30000,
      killSignal: 'SIGKILL',
      env: { ...process.env, IP_PICKER_SMOKE_DATA_DIR: directory, IP_PICKER_SMOKE_PHASE: phase },
    })
    if (app.stdout) process.stdout.write(app.stdout)
    if (app.error) throw app.error
    assert.equal(app.status, 0, `desktop smoke ${phase} failed`)
    assert.ok(app.stdout.includes(`desktop smoke ${phase} passed`), `desktop smoke ${phase} exited before verification`)

    const saved = JSON.parse(await readFile(statePath, 'utf8'))
    assert.equal(saved.lastRun.status, 'cancelled')
    assert.deepEqual(saved.config.regions, ['HK'])
    assert.equal(saved.favorites.entries.length, 1)
    const [entry] = saved.favorites.entries
    assert.equal(entry.runId, saved.lastRun.id)
    assert.equal(entry.result.ip, '127.0.0.1')
    assert.equal(entry.result.exclusionCode, 'cancelled')
    assert.equal(entry.note, '桌面冒烟收藏备注')
  }
} finally {
  await rm(directory, { recursive: true, force: true })
}
