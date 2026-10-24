export type ResultSortBy = 'tcp' | 'download' | 'initialTcp' | 'ip'

export interface ResultPreferences {
  completedDownloadOnly: boolean
  sortBy: ResultSortBy
}

const STORAGE_KEY = 'ip-picker.result-preferences'

export function defaultResultPreferences(): ResultPreferences {
  return { completedDownloadOnly: true, sortBy: 'tcp' }
}

function isResultSortBy(value: unknown): value is ResultSortBy {
  return value === 'tcp' || value === 'download' || value === 'initialTcp' || value === 'ip'
}

function normalizeResultPreferences(value: unknown): ResultPreferences {
  const defaults = defaultResultPreferences()
  if (value === null || typeof value !== 'object' || Array.isArray(value)) return defaults
  const saved = value as Record<string, unknown>
  return {
    completedDownloadOnly: typeof saved.completedDownloadOnly === 'boolean' ? saved.completedDownloadOnly : defaults.completedDownloadOnly,
    sortBy: isResultSortBy(saved.sortBy) ? saved.sortBy : defaults.sortBy,
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
