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
const performanceMode = process.argv.includes('--performance')
try {
  const config = { source: 'custom', customIps: '127.0.0.1', maxCandidates: 1, regions: ['HK', 'JP'] }
  const oldTime = '2026-10-01T00:00:00Z'
  const initialState = JSON.stringify({
    config,
    lastRun: null,
    favoriteTest: {
      favoriteIds: [],
      run: {
        id: 'previous-favorite-test', status: 'completed', startedAt: oldTime, finishedAt: oldTime,
        config: { ...config, stabilityAttempts: 110, downloadBudgetEnabled: false },
        progress: {
          stage: 'completed', completed: 1, total: 1, downloadedBytes: 0, discoveredByRegion: { HK: 1 }, message: '收藏复测完成',
          networkPath: { sampleIp: '127.0.0.1', interfaceName: 'en8', interfaceKind: 'wired', tunnelInterface: false, capturedAt: oldTime },
        },
        results: [{
          ip: '127.0.0.1', regionCode: 'HK', city: '', colo: 'HKG', observedColos: ['HKG'],
          tcpMs: 12, httpsMs: 0, jitterMs: 0, downloadMbps: 0, successCount: 1, attempts: 1,
          downloadSamplesMbps: [], eligible: false, exclusionCode: 'stabilityIncomplete',
        }],
      },
    },
  })
  await writeFile(statePath, initialState)

  for (const phase of performanceMode ? ['performance-1000', 'performance-10000'] : ['exercise', 'reopen']) {
    if (performanceMode) await writeFile(statePath, initialState)
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
    if (performanceMode) {
      const metrics = JSON.parse(app.stdout.match(/^desktop smoke metrics (.+)$/m)?.[1] ?? 'null')
      assert.ok(metrics && metrics.downloadUpdateBytes < 128)
      assert.equal(saved.lastRun.results.length, metrics.candidates)
      assert.ok(saved.lastRun.progress.downloadedBytes > 0)
      continue
    }
    assert.equal(saved.lastRun.config.stabilityAttempts, phase === 'exercise' ? 10 : 110)
    assert.equal(saved.lastRun.config.downloadBudgetEnabled, false)
    assert.equal(saved.lastRun.config.downloadBudgetMiB, saved.config.downloadBudgetMiB)
    assert.deepEqual(saved.config.regions, ['HK'])
    assert.equal(saved.config.stabilityAttempts, 100)
    assert.equal(saved.config.downloadBudgetEnabled, false)
    assert.equal(saved.favorites.entries.length, 2)
    assert.equal(saved.lastRun.quickValidation, phase === 'exercise')
    assert.equal(saved.historyBaselines['127.0.0.1'].run.id, 'previous-favorite-test')
    const untested = saved.favorites.entries.find(entry => entry.result.ip === '127.0.0.2')
    assert.equal(untested.runId, '')
    assert.equal(untested.note, '等待期间整理的备注')
    const entry = saved.favorites.entries.find(entry => entry.result.ip === '127.0.0.1')
    if (phase === 'exercise') assert.equal(entry.runId, saved.lastRun.id)
    else {
      assert.equal(saved.lastRun.retestIp, '127.0.0.1')
      assert.notEqual(entry.runId, saved.lastRun.id)
    }
    assert.equal(entry.result.ip, '127.0.0.1')
    assert.equal(entry.result.exclusionCode, 'cancelled')
    assert.equal(entry.note, '桌面冒烟收藏备注')
  }
} finally {
  await rm(directory, { recursive: true, force: true })
}
