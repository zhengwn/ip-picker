import type { IpResult, NetworkPathSnapshot, ScanConfig, ScanRun } from './types'
import { completedTcpMetricValue, stabilityMetricLabel } from './ui-state'

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
}

export interface FavoriteTestStart {
  favorites: FavoriteLibrary
  job: FavoriteTestJob
}

/** Match the native IPv4 parser: one dotted-decimal address without leading zeroes. */
export function validManualIpv4(value: string): boolean {
  const parts = value.trim().split('.')
  return parts.length === 4 && parts.every(part => /^(0|[1-9]\d{0,2})$/.test(part) && Number(part) <= 255)
}

export function favoriteHasTestData(entry: FavoriteEntry): boolean {
  const result = entry.result
  return !!(result.tcpMs || result.colo || result.attempts || result.tcpStability?.probesMs.length || result.downloadSamplesMbps.length
    || result.exclusionReason && result.exclusionReason !== '测试已停止，验证或下载复测未完成')
}

export function favoriteTestPhase(job: FavoriteTestJob): string {
  if (job.run.status !== 'running') return favoriteRunStatus(job.run.status)
  return ({ connecting: 'TCP 连通性检查', discovering: '识别地区与 POP', stabilizing: 'TCP 稳定性测试', verifying: 'HTTPS 服务校验', downloading: '下载测速', completed: '测试已完成', cancelled: '测试已停止', failed: '测试失败' })[job.run.progress.stage]
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
  const connected = result.tcpMs > 0 || result.successCount > 0 || !!result.colo
  return { label: '初筛', value: connected ? result.tcpMs : null }
}

export function favoriteProbes(probes: (number | null)[]): string {
  return probes.map(sample => sample === null ? '失败' : favoriteNumber(sample)).join(' / ')
}

export function favoriteError(error: unknown): string {
  return error instanceof Error ? error.message : String(error)
}
