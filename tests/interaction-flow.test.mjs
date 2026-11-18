import assert from 'node:assert/strict'
import { setImmediate } from 'node:timers/promises'
import test from 'node:test'
import { AllResultsPanel, FavoritesPanel, defaultConfig, mountComponent, reactive, setInvokeHandler, favoriteTestChange, comparisonContextDifferences, preferredFavoriteFolder, rememberFavoriteFolder, compactScanProgress, scanModeLabel, readResultPreferences } from './support/component-fixture.mjs'

function result(ip, averageMs = 30, p95Ms = 60) {
  return { ip, regionCode: 'HK', city: '', colo: 'HKG', observedColos: ['HKG'], tcpMs: 15,
    httpsMs: 20, jitterMs: 0, attempts: 5, successCount: 5, eligible: true, measurementPassed: true,
    downloadMbps: 30, downloadSamplesMbps: [30, 30],
    tcpStability: { requested: 100, attempted: 100, succeeded: 100, failed: 0, completed: true, probesMs: [...Array(94).fill(averageMs), ...Array(5).fill(p95Ms), p95Ms + 10], averageMs, medianMs: averageMs, p95Ms, maxMs: p95Ms + 10 },
  }
}
function run(results) {
  return { id: 'current', status: 'completed', startedAt: '2026-11-16T00:00:00Z', config: defaultConfig(), results,
    progress: { stage: 'completed', completed: 2, total: 2, downloadedBytes: 100, discoveredByRegion: { HK: 2 }, message: '' } }
}
function mount(context, component, props) {
  const app = mountComponent(component, props)
  context.after(app.unmount)
  return app.state
}

test('displayed latency and latency sorting stay aligned in both directions; download sorting is preserved', context => {
  const lowAverage = result('104.16.0.1', 20, 90)
  const lowP95 = result('104.16.0.2', 40, 50)
  const state = mount(context, AllResultsPanel, { run: run([lowAverage, lowP95]), savedIps: new Set(), favoriteDisabled: false })
  assert.equal(state.filtered.value[0].result.ip, lowP95.ip)
  state.tcpDisplayMetric.value = 'average'
  assert.equal(state.sortBy.value, 'averageTcp')
  assert.equal(state.filtered.value[0].result.ip, lowAverage.ip)
  state.sortBy.value = 'peakTcp'
  assert.equal(state.tcpDisplayMetric.value, 'peak')
  assert.equal(state.filtered.value[0].result.ip, lowP95.ip)
  state.sortBy.value = 'download'
  state.tcpDisplayMetric.value = 'median'
  assert.equal(state.sortBy.value, 'download')
})

test('viewing running observations does not overwrite the completed-download preference', async context => {
  const current = reactive(run([{ ...result('104.16.0.1'), measurementPassed: false, downloadSamplesMbps: [], downloadMbps: 0 }]))
  current.status = 'running'
  current.progress.stage = 'stabilizing'
  const state = mount(context, AllResultsPanel, { run: current, savedIps: new Set(), favoriteDisabled: false })
  assert.equal(state.filtered.value.length, 0)
  state.showObservations()
  assert.equal(state.filtered.value.length, 1)
  assert.equal(state.completedDownloadOnly.value, true)
  current.id = 'next-run'
  await setImmediate()
  assert.equal(state.downloadFilterActive.value, true)
})

test('result shortcuts clear unrelated filters and keep full evidence available', context => {
  const failed = { ...result('104.16.0.2'), eligible: false, measurementPassed: false, downloadSamplesMbps: [], exclusionCode: 'downloadBudget' }
  const state = mount(context, AllResultsPanel, { run: run([result('104.16.0.1'), failed]), savedIps: new Set(), favoriteDisabled: false })
  state.query.value = 'no-match'
  state.showQualified('download')
  assert.equal(state.visibleRows.value.length, 1)
  assert.equal(state.visibleRows.value[0].result.ip, '104.16.0.1')
  assert.equal(state.hasBudgetExclusions.value, true)
  state.showObservations()
  assert.equal(state.visibleRows.value.length, 2)
})

test('saving an address and editing notes stay available while a measurement is running', async context => {
  const calls = []
  const props = reactive({ desktop: true, scanBusy: true, config: defaultConfig(), library: { folders: [{ id: 'home', name: '家用' }], entries: [] } })
  setInvokeHandler(async (command, args) => { calls.push([command, args]); return props.library })
  const state = mount(context, FavoritesPanel, props)
  assert.equal(state.disabled.value, false)
  assert.equal(state.testDisabled.value, true)
  state.action.value = 'addIp'
  state.manualIp.value = '104.16.0.1'
  state.folderId.value = 'home'
  state.note.value = '稍后测试'
  state.search.value = 'another-ip'
  state.showChangesOnly.value = true
  await state.submit()
  assert.deepEqual(calls.map(([command]) => command), ['add_favorite_ip'])
  assert.equal(calls[0][1].note, '稍后测试')
  assert.equal(state.search.value, '', 'a newly saved address must not be hidden by an old search')
  assert.equal(state.showChangesOnly.value, false)
  await state.startTest({ ip: '104.16.0.1', folderId: 'home' })
  assert.equal(calls.length, 1)
  state.action.value = 'editEntry'
  state.actionEntry.value = { id: 'existing', folderId: 'home' }
  state.note.value = '测速中更新备注'
  await state.submit()
  assert.equal(calls.at(-1)[0], 'update_favorite')
  state.action.value = 'deleteEntry'
  await state.submit()
  assert.equal(calls.length, 2)
})

test('favorite changes count follows the selected folder while retesting keeps the whole folder scope', async context => {
  const config = defaultConfig()
  const library = {
    folders: [{ id: 'home', name: '家用' }, { id: 'work', name: '办公' }],
    entries: ['a', 'b', 'c'].map((id, index) => ({
      id, folderId: id === 'b' ? 'work' : 'home', savedAt: '2026-11-15T00:00:00Z',
      runId: 'current', runStatus: 'completed', runStartedAt: '2026-11-16T00:00:00Z', note: '', config,
      result: result(`104.16.0.${index + 1}`),
    })),
  }
  const job = {
    run: run(library.entries.map(entry => entry.result)), favoriteIds: ['a', 'b', 'c'],
    baselines: library.entries.map(entry => ({ favoriteId: entry.id, testedAt: entry.savedAt, config, colo: 'HKG', measurementPassed: true, tcpP95Ms: entry.id === 'c' ? 60 : 80, downloadMbps: 30 })),
  }
  const calls = []
  setInvokeHandler(async (command, args) => {
    calls.push([command, args])
    return { favorites: library, job: { run: { ...job.run, id: 'next', status: 'running' }, favoriteIds: ['a', 'c'] } }
  })
  const state = mount(context, FavoritesPanel, { desktop: true, config, library, testJob: job })
  assert.equal(state.changedCount.value, 2)
  state.selectedFolderId.value = 'home'
  state.showChangesOnly.value = true
  assert.equal(state.changedCount.value, 1)
  assert.deepEqual(state.visibleEntries.value.map(entry => entry.id), ['a'])
  assert.equal(state.entries.value.length, 2)
  await state.startTest({ folderId: 'home' })
  assert.equal(calls[0][0], 'start_favorite_test')
  assert.equal(calls[0][1].folderId, 'home')
  assert.equal(calls[0][1].favoriteId, undefined)
})

test('before/after comparisons distinguish changed conditions, partial data and untouched entries', () => {
  const config = defaultConfig()
  const networkPath = { interfaceName: 'en0', interfaceKind: 'wifi', tunnelInterface: false, sampleIp: '104.16.0.1' }
  const entry = { id: 'saved', runId: 'current', config, networkPath, result: result('104.16.0.1', 20, 40) }
  const job = { run: run([entry.result]), favoriteIds: ['saved'], baselines: [{ favoriteId: 'saved', testedAt: '2026-11-15T00:00:00Z', config, networkPath, colo: 'HKG', measurementPassed: true, tcpP95Ms: 60, downloadMbps: 20 }] }
  const change = favoriteTestChange(entry, job)
  assert.match(change.label, /P95 −20.0 ms.*下载 \+10.0 Mbps/)
  assert.deepEqual(change.differences, [])
  entry.config = { ...config, stabilityAttempts: 10 }
  entry.networkPath = { ...networkPath, interfaceName: 'en8' }
  assert.deepEqual(favoriteTestChange(entry, job).differences, ['测速参数不同', '网络接口不同'])
  entry.networkPath.sampleIp = '104.16.0.2'
  assert.deepEqual(favoriteTestChange(entry, job).differences, ['测速参数不同', '部分接口来自其他 IP 的采样'])
  entry.result.downloadSamplesMbps = [30]
  assert.match(favoriteTestChange(entry, job).label, /下载本次未测满/)
  entry.runId = 'previous'
  assert.equal(favoriteTestChange(entry, job).label, '本轮未更新')
  assert.equal(favoriteTestChange(entry, job).changed, false)
  assert.deepEqual(comparisonContextDifferences({ config, colo: '' }, { config, colo: '' }), ['网络路径未完整记录'])
})

test('last-used folder preference falls back after deletion and tolerates unavailable storage', context => {
  const original = Object.getOwnPropertyDescriptor(globalThis, 'window')
  context.after(() => { if (original) Object.defineProperty(globalThis, 'window', original); else delete globalThis.window })
  const values = new Map()
  globalThis.window = { localStorage: { getItem: key => values.get(key), setItem: (key, value) => values.set(key, value) } }
  const folders = [{ id: 'first' }, { id: 'last-used' }]
  rememberFavoriteFolder('last-used')
  assert.equal(preferredFavoriteFolder(folders), 'last-used')
  assert.equal(preferredFavoriteFolder(folders.slice(0, 1)), 'first')
  globalThis.window.localStorage.getItem = () => { throw new Error('blocked') }
  assert.equal(preferredFavoriteFolder(folders), 'first')
})

test('legacy sort-only preferences preserve their chosen latency order', context => {
  const original = Object.getOwnPropertyDescriptor(globalThis, 'window')
  context.after(() => { if (original) Object.defineProperty(globalThis, 'window', original); else delete globalThis.window })
  let saved = { sortBy: 'averageTcp' }
  globalThis.window = { localStorage: { getItem: () => JSON.stringify(saved) } }
  assert.deepEqual(readResultPreferences(), { completedDownloadOnly: true, sortBy: 'averageTcp', tcpDisplayMetric: 'average' })
  saved = { sortBy: 'tcpJitter', completedDownloadOnly: false }
  assert.deepEqual(readResultPreferences(), { completedDownloadOnly: false, sortBy: 'averageTcp', tcpDisplayMetric: 'average' })
  saved = { sortBy: 'tcp', tcpDisplayMetric: 'peak' }
  assert.equal(readResultPreferences().sortBy, 'peakTcp')
})

test('retest comparison does not report missing evidence as unchanged or annotate a newer saved snapshot', () => {
  const config = defaultConfig()
  const entry = { id: 'saved', savedAt: '2026-11-15T00:00:00Z', runId: 'current', config, result: { ...result('104.16.0.1'), measurementPassed: false, tcpStability: undefined, downloadSamplesMbps: [] } }
  const job = { run: run([entry.result]), favoriteIds: ['saved'], baselines: [{ favoriteId: 'saved', testedAt: entry.savedAt, config, colo: 'HKG', measurementPassed: false, tcpP95Ms: null, downloadMbps: null }] }
  assert.equal(favoriteTestChange(entry, job).label, '暂无可比较的完整测量')
  entry.runId = 'newer-snapshot'
  entry.savedAt = '2026-11-17T00:00:00Z'
  assert.equal(favoriteTestChange(entry, job), null)
  entry.runId = job.run.id
  entry.result = result('104.16.0.1', 20, 40)
  entry.result.tcpStability.p95Ms = 999
  job.baselines[0].tcpP95Ms = 60
  assert.match(favoriteTestChange(entry, job).label, /P95 −20.0 ms/)
})

test('compact progress shows the actual stage, probe count and complete result count', () => {
  const current = run([result('104.16.0.1')])
  current.status = 'running'
  current.progress = { ...current.progress, stage: 'stabilizing', completed: 120, total: 400 }
  assert.equal(scanModeLabel({ quickValidation: true }), '快速验证')
  assert.equal(scanModeLabel({ quickValidation: false }), '完整优选')
  assert.equal(scanModeLabel({}), '历史测速')
  const progress = compactScanProgress(current)
  assert.equal(progress.step, '阶段 3 / 5')
  assert.equal(progress.caption, '已探测 120 / 400 次')
  assert.equal(progress.complete, 1)
})
