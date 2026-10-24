import type { FavoriteLibrary, FavoriteTestJob } from './favorites'

export interface ScanConfig {
  regions: string[]
  ipsPerRegion: number
  preference: 'daily' | 'download'
  minDownloadMbps: number
  maxTcpMs: number | null
  tcpLatencyScreening: boolean
  /** Complete stability for every admitted candidate before choosing download candidates. */
  stabilityFirst: boolean
  stabilityAttempts: number | null
  stabilityIntervalMs: number
  maxStabilityFailurePercent: number
  stabilityLatencyMetric: 'average' | 'p95' | 'max'
  maxCandidates: number
  tcpConcurrency: number
  connectTimeoutMs: number
  requestTimeoutMs: number
  precisionMode: 'adaptive' | 'limited'
  /** Null belongs to historical runs without the regional admission cap. */
  regionalCandidateLimit: number | null
  candidatePoolSize: number | null
  candidatesPerRegion: number
  verificationAttempts: number
  downloadRounds: number
  downloadSeconds: number
  downloadBudgetEnabled: boolean
  downloadBudgetMiB: number
  source: 'cloudflare' | 'custom'
  customIps: string
  targetUrl: string
}

export type ScanStage = 'connecting' | 'discovering' | 'verifying' | 'stabilizing' | 'downloading' | 'completed' | 'cancelled' | 'failed'

export interface NetworkPathSnapshot {
  sampleIp: string
  interfaceName: string
  tunnelInterface: boolean
  capturedAt: string
}

export interface ScanProgress {
  stage: ScanStage
  completed: number
  total: number
  downloadedBytes: number
  discoveredByRegion: Record<string, number>
  message: string
  networkPath?: NetworkPathSnapshot
  stabilityBatch?: { number: number; candidateIps: string[] }
  precisionBatch?: { number: number; candidateIps: string[] }
}

export interface IpResult {
  ip: string
  regionCode: string
  city: string
  colo: string
  observedColos: string[]
  tcpMs: number
  httpsMs: number
  jitterMs: number
  downloadMbps: number
  successCount: number
  attempts: number
  downloadSamplesMbps: number[]
  measurementPassed: boolean
  tcpStability?: TcpStability
  eligible: boolean
  exclusionReason?: string
  role?: 'primary' | 'backup' | 'candidate'
}

export interface TcpStability {
  requested: number
  attempted: number
  succeeded: number
  failed: number
  overLimitCount: number
  delayLimitMs: number
  failurePercent: number | null
  averageMs: number | null
  medianMs: number | null
  minMs: number | null
  maxMs: number | null
  p95Ms: number | null
  jitterMs: number | null
  probesMs: (number | null)[]
  completed: boolean
  /** A proven rejection can settle this candidate before all probes are sent. */
  earlyRejection?: string | null
}

export interface ScanRun {
  id: string
  status: 'running' | 'completed' | 'cancelled' | 'failed'
  startedAt: string
  finishedAt?: string
  config: ScanConfig
  progress: ScanProgress
  results: IpResult[]
  error?: string
}

export interface SavedState { config: ScanConfig; lastRun?: ScanRun; warning?: string; favorites?: FavoriteLibrary; favoriteTest?: FavoriteTestJob | null }

export const regions = [
  { code: 'HK', name: '香港', popular: true },
  { code: 'JP', name: '日本', popular: true },
  { code: 'SG', name: '新加坡', popular: true },
  { code: 'US', name: '美国', popular: true },
  { code: 'DE', name: '德国', popular: true },
  { code: 'AU', name: '澳大利亚', popular: true },
  { code: 'KR', name: '韩国', popular: false },
  { code: 'TW', name: '台湾', popular: false },
  { code: 'GB', name: '英国', popular: false },
  { code: 'CA', name: '加拿大', popular: false },
  { code: 'NL', name: '荷兰', popular: false },
  { code: 'FR', name: '法国', popular: false },
  { code: 'IN', name: '印度', popular: false },
] as const

/** Targets offered for new scans; the full catalog still names historical results. */
export const selectableRegions = regions.slice(0, 4)

export function regionName(code: string) {
  return regions.find(region => region.code === code)?.name ?? code
}

export function defaultConfig(): ScanConfig {
  return {
    regions: selectableRegions.map(region => region.code), ipsPerRegion: 1, preference: 'daily',
    minDownloadMbps: 0, maxTcpMs: 230, tcpLatencyScreening: false, stabilityFirst: true, stabilityAttempts: 100, stabilityIntervalMs: 200,
    maxStabilityFailurePercent: 1, stabilityLatencyMetric: 'p95', maxCandidates: 5956, tcpConcurrency: 64,
    connectTimeoutMs: 1500, requestTimeoutMs: 5000, precisionMode: 'adaptive', regionalCandidateLimit: 20, candidatePoolSize: 20, candidatesPerRegion: 10,
    verificationAttempts: 5, downloadRounds: 2, downloadSeconds: 3,
    downloadBudgetEnabled: false, downloadBudgetMiB: 400, source: 'cloudflare', customIps: '',
    targetUrl: 'https://speed.cloudflare.com/__down?bytes=25000000',
  }
}
