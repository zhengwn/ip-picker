import assert from 'node:assert/strict'
import { setImmediate } from 'node:timers/promises'
import test from 'node:test'
import { App, defaultConfig, estimateScan, mountComponent, setInvokeHandler, setWindowFixture } from './support/component-fixture.mjs'

test('the default scan shows a workload estimate before starting and updates downloaded bytes during the run', async context => {
  const config = defaultConfig()
  const run = {
    id: 'regional-scan', status: 'running', startedAt: '2026-01-01T00:00:00Z',
    config, results: [],
    progress: { stage: 'connecting', completed: 0, total: config.maxCandidates, downloadedBytes: 0, discoveredByRegion: {}, message: '' },
  }
  setWindowFixture({ async onCloseRequested() { return () => {} } })
  setInvokeHandler(async command => {
    if (command === 'load_state') return { config }
    if (command === 'get_scan' || command === 'get_favorite_test') return null
    if (command === 'start_scan') return run
    throw new Error(`Unexpected command: ${command}`)
  })
  const { state, unmount } = mountComponent(App)
  context.after(unmount)
  await setImmediate()
  assert.equal(state.scanEstimate.value.duration, '约 14 分钟')
  assert.equal(state.scanEstimate.value.download, '约 1.9 GiB')

  await state.startScan()
  assert.equal(state.scanEstimate.value, null)
  assert.equal(state.scanDownloaded.value, '0.0 MiB')

  state.receiveRun({ ...run, progress: { ...run.progress, stage: 'downloading', downloadedBytes: 3.5 * 1024 ** 2 } })
  assert.equal(state.scanDownloaded.value, '3.5 MiB')
})

test('the download estimate respects the budget without guessing custom response sizes', () => {
  const config = defaultConfig()
  assert.equal(estimateScan({ ...config, downloadBudgetEnabled: true }).download, '最多 400 MiB')
  assert.equal(estimateScan({ ...config, downloadBudgetEnabled: true, downloadBudgetMiB: 4096 }).download, '约 1.9 GiB')

  const custom = { ...config, targetUrl: 'https://example.com/__down?bytes=25000000' }
  assert.equal(estimateScan(custom).download, '随响应变化')
  assert.equal(estimateScan({ ...custom, downloadBudgetEnabled: true }).download, '最多 400 MiB')
})

test('small custom lists use address capacity and a large CIDR respects the sampling limit', () => {
  const config = { ...defaultConfig(), source: 'custom' }
  const small = { ...config, customIps: '192.0.2.0/31 # two addresses\n192.0.2.4, 192.0.2.4; 192.0.2.5/32' }
  assert.equal(estimateScan(small).download, '约 191 MiB')
  const overlapping = { ...config, customIps: '192.0.2.1/30, 192.0.2.0/30, 192.0.2.2/31, 192.0.2.1' }
  assert.equal(estimateScan(overlapping).download, estimateScan(small).download)
  assert.equal(estimateScan({ ...config, customIps: '0.0.0.0/0', maxCandidates: 3 }).download, '约 144 MiB')
})

test('a favorite retest exposes its download total and stop action in the history workspace', async context => {
  const calls = []
  setWindowFixture({ async onCloseRequested() { return () => {} } })
  setInvokeHandler(async (command, args) => {
    if (command === 'load_state') return { config: defaultConfig() }
    if (command === 'get_scan' || command === 'get_favorite_test') return null
    if (command === 'get_favorites') return { revision: 0, folders: [], entries: [] }
    if (command === 'cancel_favorite_test') { calls.push(args.runId); return }
    throw new Error(`Unexpected command: ${command}`)
  })
  const { state, unmount } = mountComponent(App)
  context.after(unmount)
  await setImmediate()
  const retest = {
    favoriteIds: ['favorite'],
    run: {
      id: 'retest', status: 'running', config: defaultConfig(), results: [],
      progress: { stage: 'downloading', downloadedBytes: 4 * 1024 ** 2, completed: 1, total: 2 },
    },
  }
  state.receiveFavoriteTest(retest)
  await state.showWorkspace('history')
  assert.equal(state.showGlobalTask.value, true)
  assert.equal(state.activeDownloaded.value, '4.0 MiB')
  await state.stopMeasurement()
  await state.stopMeasurement()
  assert.deepEqual(calls, ['retest'])
  assert.equal(state.stopping.value, true)
  state.receiveFavoriteTest({ ...retest, run: { ...retest.run, status: 'cancelled' } })
  assert.equal(state.showGlobalTask.value, false)
  assert.equal(state.stopping.value, false)
})
