import type { IpResult, ScanConfig, ScanRun } from './types'
import { completedTcpMetricValue, hasPrecisionMeasurement, initialTcpFailureReason, stabilityVerdict, summarizeExclusions } from './ui-state'

type StabilityFailureKind = 'connection-stop' | 'connection-limit' | 'latency' | 'other'

export function stabilityFailureKind(result: IpResult, config: ScanConfig): StabilityFailureKind | null {
  const { exclusionCode, exclusionReason = '', tcpStability } = result
  const reason = tcpStability?.earlyRejection || exclusionReason
  const stoppedAtTwoFailures = config.stabilityFirst && (tcpStability?.failed ?? 0) >= 2
    || /累计\s*(?:2|两)\s*次连接失败|累计\s*\d+\s*次连接失败，达到\s*2\s*次/.test(reason)
  if (exclusionCode === 'stabilityLatency') return 'latency'
  if (exclusionCode === 'stabilityFailureRate' || exclusionCode === 'stabilityAllFailed') return 'connection-limit'
  if (stoppedAtTwoFailures || (exclusionCode === 'stabilityEarly' && config.stabilityFirst)) {
    return tcpStability?.completed ? 'connection-limit' : 'connection-stop'
  }
  if ((!exclusionCode || exclusionCode === 'stabilityEarly') && /^TCP\s*稳定性/.test(reason)) {
    if (/连接失败|失败率/.test(reason) && /提前淘汰|全部连接失败|超(?:出|过)|高于/.test(reason)) return 'connection-limit'
    if (/延迟|耗时|P95|样本超过|超过.+样本/.test(reason) && /提前淘汰|超(?:出|过)|高于/.test(reason)) return 'latency'
  }
  if (exclusionCode === 'stabilityEarly') return 'other'
  if (stabilityVerdict(result, config) !== 'failed') return null
  const latency = completedTcpMetricValue(result, config.stabilityLatencyMetric ?? 'max')
  if (latency != null && config.maxTcpMs != null && latency > config.maxTcpMs) return 'latency'
  return tcpStability?.failed ? 'connection-limit' : 'other'
}

const number = new Intl.NumberFormat('zh-CN')
const stabilityFailureDescriptions: Record<StabilityFailureKind, string> = {
  'connection-stop': '累计两次连接失败而提前结束',
  'connection-limit': '因连接失败未达标',
  latency: '延迟超过上限',
  other: '稳定性未达标',
}

export function summarizeScanResults(run: ScanRun, downloadedCount: number) {
  const totalCandidates = run.results.length
  let checkedCandidates = 0
  let initialTcpFailures = 0
  let precisionCount = 0
  let stabilityPassed = 0
  let anyDownloadSample = false
  const otherPrecisionResults: IpResult[] = []
  const failures: Record<StabilityFailureKind, number> = { 'connection-stop': 0, 'connection-limit': 0, latency: 0, other: 0 }
  for (const result of run.results) {
    const initialFailure = Boolean(initialTcpFailureReason(result))
    const precision = hasPrecisionMeasurement(result)
    if (initialFailure) initialTcpFailures += 1
    if (initialFailure || result.tcpMs > 0 || result.attempts > 0 || result.colo || precision || result.exclusionCode === 'tcpLatency') checkedCandidates += 1
    if (!precision) continue
    precisionCount += 1
    anyDownloadSample ||= result.downloadSamplesMbps.length > 0
    const failure = stabilityFailureKind(result, run.config)
    if (failure) {
      failures[failure] += 1
    } else {
      otherPrecisionResults.push(result)
      if (stabilityVerdict(result, run.config) === 'passed') stabilityPassed += 1
    }
  }
  if (run.status === 'completed' || (run.status === 'running' && run.progress.stage !== 'connecting')) {
    checkedCandidates = totalCandidates
  } else if (run.status === 'running') {
    checkedCandidates = Math.min(totalCandidates, Math.max(checkedCandidates, run.progress.completed))
  }

  const stabilityFailures = precisionCount - otherPrecisionResults.length
  const counts = { totalCandidates, checkedCandidates, initialTcpFailures, precisionCount, stabilityPassed, stabilityFailures, downloadedCount }
  if (run.status === 'running') return { ...counts, headline: '', detail: '' }
  if (run.status === 'cancelled') return { ...counts, headline: checkedCandidates ? '已保留停止前取得的结果' : '停止时尚未取得测量结果', detail: '' }
  if (run.status === 'failed') return { ...counts, headline: checkedCandidates ? '已保留中断前取得的结果' : '本轮未取得测量结果', detail: run.error ?? '' }
  if (downloadedCount) return { ...counts, headline: `${number.format(downloadedCount)} 个 IP 完成下载测速`, detail: '' }
  if (!totalCandidates) return { ...counts, headline: '本轮没有候选 IP', detail: '' }
  if (initialTcpFailures === totalCandidates) return { ...counts, headline: '没有 IP 通过 TCP 初筛', detail: '' }
  if (!precisionCount) {
    const recognized = run.config.regions.some(code => (run.progress.discoveredByRegion[code] ?? 0) > 0)
      || run.results.some(result => result.colo && (run.retestIp || run.config.regions.includes(result.regionCode)))
    return { ...counts, headline: '没有 IP 开始精测', detail: recognized ? '' : run.retestIp ? '没有识别到该 IP 的接入点' : '没有识别到所选地区的接入点' }
  }

  const stabilityDetails = (Object.keys(failures) as StabilityFailureKind[])
    .filter(kind => failures[kind])
    .map(kind => `${number.format(failures[kind])} 个${stabilityFailureDescriptions[kind]}`)
  if (stabilityFailures === precisionCount && !run.progress.downloadedBytes && !anyDownloadSample) {
    return { ...counts, headline: '没有 IP 通过 TCP 稳定性复测', detail: stabilityDetails.join(' · ') }
  }
  const downstreamDetails = summarizeExclusions(otherPrecisionResults.filter(result => result.exclusionCode || result.exclusionReason), run.status)
    .slice(0, 2)
    .map(category => `${number.format(category.count)} 个${category.label}`)
  return { ...counts, headline: '没有 IP 完成下载测速', detail: [...downstreamDetails, ...stabilityDetails].slice(0, 2).join(' · ') }
}
