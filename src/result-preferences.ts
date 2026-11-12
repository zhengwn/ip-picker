const RESULT_SORT_VALUES = ['tcp', 'medianTcp', 'averageTcp', 'tcpFailure', 'download', 'initialTcp', 'ip'] as const
const TCP_DISPLAY_METRICS = ['runMetric', 'median', 'average', 'peak'] as const

export type ResultSortBy = typeof RESULT_SORT_VALUES[number]
export type TcpDisplayMetric = typeof TCP_DISPLAY_METRICS[number]

export interface ResultPreferences {
  completedDownloadOnly: boolean
  sortBy: ResultSortBy
  tcpDisplayMetric: TcpDisplayMetric
}

const STORAGE_KEY = 'ip-picker.result-preferences'

export function defaultResultPreferences(): ResultPreferences {
  return { completedDownloadOnly: true, sortBy: 'tcp', tcpDisplayMetric: 'runMetric' }
}

function isResultSortBy(value: unknown): value is ResultSortBy {
  return RESULT_SORT_VALUES.some(option => option === value)
}

function isTcpDisplayMetric(value: unknown): value is TcpDisplayMetric {
  return TCP_DISPLAY_METRICS.some(option => option === value)
}

function normalizeResultPreferences(value: unknown): ResultPreferences {
  const defaults = defaultResultPreferences()
  if (value === null || typeof value !== 'object' || Array.isArray(value)) return defaults
  const saved = value as Record<string, unknown>
  const sortBy = saved.sortBy === 'tcpJitter' ? 'averageTcp' : saved.sortBy
  const tcpDisplayMetric = saved.tcpDisplayMetric === 'jitter' ? 'average' : saved.tcpDisplayMetric
  return {
    completedDownloadOnly: typeof saved.completedDownloadOnly === 'boolean' ? saved.completedDownloadOnly : defaults.completedDownloadOnly,
    sortBy: isResultSortBy(sortBy) ? sortBy : defaults.sortBy,
    tcpDisplayMetric: isTcpDisplayMetric(tcpDisplayMetric) ? tcpDisplayMetric : defaults.tcpDisplayMetric,
  }
}

export function readResultPreferences(): ResultPreferences {
  try {
    const saved = window.localStorage.getItem(STORAGE_KEY)
    return saved === null ? defaultResultPreferences() : normalizeResultPreferences(JSON.parse(saved))
  } catch {
    return defaultResultPreferences()
  }
}

export function saveResultPreferences(preferences: ResultPreferences): boolean {
  try {
    window.localStorage.setItem(STORAGE_KEY, JSON.stringify(normalizeResultPreferences(preferences)))
    return true
  } catch {
    return false
  }
}
