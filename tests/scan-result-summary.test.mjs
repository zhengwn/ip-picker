import assert from 'node:assert/strict'
import test from 'node:test'
import { defaultConfig, stabilityFailureKind, summarizeScanResults } from './support/component-fixture.mjs'

const config = { ...defaultConfig(), regions: ['HK'], stabilityAttempts: 10 }

function candidate(fields = {}) {
  return { tcpMs: 0, attempts: 0, colo: '', downloadSamplesMbps: [], ...fields }
}

function stability(probesMs, fields = {}) {
  const succeeded = probesMs.filter(probe => probe !== null).length
  return {
    attempted: probesMs.length, requested: 10, succeeded, failed: probesMs.length - succeeded,
    completed: probesMs.length === 10, probesMs, ...fields,
  }
}

function run(results, status = 'completed') {
  return {
    config, results, status,
    progress: { stage: status, completed: 0, downloadedBytes: 0, discoveredByRegion: {} },
  }
}

test('round summary separates initial TCP failures from the two precision failure causes', () => {
  const initialTimeout = candidate({ exclusionReason: 'TCP 连接超时' })
  const initialRefused = candidate({ tcpConnectFailure: { kind: 'connect', elapsedMs: 0.4 } })
  const earlyRejection = 'TCP 稳定性提前淘汰：累计 2 次连接失败，达到 2 次淘汰阈值'
  const stopped = candidate({ tcpMs: 35, tcpStability: stability([null, 35, null], { earlyRejection }), exclusionReason: earlyRejection })
  const slow = candidate({
    tcpMs: 58, tcpStability: stability(Array(10).fill(288.7)),
    exclusionReason: 'TCP 稳定性P95耗时 288.7 ms，超过上限 230 ms',
  })
  const unselected = candidate({ tcpMs: 45, exclusionCode: 'regionMismatch' })

  assert.deepEqual(summarizeScanResults(run([initialTimeout, initialRefused, stopped, slow, unselected]), 0), {
    totalCandidates: 5, checkedCandidates: 5, initialTcpFailures: 2, precisionCount: 2, stabilityPassed: 0,
    stabilityFailures: 2, downloadedCount: 0, headline: '没有 IP 通过 TCP 稳定性复测',
    detail: '1 个累计两次连接失败而提前结束 · 1 个延迟超过上限',
  })
})

test('cancelled round counts observations without treating untested placeholders as failed', () => {
  const noAttempt = candidate({ exclusionCode: 'cancelled' })
  const stoppedAfterProbe = candidate({ exclusionCode: 'cancelled', tcpMs: 30, tcpStability: stability([30]) })
  const initialTimeout = candidate({ exclusionCode: 'tcpConnect' })
  const summary = summarizeScanResults(run([noAttempt, stoppedAfterProbe, initialTimeout], 'cancelled'), 0)

  assert.equal(summary.totalCandidates, 3)
  assert.equal(summary.checkedCandidates, 2)
  assert.equal(summary.initialTcpFailures, 1)
  assert.equal(summary.precisionCount, 1)
  assert.equal(summary.stabilityFailures, 0)
  assert.equal(summary.headline, '已保留停止前取得的结果')
})

test('zero completed downloads preserves evidence of passing TCP and the downstream failure', () => {
  const verified = candidate({ tcpStability: stability(Array(10).fill(50)), exclusionCode: 'downloadFailure' })
  const summary = summarizeScanResults(run([verified]), 0)
  assert.equal(summary.stabilityPassed, 1)
  assert.equal(summary.headline, '没有 IP 完成下载测速')
  assert.match(summary.detail, /下载请求失败/)
})

test('failure categories distinguish old latency stops and completed connection rejections', () => {
  const historicalConfig = { ...config, stabilityFirst: false }
  const legacyLatency = candidate({
    exclusionCode: 'stabilityEarly',
    tcpStability: stability([1000], { earlyRejection: 'TCP 稳定性提前淘汰：已出现超过 230 ms 的样本，最终最大延迟无法达标' }),
  })
  const completeWithTwoFailures = candidate({ tcpStability: stability([...Array(8).fill(50), null, null]), exclusionCode: 'stabilityEarly' })

  assert.equal(stabilityFailureKind(legacyLatency, historicalConfig), 'latency')
  assert.equal(stabilityFailureKind(completeWithTwoFailures, config), 'connection-limit')
})
