import type { IpResult, NetworkPathSnapshot, ScanConfig, ScanRun } from './types'
import { completedTcpMetricValue, hasCompleteDownload, stabilityMetricLabel } from './ui-state'

export interface FavoriteFolder {
  id: string
  name: string
  createdAt: string
}

export interface FavoriteEntry {
  id: string
  folderId: string
  savedAt: string
  note: string
  runId: string
  runStatus: ScanRun['status']
  runStartedAt: string
  runFinishedAt?: string | null
  config: ScanConfig
  result: IpResult
  networkPath?: NetworkPathSnapshot | null
  /** This snapshot was produced by explicitly testing saved IPs. */
  targeted?: boolean
  downloadTest?: {
    runId: string
    testedAt: string
    config: ScanConfig
    result: IpResult
  }
}

export interface FavoriteLibrary {
  /** Monotonic native-store version; missing only in legacy/preview snapshots. */
  revision?: number
  folders: FavoriteFolder[]
  entries: FavoriteEntry[]
}

/** Late events and command replies must not roll back a newer saved library. */
export function acceptsFavoriteLibrary(current: FavoriteLibrary, incoming: FavoriteLibrary): boolean {
  const revision = (library: FavoriteLibrary) => {
    const value = library.revision === undefined ? 0 : library.revision
    return Number.isSafeInteger(value) && value >= 0 ? value : null
  }
  const next = revision(incoming)
  return next !== null && next >= (revision(current) ?? 0)
}

export interface FavoriteTestJob {
  run: ScanRun
  favoriteIds: string[]
  baselines?: FavoriteTestBaseline[]
}

export interface FavoriteTestBaseline {
  favoriteId: string
  testedAt: string
  config: ScanConfig
  networkPath?: NetworkPathSnapshot | null
  colo: string
  measurementPassed: boolean
  tcpP95Ms: number | null
  downloadMbps: number | null
}

const comparisonFields: (keyof ScanConfig)[] = ['targetUrl', 'connectTimeoutMs', 'requestTimeoutMs', 'tcpConcurrency', 'stabilityAttempts', 'stabilityIntervalMs', 'stabilityLatencyMetric', 'maxTcpMs', 'maxStabilityFailurePercent', 'tcpLatencyScreening', 'stabilityFirst', 'verificationAttempts', 'downloadRounds', 'downloadSeconds', 'downloadBudgetEnabled', 'downloadBudgetMiB']
const downloadComparisonFields: (keyof ScanConfig)[] = ['targetUrl', 'connectTimeoutMs', 'requestTimeoutMs', 'downloadRounds', 'downloadSeconds', 'downloadBudgetEnabled', 'downloadBudgetMiB']
export const indirectRouteWarning = '部分接口来自其他 IP 的采样'

export function comparisonContextDifferences(before: { config: ScanConfig; networkPath?: NetworkPathSnapshot | null; colo: string }, after: { config: ScanConfig; networkPath?: NetworkPathSnapshot | null; colo: string }, ip?: string, downloadOnly = false): string[] {
  const differences: string[] = []
  const fields = downloadOnly ? downloadComparisonFields : comparisonFields
  if (fields.some(field => !(field === 'downloadBudgetMiB' && before.config.downloadBudgetEnabled === false && after.config.downloadBudgetEnabled === false) && before.config[field] !== after.config[field])) differences.push('测速参数不同')
  const previousPath = before.networkPath
  const currentPath = after.networkPath
  if (!previousPath || !currentPath) differences.push('网络路径未完整记录')
  else if (ip && (previousPath.sampleIp !== ip || currentPath.sampleIp !== ip)) differences.push(indirectRouteWarning)
  else if (previousPath.interfaceName !== currentPath.interfaceName || previousPath.tunnelInterface !== currentPath.tunnelInterface
    || (previousPath.interfaceKind && currentPath.interfaceKind && previousPath.interfaceKind !== currentPath.interfaceKind)) differences.push('网络接口不同')
  if (before.colo && after.colo && before.colo !== after.colo) differences.push('接入机房不同')
  return differences
}

export function favoriteTestChange(entry: FavoriteEntry, job?: FavoriteTestJob | null) {
  if (!job || job.run.status === 'running' || !job.favoriteIds.includes(entry.id)) return null
  return compareFavoriteEntry(entry, job, job.baselines?.find(item => item.favoriteId === entry.id))
}

export function favoriteTestChanges(entries: FavoriteEntry[], job?: FavoriteTestJob | null) {
  const changes = new Map<string, ReturnType<typeof compareFavoriteEntry>>()
  if (!job || job.run.status === 'running') return changes
  const targets = new Set(job.favoriteIds)
  const baselines = new Map(job.baselines?.map(baseline => [baseline.favoriteId, baseline]))
  for (const entry of entries) {
    if (targets.has(entry.id)) changes.set(entry.id, compareFavoriteEntry(entry, job, baselines.get(entry.id)))
  }
  return changes
}

function compareFavoriteEntry(entry: FavoriteEntry, job: FavoriteTestJob, baseline?: FavoriteTestBaseline) {
  const download = job.run.downloadOnly ? entry.downloadTest : undefined
  const runId = job.run.downloadOnly ? download?.runId : entry.runId
  // An explicit save after this job may replace the same favorite with another
  // snapshot. Do not describe that newer snapshot as an untouched retest target.
  if (runId !== job.run.id && Date.parse(entry.savedAt) > Date.parse(job.run.startedAt)) return null
  if (runId !== job.run.id) return { label: '本轮未更新', detail: '保留上次测试记录', changed: false, differences: [] as string[] }
  if (!baseline) return { label: '本轮已更新', detail: '可在测速历史查看已保留的记录', changed: true, differences: [] as string[] }
  const { config, result } = download ?? entry
  const networkPath = download ? result.networkPath : entry.networkPath
  const differences = comparisonContextDifferences(baseline, { config, networkPath, colo: result.colo }, result.ip, !!download)
  const parts: string[] = []
  if (!download && baseline.measurementPassed !== result.measurementPassed) parts.push(result.measurementPassed ? '本次完整通过' : '本次未通过完整测速')
  const p95 = download ? null : completedTcpMetricValue(result, 'p95')
  const speed = hasCompleteDownload(result, config) ? result.downloadMbps : null
  const metrics = [['P95', baseline.tcpP95Ms, p95, 'ms'], ['下载', baseline.downloadMbps, speed, 'Mbps']] as const
  const compared = download ? metrics.slice(1) : metrics
  const comparable = compared.some(([, before, after]) => before != null && after != null)
  for (const [label, before, after, unit] of compared) {
    if (before != null && after != null && Math.abs(after - before) >= 0.1) parts.push(`${label} ${after > before ? '+' : '−'}${Math.abs(after - before).toFixed(1)} ${unit}`)
    else if (before != null && after == null) parts.push(`${label}本次未测满`)
    else if (before == null && after != null) parts.push(`${label}本次已测满`)
  }
  return { label: parts.join(' · ') || (comparable ? '未见明显数值变化' : '暂无可比较的完整测量'), detail: `对比 ${favoriteDate(baseline.testedAt)}；仅描述两次观测，不代表长期趋势。`, changed: parts.length > 0 || differences.some(difference => difference !== '网络路径未完整记录' && difference !== indirectRouteWarning), differences }
}

export function favoriteTestDate(entry: FavoriteEntry): string {
  if (!entry.downloadTest && !favoriteHasTestData(entry)) return '尚未测速'
  const value = entry.downloadTest?.testedAt ?? (entry.runFinishedAt || entry.runStartedAt)
  const date = new Date(value)
  return Number.isNaN(date.getTime()) ? '时间未记录' : date.toLocaleString('zh-CN', { year: 'numeric', month: 'numeric', day: 'numeric', hour: '2-digit', minute: '2-digit', hour12: false })
}

export interface FavoriteTestStart {
  favorites: FavoriteLibrary
  job: FavoriteTestJob
}

export function favoriteHasTestData(entry: FavoriteEntry): boolean {
  const result = entry.result
  return !!(result.tcpMs || result.colo || result.attempts || result.tcpStability?.probesMs.length || result.downloadSamplesMbps.length
    || (result.exclusionCode ? result.exclusionCode !== 'cancelled' : result.exclusionReason && result.exclusionReason !== '测试已停止，验证或下载复测未完成'))
}

export function favoriteTestPhase(job: FavoriteTestJob): string {
  if (job.run.status !== 'running') return favoriteRunStatus(job.run.status)
  if (job.run.downloadOnly) return '仅测下载'
  return ({ connecting: 'TCP 连通性检查', discovering: '识别地区与 POP', stabilizing: 'TCP 稳定性测试', verifying: 'HTTPS 服务校验', downloading: '下载测速', completed: '测试已完成', cancelled: '测试已停止', failed: '测试失败' })[job.run.progress.stage]
}

export function favoriteTestPassedCount(run: ScanRun): number {
  return run.results.filter(result => run.downloadOnly ? hasCompleteDownload(result, run.config) : result.measurementPassed).length
}

/** The native store supplies stable folder IDs; these are browser-preview placeholders. */
export function defaultFavoriteLibrary(): FavoriteLibrary {
  return {
    revision: 0,
    folders: ['联通流量', '联通宽带', '移动流量'].map((name, index) => ({ id: `preview-folder-${index + 1}`, name, createdAt: '' })),
    entries: [],
  }
}

export function isIpSaved(library: FavoriteLibrary, ip: string): boolean {
  return library.entries.some(entry => entry.result.ip === ip)
}

/** Reconstruct enough of the original run for the existing evidence/status helpers. */
export function favoriteSnapshotRun(entry: FavoriteEntry): ScanRun {
  return {
    id: entry.runId,
    status: entry.runStatus,
    startedAt: entry.runStartedAt,
    finishedAt: entry.runFinishedAt ?? undefined,
    config: entry.config,
    results: [entry.result],
    progress: {
      stage: entry.runStatus === 'running' ? 'completed' : entry.runStatus,
      completed: 0,
      total: 0,
      downloadedBytes: 0,
      discoveredByRegion: {},
      message: '',
      networkPath: entry.networkPath ?? undefined,
    },
  }
}

export function favoriteDate(value?: string | null): string {
  if (!value) return '—'
  const date = new Date(value)
  return Number.isNaN(date.getTime()) ? value : date.toLocaleString('zh-CN', { hour12: false })
}

export function favoriteRunStatus(status: ScanRun['status']): string {
  return ({ running: '测试进行中', completed: '测试已完成', cancelled: '测试已停止', failed: '测试失败' })[status]
}

export function favoriteNumber(value?: number | null): string {
  return typeof value === 'number' && Number.isFinite(value) ? value.toFixed(1) : '—'
}

export function favoriteTcpSummary(result: IpResult, config: ScanConfig): { label: string; value: number | null } {
  const completed = completedTcpMetricValue(result, config.stabilityLatencyMetric)
  if (completed !== null) return { label: stabilityMetricLabel(config.stabilityLatencyMetric), value: completed }
  const peak = result.tcpStability?.maxMs
  if (typeof peak === 'number' && Number.isFinite(peak) && peak >= 0) return { label: '样本峰值', value: peak }
  const connected = result.tcpMs > 0 || result.successCount > 0 || !!result.colo
  return { label: '初筛', value: connected ? result.tcpMs : null }
}

export function favoriteError(error: unknown): string {
  return error instanceof Error ? error.message : String(error)
}
