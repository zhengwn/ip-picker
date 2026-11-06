import assert from 'node:assert/strict'
import { setImmediate } from 'node:timers/promises'
import test from 'node:test'
import { AllResultsPanel, App, FavoriteSaveDialog, defaultConfig, mountComponent, setInvokeHandler, setWindowFixture } from './support/component-fixture.mjs'

function candidate(ip, fields = {}) {
  return {
    ip, regionCode: 'UNKNOWN', city: '', colo: '', observedColos: [], tcpMs: 0,
    httpsMs: 0, jitterMs: 0, downloadMbps: 0, attempts: 0, successCount: 0,
    downloadSamplesMbps: [], measurementPassed: false, eligible: false, ...fields,
  }
}

function finishedRun(results, discoveredByRegion = {}) {
  return {
    id: 'finished-scan', status: 'completed', startedAt: '2026-01-01T00:00:00Z',
    config: { ...defaultConfig(), regions: ['HK', 'JP'], stabilityAttempts: 10 },
    results,
    progress: { stage: 'completed', completed: results.length, total: results.length, downloadedBytes: 0, discoveredByRegion, message: '' },
  }
}

function savedLibrary(result) {
  const folders = [
    { id: 'unused', name: '未收藏的分类' },
    { id: 'home', name: '家庭宽带' },
    { id: 'mobile', name: '移动流量' },
  ]
  return {
    revision: 1,
    folders,
    entries: [
      { id: 'home-entry', folderId: 'home', result, note: '家用' },
      { id: 'mobile-entry', folderId: 'mobile', result, note: '通勤' },
    ],
  }
}

function mount(context, component, props) {
  const app = mountComponent(component, props)
  context.after(() => app.unmount())
  return app.state
}

test('a saved star opens management without mutating any favorite', async context => {
  const result = candidate('104.16.0.1')
  const run = finishedRun([result])
  const library = savedLibrary(result)
  const originalElement = Object.getOwnPropertyDescriptor(globalThis, 'HTMLElement')
  Object.defineProperty(globalThis, 'HTMLElement', { configurable: true, value: class HTMLElement {} })
  context.after(() => {
    if (originalElement) Object.defineProperty(globalThis, 'HTMLElement', originalElement)
    else delete globalThis.HTMLElement
  })
  const calls = []
  setWindowFixture({ async onCloseRequested() { return () => {} } })
  setInvokeHandler(async (command, args) => {
    calls.push([command, args])
    if (command === 'load_state') return { config: defaultConfig(), lastRun: run, favorites: library }
    if (command === 'get_scan' || command === 'get_favorite_test') return null
    throw new Error(`Unexpected command: ${command}`)
  })
  const state = mount(context, App)
  await setImmediate()
  assert.equal(state.savedIps.value.has(result.ip), true)

  calls.length = 0
  await state.openFavorite(result, { currentTarget: null })
  assert.deepEqual(state.favoriteTarget.value, { runId: run.id, ip: result.ip })
  assert.deepEqual(calls, [])
})

test('favorite management updates or removes the selected folder entry', async context => {
  const result = candidate('104.16.0.1')
  const run = finishedRun([result])
  const library = savedLibrary(result)
  const props = { desktop: true, open: true, run, result, library }
  const calls = []
  const changeEvents = []
  setInvokeHandler(async (command, args) => {
    calls.push([command, args])
    if (command === 'save_favorite') return library
    if (command === 'delete_favorite') return { ...library, revision: 2, entries: library.entries.filter(entry => entry.id !== args.favoriteId) }
    throw new Error(`Unexpected command: ${command}`)
  })

  const updating = mount(context, FavoriteSaveDialog, props)
  assert.equal(updating.folderId.value, 'home')
  updating.folderId.value = 'mobile'
  assert.equal(updating.note.value, '通勤')
  await updating.save()
  assert.deepEqual(calls, [['save_favorite', { folderId: 'mobile', runId: run.id, ip: result.ip, note: '通勤' }]])

  const removing = mount(context, FavoriteSaveDialog, { ...props, onChange: value => changeEvents.push(value) })
  removing.folderId.value = 'mobile'
  await removing.requestRemoval()
  assert.equal(removing.confirmingRemoval.value, true)
  assert.equal(calls.length, 1)
  await removing.remove()
  assert.deepEqual(calls.at(-1), ['delete_favorite', { favoriteId: 'mobile-entry' }])
  assert.equal(changeEvents[0].revision, 2)
  assert.deepEqual(changeEvents[0].entries.map(entry => entry.id), ['home-entry'])
})

test('status counts reflect other filters, including the completed download preference', context => {
  const rejected = candidate('104.16.0.1', {
    regionCode: 'HK', colo: 'HKG', attempts: 1, successCount: 1,
    tcpStability: { attempted: 2, failed: 2, earlyRejection: '累计 2 次连接失败' },
  })
  const downloaded = candidate('104.16.0.2', {
    regionCode: 'JP', colo: 'NRT', attempts: 5, successCount: 5,
    downloadMbps: 30, downloadSamplesMbps: [29, 31], measurementPassed: true, eligible: true,
    tcpStability: { requested: 10, attempted: 10, succeeded: 10, failed: 0, completed: true, probesMs: Array(10).fill(12) },
  })
  const state = mount(context, AllResultsPanel, {
    run: finishedRun([rejected, downloaded], { HK: 1, JP: 1 }), savedIps: new Set(), favoriteDisabled: false,
  })
  assert.equal(state.completedDownloadOnly.value, true)
  assert.equal(state.statusCandidates.value.length, 1)
  assert.equal(state.statusCounts.value.rejected, 0)
  assert.equal(state.statusCounts.value.qualified, 1)

  state.statusFilter.value = 'rejected'
  assert.equal(state.visibleRows.value.length, 0)
  assert.equal(state.statusDownloadConflict.value, true)

  state.completedDownloadOnly.value = false
  assert.equal(state.statusCandidates.value.length, 2)
  assert.equal(state.statusCounts.value.rejected, 1)
  assert.equal(state.statusCounts.value.qualified, 1)
  assert.deepEqual(state.visibleRows.value.map(row => row.result.ip), [rejected.ip])
  assert.equal(state.visibleRows.value[0].reason, rejected.tcpStability.earlyRejection)
  state.query.value = 'NRT'
  assert.equal(state.statusCounts.value.rejected, 0)
  assert.equal(state.statusCounts.value.qualified, 1)
  assert.equal(state.statusDownloadConflict.value, false)
})

test('regional and exclusion diagnostics survive a finished scan with zero precision measurements', context => {
  const noConnection = candidate('104.16.0.1', { exclusionReason: 'TCP 连接超时' })
  const outsideSelection = candidate('104.16.0.2', { regionCode: 'US', colo: 'LAX', attempts: 1, exclusionReason: '实际响应地区不在选择范围内' })
  const state = mount(context, AllResultsPanel, {
    run: finishedRun([noConnection, outsideSelection], { HK: 0, JP: 0, US: 1 }), savedIps: new Set(), favoriteDisabled: false,
  })
  assert.equal(state.visibleRows.value.length, 0)
  assert.deepEqual(state.regionalStats.value.map(({ code, identified, admitted, stabilityPassed, downloaded }) => [code, identified, admitted, stabilityPassed, downloaded]), [
    ['HK', 0, 0, 0, 0], ['JP', 0, 0, 0, 0],
  ])
  assert.ok(state.exclusions.value.some(reason => reason.key === 'connection' && reason.count === 1))
  assert.ok(state.exclusions.value.some(reason => reason.key === 'region-filter' && reason.count === 1))
})

test('coded results filter and summarize independently of diagnostic wording', context => {
  const shortlistedOut = candidate('104.16.0.1', {
    regionCode: 'HK', colo: 'HKG', attempts: 1, successCount: 1,
    tcpStability: { requested: 10, attempted: 10, succeeded: 10, failed: 0, completed: true, probesMs: Array(10).fill(12) },
    exclusionCode: 'downloadShortlist', exclusionReason: 'Reworded shortlist message',
  })
  const connectionFailure = candidate('104.16.0.2', { exclusionCode: 'tcpConnect' })
  const state = mount(context, AllResultsPanel, {
    run: finishedRun([shortlistedOut, connectionFailure], { HK: 1 }), savedIps: new Set(), favoriteDisabled: false,
  })
  state.completedDownloadOnly.value = false
  state.statusFilter.value = 'notDownloaded'
  assert.equal(state.statusCounts.value.notDownloaded, 1)
  assert.equal(state.visibleRows.value[0].reason, 'Reworded shortlist message')
  assert.deepEqual(state.exclusions.value.map(reason => reason.key), ['download-shortlist', 'connection'])
})
