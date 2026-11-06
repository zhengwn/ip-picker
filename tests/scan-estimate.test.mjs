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
  assert.equal(estimateScan({ ...config, customIps: '0.0.0.0/0', maxCandidates: 3 }).download, '约 144 MiB')
})
