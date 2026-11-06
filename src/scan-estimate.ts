import type { ScanConfig } from './types'
import { regionalPrecisionLimit } from './ui-state'
import { inspectCustomIps } from './custom-ips'

const MIB = 1024 ** 2
const REFERENCE_TCP_MS = 150
const REFERENCE_HTTPS_MS = 500

export function formatDownloadedMiB(bytes: number): string {
  return `${(bytes / MIB).toFixed(1)} MiB`
}

function candidateCapacity(config: ScanConfig): number {
  if (config.source === 'cloudflare') return config.maxCandidates
  // Distinct entries can overlap, so capacity is a workload reference.
  return Math.min(config.maxCandidates, inspectCustomIps(config.customIps).capacity ?? 0)
}

function cloudflareResponseBytes(target: string): number | null {
  const url = new URL(target)
  if (url.protocol !== 'https:' || url.hostname !== 'speed.cloudflare.com' || url.port || url.pathname !== '/__down') return null
  const [value, ...duplicates] = url.searchParams.getAll('bytes')
  if (!value || duplicates.length || !/^\d+$/.test(value)) return null
  const bytes = Number(value)
  return Number.isSafeInteger(bytes) && bytes > 0 ? bytes : null
}

function workloadMs(count: number, concurrent: number, perTaskMs: number): number {
  return Math.ceil(count / concurrent) * perTaskMs
}

function formatDuration(milliseconds: number): string {
  const seconds = Math.ceil(milliseconds / 1000)
  if (seconds < 60) return `约 ${seconds} 秒`
  const minutes = Math.ceil(seconds / 60)
  if (minutes < 60) return `约 ${minutes} 分钟`
  if (minutes < 1440) return `约 ${Math.floor(minutes / 60)} 小时 ${minutes % 60} 分`
  return `约 ${Math.ceil(minutes / 1440)} 天`
}

function formatEstimatedBytes(bytes: number): string {
  if (bytes >= 1024 * MIB) return `${(bytes / (1024 * MIB)).toFixed(1)} GiB`
  if (bytes >= MIB) return `${Math.ceil(bytes / MIB)} MiB`
  return bytes >= 1024 ? `${Math.ceil(bytes / 1024)} KiB` : `${bytes} B`
}

/** Reference workload for a validated regional scan configuration. */
export function estimateScan(config: ScanConfig): { duration: string; download: string; detail: string } {
  const candidates = candidateCapacity(config)
  if (!candidates) return { duration: '待确认', download: '待确认', detail: '自定义列表中没有可估算的候选地址。' }

  const regions = config.regions.length
  const regionLimit = regionalPrecisionLimit(config) ?? candidates
  const stabilityCandidates = Math.min(candidates, regions * regionLimit)
  // Match the regional shortlist in engine.rs.
  const downloadCandidates = Math.min(candidates, regions * Math.min(10, regionLimit))
  const rounds = downloadCandidates * config.downloadRounds
  const concurrency = config.tcpConcurrency
  const tcpMs = Math.min(config.connectTimeoutMs, REFERENCE_TCP_MS)
  const httpsMs = Math.min(config.requestTimeoutMs, REFERENCE_HTTPS_MS)
  const stabilityMs = ((config.stabilityAttempts ?? 1) - 1) * Math.max(config.stabilityIntervalMs, tcpMs) + tcpMs
  const durationMs = workloadMs(candidates, concurrency, tcpMs)
    + workloadMs(candidates, Math.min(concurrency, 24), httpsMs)
    + workloadMs(stabilityCandidates, Math.min(concurrency, 4), stabilityMs)
    + workloadMs(downloadCandidates, Math.min(concurrency, 16), (config.verificationAttempts - 1) * httpsMs)
    + rounds * (config.downloadSeconds * 1000 + httpsMs)

  const responseBytes = cloudflareResponseBytes(config.targetUrl)
  const fullDownload = responseBytes === null ? null : rounds * responseBytes
  const budget = config.downloadBudgetEnabled ? config.downloadBudgetMiB * MIB : null
  const download = budget !== null && (fullDownload === null || budget <= fullDownload)
    ? `最多 ${config.downloadBudgetMiB} MiB`
    : fullDownload === null ? '随响应变化' : `约 ${formatEstimatedBytes(fullDownload)}`
  const responseDetail = responseBytes === null && budget === null
    ? '自定义目标的响应大小未知。'
    : responseBytes === null ? '下载流量按预算上限显示。' : '下载流量按测速地址的请求字节数与计划轮数估算，预算开启时受预算限制。'
  return {
    duration: formatDuration(durationMs),
    download,
    detail: `按最多 ${candidates.toLocaleString('zh-CN')} 个候选、精测名单满额估算。建连按 ${tcpMs} ms、HTTPS 请求按 ${httpsMs} ms、下载正文按每轮设定时长估算。实际耗时受候选发现、网络等待及提前淘汰影响。${responseDetail}流量只计下载响应正文。`,
  }
}
