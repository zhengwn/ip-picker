import type { TcpDisplayMetric } from './result-preferences'
import { stabilityFailureKind } from './scan-result-summary'
import type { IpResult, ScanConfig, ScanRun } from './types'
import { candidateMeasurementStatus, completedTcpMetricValue, initialTcpFailureReason, observedTcpMs, stabilityMetricLabel } from './ui-state'

const stabilityLabels = {
  'connection-stop': '连接失败提前结束',
  'connection-limit': '连接失败率超限',
  latency: '延迟超限',
  other: '稳定性未达标',
} as const

function rowOutcome(result: IpResult, run: ScanRun, status: string) {
  if (status === 'qualified') return { label: '测速通过', tone: 'good' } as const
  if (status === 'measured') return { label: '测速完成 · 未达标', tone: 'warning' } as const
  if (status === 'pendingSummary') return { label: '测速完成 · 待汇总', tone: 'quiet' } as const

  const initialFailure = initialTcpFailureReason(result)
  if (initialFailure) {
    const kind = result.tcpConnectFailure?.kind
    const label = kind === 'timeout' || /^TCP 连接超时/.test(initialFailure) ? '初筛超时'
      : kind === 'worker' || /^TCP 探测任务失败/.test(initialFailure) ? '初筛任务异常' : '初筛连接失败'
    return { label, tone: 'warning', blockedDownload: true } as const
  }
  if (result.exclusionCode === 'tcpLatency' || (!result.exclusionCode && /^TCP 延迟 .*超过上限/.test(result.exclusionReason ?? ''))) {
    return { label: '初筛延迟超限', tone: 'warning', blockedDownload: true } as const
  }
  const stabilityFailure = stabilityFailureKind(result, run.config)
  if (stabilityFailure) return { label: stabilityLabels[stabilityFailure], tone: 'warning', blockedDownload: true } as const
  if (status === 'notDownloaded') return { label: '未入选下载', tone: 'quiet', blockedDownload: true } as const
  return { label: candidateMeasurementStatus(result, run), tone: 'quiet' } as const
}

function observedPeak(result: IpResult): number | null {
  const peak = result.tcpStability?.maxMs
  return typeof peak === 'number' && Number.isFinite(peak) && peak >= 0 ? peak : null
}

function rowTcp(result: IpResult, metric: ScanConfig['stabilityLatencyMetric'], completed: number | null, initial: number | null) {
  if (completed != null) {
    const label = stabilityMetricLabel(metric)
    const title = label === 'P95' ? '至少 95% 的成功探测延迟不超过此值' : `完成全部稳定性探测后，成功样本的${label}延迟`
    return { label, value: completed, title }
  }
  const peak = observedPeak(result)
  if (peak != null) {
    return { label: '样本峰值', value: peak, title: '已取得的成功探测中，延迟最高的一次' }
  }
  return initial != null ? { label: '初筛', value: initial, title: '初筛 TCP 连接成功时的耗时' } : null
}

export function describeResultRow(result: IpResult, run: ScanRun, completedTcp: number | null, initialTcp: number | null, status: string) {
  const outcome = rowOutcome(result, run, status)
  return {
    outcome,
    tcp: rowTcp(result, run.config.stabilityLatencyMetric ?? 'max', completedTcp, initialTcp),
    missingDownload: 'blockedDownload' in outcome ? '未测速' : '未测得',
  }
}

export function describeTcpCell(
  result: IpResult,
  selectedMetric: TcpDisplayMetric,
  configuredMetric: ScanConfig['stabilityLatencyMetric'] = 'p95',
  completedValue: number | null = completedTcpMetricValue(result, configuredMetric),
  labelRunMetric = false,
): { value: number | null; label: string | null; tooltip: string } {
  const observed = {
    median: observedTcpMs(result.tcpStability, 'medianMs'),
    average: observedTcpMs(result.tcpStability, 'averageMs'),
    peak: observedPeak(result),
  }
  const p95 = completedValue == null || configuredMetric === 'p95' ? completedValue : completedTcpMetricValue(result, 'p95')
  const stats: [string, number | null][] = [
    ['P95', p95],
    ['中位', observed.median],
    ['平均', observed.average],
    ['峰值', observed.peak],
  ]
  const tooltip = stats.flatMap(([name, value]) => value == null ? [] : [`${name} ${value.toFixed(1)} ms`]).join(' · ')
  if (selectedMetric !== 'runMetric') return { value: observed[selectedMetric], label: null, tooltip: tooltip || '尚未取得该项数据' }
  const { tcpMs, tcpConnectFailure, attempts, colo } = result
  const initial = !tcpConnectFailure && Number.isFinite(tcpMs) && tcpMs >= 0 && (tcpMs > 0 || attempts > 0 || colo) ? tcpMs : null
  const recorded = rowTcp(result, configuredMetric, completedValue, initial)
  const label = recorded?.label === '样本峰值' ? '峰值' : recorded?.label
  return {
    value: recorded?.value ?? null,
    label: completedValue == null || labelRunMetric ? label ?? null : null,
    tooltip: tooltip || recorded?.title || '尚未取得 TCP 延迟数据',
  }
}

export function tcpLatencyChartScale(results: readonly IpResult[]): number {
  let peak = 0
  for (const result of results) {
    for (const sample of result.tcpStability?.probesMs ?? []) {
      if (sample != null && Number.isFinite(sample) && sample > peak) peak = sample
    }
  }
  return Math.max(50, Math.ceil(peak / 50) * 50)
}
