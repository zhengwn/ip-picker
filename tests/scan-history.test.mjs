import assert from 'node:assert/strict'
import { setImmediate } from 'node:timers/promises'
import test from 'node:test'
import { ScanHistoryPanel, defaultConfig, mountComponent, setInvokeHandler } from './support/component-fixture.mjs'

function match(ip, interfaceName, tcpStability) {
  const startedAt = '2026-11-11T08:00:00Z'
  const networkPath = { sampleIp: ip, interfaceName, tunnelInterface: false, capturedAt: startedAt }
  return {
    kind: 'favorite',
    run: {
      id: interfaceName, status: 'completed', startedAt,
      config: { ...defaultConfig(), stabilityAttempts: 10 },
      progress: { stage: 'completed', networkPath },
      results: [{
        ip, regionCode: 'HK', colo: 'HKG', observedColos: ['HKG'], tcpMs: 20,
        attempts: 1, successCount: 1, downloadMbps: 0, downloadSamplesMbps: [],
        measurementPassed: false, eligible: false, tcpStability,
      }],
    },
  }
}

test('history compares the exact requested IP and scales TCP charts across matching runs', async context => {
  const oldIp = '104.16.0.1'
  const newIp = '104.16.0.2'
  const unfinished = { requested: 10, attempted: 4, failed: 2, completed: false, probesMs: [20, 400, null, null], p95Ms: 400, maxMs: 400, earlyRejection: '累计 2 次失败' }
  const completed = { requested: 10, attempted: 10, failed: 0, completed: true, probesMs: [...Array(9).fill(20), 32] }
  const wired = match(newIp, 'en0', unfinished)
  const wifi = match(newIp, 'en8', completed)
  wifi.run.progress.networkPath.sampleIp = oldIp
  wifi.run.config.targetUrl = 'https://speed.cloudflare.com/__down?bytes=1048576'
  wifi.run.config.stabilityIntervalMs = 300
  let finishOldRequest
  const calls = []
  setInvokeHandler(async (command, args) => {
    calls.push([command, args])
    if (command === 'get_scan_history') return { limit: 10, entries: [] }
    if (command === 'compare_scan_history_ip' && args.ip === oldIp) return new Promise(resolve => { finishOldRequest = resolve })
    if (command === 'compare_scan_history_ip' && args.ip === newIp) return [wired, wifi]
    throw new Error(`Unexpected command: ${command}`)
  })
  const mounted = mountComponent(ScanHistoryPanel, { desktop: true, active: true, revision: 0 })
  context.after(() => mounted.unmount())
  const { state } = mounted
  await setImmediate()

  state.compareIp('104.16')
  assert.match(state.inputError.value, /IPv4/)
  assert.equal(calls.length, 1)
  state.compareIp(oldIp)
  await setImmediate()
  state.openComparison(` ${newIp} `)
  await setImmediate()
  finishOldRequest([match(oldIp, 'en9', completed)])
  await setImmediate()

  assert.deepEqual(calls.map(([command, args]) => [command, args?.ip]), [
    ['get_scan_history', undefined], ['compare_scan_history_ip', oldIp], ['compare_scan_history_ip', newIp],
  ])
  assert.deepEqual(state.rows.value.map(row => [row.result.ip, row.run.progress.networkPath.interfaceName, row.result.tcpStability]), [
    [newIp, 'en0', unfinished], [newIp, 'en8', completed],
  ])
  assert.equal(state.rows.value[1].run.progress.networkPath.sampleIp, oldIp)
  assert.deepEqual([...state.changedSettings.value], ['测速目标', 'TCP 探测'])
  assert.equal(state.infoLabel.value, '对比条件有差异')
  assert.deepEqual(state.comparisonWarnings.value, ['测速参数不同', '部分接口来自其他 IP 的采样'])
  assert.equal(state.rows.value[1].settings.find(setting => setting.label === '测速目标').value, wifi.run.config.targetUrl)
  assert.match(state.rows.value[1].settings.find(setting => setting.label === 'TCP 探测').value, /间隔 300 ms/)
  assert.ok(state.tcpScale.value >= 400)
})

test('history does not confirm a missing POP after a full scan was stopped during a POP recheck', async context => {
  const ip = '104.16.0.1'
  const complete = match(ip, 'en0')
  const interrupted = match(ip, 'en0')
  interrupted.run.id = 'interrupted'
  interrupted.run.status = 'cancelled'
  Object.assign(complete.run.results[0], { downloadSamplesMbps: [35, 45], observedColos: ['HKG', 'LAX'] })
  Object.assign(interrupted.run.results[0], { downloadSamplesMbps: [35, 45], exclusionCode: 'popRecheckIncomplete' })
  setInvokeHandler(async command => command === 'get_scan_history' ? { limit: 10, entries: [] } : [complete, interrupted])
  const mounted = mountComponent(ScanHistoryPanel, { desktop: true, active: true, revision: 0 })
  context.after(() => mounted.unmount())
  mounted.state.openComparison(ip)
  await setImmediate()

  assert.ok(mounted.state.comparisonWarnings.value.includes('接入机房未完整记录'))
  assert.ok(!mounted.state.comparisonWarnings.value.includes('接入机房不同'))
  assert.equal(mounted.state.infoLabel.value, '对比条件待确认')
})

test('download-only history identifies the timeout difference that triggers a settings warning', async context => {
  const ip = '104.16.0.1'
  const entries = [3000, 5000].map(requestTimeoutMs => {
    const entry = match(ip, 'en0')
    entry.run.id = `download-${requestTimeoutMs}`
    entry.run.downloadOnly = true
    entry.run.config.requestTimeoutMs = requestTimeoutMs
    return entry
  })
  setInvokeHandler(async command => command === 'get_scan_history' ? { limit: 10, entries: [] } : entries)
  const mounted = mountComponent(ScanHistoryPanel, { desktop: true, active: true, revision: 0 })
  context.after(() => mounted.unmount())
  mounted.state.openComparison(ip)
  await setImmediate()

  assert.deepEqual(mounted.state.comparisonWarnings.value, ['测速参数不同'])
  assert.deepEqual([...mounted.state.changedSettings.value], ['下载超时'])
  assert.deepEqual(mounted.state.rows.value.map(row => row.settings.find(setting => setting.label === '下载超时').value), [
    '连接 1500 ms · 响应头 3000 ms', '连接 1500 ms · 响应头 5000 ms',
  ])
})

test('history compares whether legacy TCP latency screening was enabled and its threshold', async context => {
  const ip = '104.16.0.1'
  const entries = [200, 300, null, 200].map((maxTcpMs, index) => {
    const entry = match(ip, `history-${index}`)
    entry.run.config = {
      ...entry.run.config, stabilityAttempts: null, stabilityFirst: false,
      tcpLatencyScreening: index !== 3, maxTcpMs,
    }
    return entry
  })
  setInvokeHandler(async (command, args) => {
    if (command === 'get_scan_history') return { limit: 10, entries: [] }
    if (command === 'compare_scan_history_ip' && args.ip === ip) return entries
    throw new Error(`Unexpected command: ${command}`)
  })
  const mounted = mountComponent(ScanHistoryPanel, { desktop: true, active: true, revision: 0 })
  context.after(() => mounted.unmount())
  mounted.state.openComparison(ip)
  await setImmediate()

  assert.deepEqual([...mounted.state.changedSettings.value], ['TCP 初筛'])
  assert.deepEqual(mounted.state.rows.value.map(row => row.settings.find(setting => setting.label === 'TCP 初筛').value), [
    '开启 · 单次延迟 ≤ 200 ms', '开启 · 单次延迟 ≤ 300 ms', '开启 · 无延迟上限', '关闭 · 仅检查连通性',
  ])
})

test('a historical run starts with precision records and applies IP, POP and status filters before pagination', async context => {
  const primaryIp = '104.16.2.1'
  const secondaryIp = '104.16.2.2'
  const early = { requested: 10, attempted: 2, failed: 2, completed: false, probesMs: [null, null], earlyRejection: '累计 2 次失败' }
  const { run } = match(primaryIp, 'history-run', early)
  const discoveryOnly = { ...run.results[0], tcpStability: { ...early, attempted: 0, failed: 0, probesMs: [], earlyRejection: null } }
  run.results.push(
    { ...run.results[0], ip: secondaryIp, colo: 'LAX', observedColos: ['LAX'], tcpStability: { ...early, attempted: 3, failed: 0, probesMs: [20, 21, 22], earlyRejection: null } },
    ...Array.from({ length: 51 }, (_, index) => ({ ...discoveryOnly, ip: `104.16.1.${index + 1}` })),
  )
  setInvokeHandler(async (command, args) => {
    if (command === 'get_scan_history') return { limit: 10, entries: [] }
    if (command === 'get_scan_history_run' && args.runId === run.id) return run
    throw new Error(`Unexpected command: ${command}`)
  })
  const mounted = mountComponent(ScanHistoryPanel, { desktop: true, active: true, revision: 0 })
  context.after(() => mounted.unmount())
  const { state } = mounted
  state.infoOpen.value = true
  state.openRun(run.id, 'scan')
  await setImmediate()

  assert.deepEqual(state.rows.value.map(row => row.result.ip), [primaryIp, secondaryIp])
  assert.equal(state.infoOpen.value, false)
  assert.equal(state.rows.value[0].settings.find(setting => setting.label === '测速目标').value, run.config.targetUrl)
  assert.equal(state.pageCount.value, 1)
  state.runScope.value = 'all'
  await setImmediate()
  assert.equal(state.pageCount.value, 2)
  state.page.value = 2
  assert.equal(state.rows.value.length, 3)

  state.runIpQuery.value = '104.16.2.'
  state.runPop.value = 'HKG'
  state.runStatus.value = '稳定性提前淘汰'
  await setImmediate()
  assert.equal(state.page.value, 1)
  assert.deepEqual(state.rows.value.map(row => row.result.ip), [primaryIp])

  state.runPop.value = 'LAX'
  await setImmediate()
  assert.equal(state.rows.value.length, 0)
  state.clearRunFilters()
  await setImmediate()
  assert.equal(state.runScope.value, 'all')
  assert.equal(state.filteredRunRows.value.length, 53)
})


test('an indirect route sample remains visible as uncertainty after comparison metadata is collapsed', async context => {
  const ip = '104.16.0.1'
  const first = match(ip, 'en0')
  const second = match(ip, 'en8')
  second.run.id = 'second-run'
  second.run.progress.networkPath.sampleIp = '104.16.0.2'
  setInvokeHandler(async command => command === 'get_scan_history' ? { limit: 10, entries: [] } : [first, second])
  const mounted = mountComponent(ScanHistoryPanel, { desktop: true, active: true, revision: 0 })
  context.after(mounted.unmount)
  mounted.state.openComparison(ip)
  await setImmediate()
  assert.equal(mounted.state.changedSettings.value.size, 0)
  assert.deepEqual(mounted.state.comparisonWarnings.value, ['部分接口来自其他 IP 的采样'])
  assert.equal(mounted.state.infoLabel.value, '对比条件待确认')
  assert.equal(mounted.state.rows.value[1].run.progress.networkPath.sampleIp, '104.16.0.2')
})
