const KEY = 'ip-picker.last-favorite-folder'

export function preferredFavoriteFolder(folders: { id: string }[]): string {
  try {
    const saved = window.localStorage.getItem(KEY)
    if (folders.some(folder => folder.id === saved)) return saved!
  } catch { /* Preferences are optional when storage is unavailable. */ }
  return folders[0]?.id ?? ''
}

export function rememberFavoriteFolder(id: string) {
  try { window.localStorage.setItem(KEY, id) } catch { /* The saved favorite remains valid. */ }
}
