<script setup lang="ts">
import { computed, nextTick, onBeforeUnmount, onMounted, ref, watch } from 'vue'
import { version as appVersion } from '../package.json'
import { invoke, isTauri } from '@tauri-apps/api/core'
import { listen, type UnlistenFn } from '@tauri-apps/api/event'
import { getCurrentWindow, type CloseRequestedEvent } from '@tauri-apps/api/window'
import { Activity, ArrowDownToLine, ArrowRight, Check, ChevronDown, CircleAlert, Globe2, Info, LoaderCircle, Network, Play, RotateCcw, Settings2, ShieldCheck, Square, Star, X } from '@lucide/vue'
import { defaultConfig, selectableRegions, type IpResult, type SavedState, type ScanConfig, type ScanRun } from './types'
import { normalizeActiveConfig, precisionPolicyDescription, stabilityMetricLabel, stageProgressText, validateConfig } from './ui-state'
import SelectControl from './components/SelectControl.vue'
import { dismissPopovers, useDismissibleDetails } from './popovers'
import ScanProgressPanel from './components/ScanProgressPanel.vue'
import AllResultsPanel from './components/AllResultsPanel.vue'
import FavoriteSaveDialog from './components/FavoriteSaveDialog.vue'
import FavoritesPanel from './components/FavoritesPanel.vue'
import { acceptsFavoriteLibrary, defaultFavoriteLibrary, type FavoriteLibrary, type FavoriteTestJob, type FavoriteTestStart } from './favorites'

const desktop = isTauri()
const appWindow = desktop ? getCurrentWindow() : null
const appShell = ref<HTMLElement | null>(null)
useDismissibleDetails(appShell, '.scan-progress-disclosure, .favorite-job-details, .favorite-test-settings')
const config = ref<ScanConfig>(defaultConfig())
const run = ref<ScanRun | null>(null)
const favorites = ref<FavoriteLibrary>(defaultFavoriteLibrary())
const favoritesOpen = ref(false)
const favoriteTest = ref<FavoriteTestJob | null>(null)
const favoriteTesting = computed(() => favoriteTest.value?.run.status === 'running')
const favoritesPanel = ref<HTMLElement | null>(null)
const favoritesToggle = ref<HTMLButtonElement | null>(null)
const scanToggle = ref<HTMLButtonElement | null>(null)
const favoriteTarget = ref<{ runId: string; ip: string } | null>(null)
const favoriteResult = computed(() => run.value?.id === favoriteTarget.value?.runId
  ? run.value?.results.find(result => result.ip === favoriteTarget.value?.ip) ?? null : null)
const favoritePending = ref(false)
const savedIps = computed(() => new Set(favorites.value.entries.map(entry => entry.result.ip)))
let favoriteTrigger: HTMLElement | null = null
const initialized = ref(false)
const loading = ref(desktop)
const starting = ref(false)
const stopping = ref(false)
const advancedOpen = ref(false)
const finishingSettings = ref(false)
const windowClosing = ref(false)
const savingConfig = ref(false)
const configSaveError = ref('')
const settingsDialog = ref<HTMLDialogElement | null>(null)
const settingsTrigger = ref<HTMLButtonElement | null>(null)
const settingsBody = ref<HTMLElement | null>(null)
const settingsSections = [
  { id: 'target', label: '目标与来源', icon: Globe2 },
  { id: 'quality', label: '筛选与下载', icon: Settings2 },
  { id: 'stability', label: '稳定性', icon: ShieldCheck },
] as const
type SettingsSection = typeof settingsSections[number]['id']
const settingsSection = ref<SettingsSection>('target')
watch(settingsSection, () => { if (settingsBody.value) settingsBody.value.scrollTop = 0 }, { flush: 'post' })
function sectionForField(field: keyof ScanConfig): SettingsSection {
  if (['stabilityLatencyMetric', 'stabilityAttempts', 'stabilityIntervalMs', 'maxStabilityFailurePercent'].includes(field)) return 'stability'
  if (['maxTcpMs', 'maxCandidates', 'tcpConcurrency', 'connectTimeoutMs', 'requestTimeoutMs', 'regionalCandidateLimit', 'precisionMode', 'candidatesPerRegion', 'verificationAttempts', 'downloadRounds', 'downloadSeconds'].includes(field)) return 'quality'
  return 'target'
}
async function onSettingsTabKey(event: KeyboardEvent) {
  const index = settingsSections.findIndex(section => section.id === settingsSection.value)
  let next: number
  if (event.key === 'ArrowRight') next = (index + 1) % settingsSections.length
  else if (event.key === 'ArrowLeft') next = (index + settingsSections.length - 1) % settingsSections.length
  else if (event.key === 'Home') next = 0
  else if (event.key === 'End') next = settingsSections.length - 1
  else return
  event.preventDefault()
  settingsSection.value = settingsSections[next].id
  await nextTick()
  document.getElementById(`settings-tab-${settingsSection.value}`)?.focus()
}
const targetHost = computed(() => {
  try { return new URL(config.value.targetUrl).hostname } catch { return '请设置测速地址' }
})
const scanActionLabel = computed(() => loading.value ? '正在加载…' : starting.value ? '正在启动…' : run.value ? '重新测速' : '开始测速')
watch(advancedOpen, open => {
  if (open && !settingsDialog.value?.open) settingsDialog.value?.showModal()
  if (!open && settingsDialog.value?.open) {
    settingsDialog.value.close()
    settingsTrigger.value?.focus({ preventScroll: true })
  }
}, { flush: 'post' })
const toast = ref<{ text: string; error: boolean } | null>(null)
const pageError = ref('')
const storageWarning = ref('')
const fileInput = ref<HTMLInputElement | null>(null)
const progressPanel = ref<HTMLElement | null>(null)
const now = ref(Date.now())
let unlisten: UnlistenFn | undefined
let unlistenStorage: UnlistenFn | undefined
let unlistenFavoriteTest: UnlistenFn | undefined
let unlistenFavorites: UnlistenFn | undefined
let unlistenClose: UnlistenFn | undefined
let disposed = false
let clock: ReturnType<typeof setInterval> | undefined
let saveTimer: ReturnType<typeof setTimeout> | undefined
let toastTimer: ReturnType<typeof setTimeout> | undefined
let previousRunIdForStart: string | undefined
// Invalid drafts never replace the most recent valid settings awaiting a save.
let pendingConfig: ScanConfig | null = null
let configSavePromise: Promise<void> | undefined
let drainConfigSaves = false
let configSaveDue = false

const running = computed(() => run.value?.status === 'running')
const scanPhaseTitle = computed(() => run.value ? stageProgressText(run.value).title : '')
const locked = computed(() => loading.value || starting.value || running.value || favoriteTesting.value || stopping.value || finishingSettings.value || windowClosing.value)
const dirty = computed(() => !!run.value && JSON.stringify(config.value) !== JSON.stringify(run.value.config))
const validationIssue = computed(() => validateConfig(config.value))
const validation = computed(() => validationIssue.value?.message ?? '')
const elapsed = computed(() => {
  if (!run.value) return ''
  const end = run.value.finishedAt ? new Date(run.value.finishedAt).getTime() : now.value
  const seconds = Math.max(0, Math.floor((end - new Date(run.value.startedAt).getTime()) / 1000))
  return seconds >= 60 ? `${Math.floor(seconds / 60)} 分 ${seconds % 60} 秒` : `${seconds} 秒`
})
function scrollBehavior(): ScrollBehavior { return window.matchMedia('(prefers-reduced-motion: reduce)').matches ? 'auto' : 'smooth' }
function fieldId(field: keyof ScanConfig) { return field === 'regions' ? 'region-choices' : field === 'customIps' ? 'custom-ips' : `setting-${field}` }
function fieldInvalid(field: keyof ScanConfig) { return validationIssue.value?.field === field || undefined }
function fieldDescription(field: keyof ScanConfig, helpId?: string) { return [fieldInvalid(field) ? (advancedOpen.value ? 'settings-validation' : 'config-validation') : '', helpId ?? ''].filter(Boolean).join(' ') || undefined }
async function focusSetting(field: keyof ScanConfig) {
  if (!['regions', 'downloadBudgetEnabled', 'downloadBudgetMiB'].includes(field)) { settingsSection.value = sectionForField(field); advancedOpen.value = true }
  await nextTick()
  const target = document.getElementById(fieldId(field))
  if (!target) return
  target.scrollIntoView({ behavior: scrollBehavior(), block: 'center' })
  const focusTarget = field === 'regions' ? target.querySelector<HTMLElement>('button:not(:disabled)')
    : target.getAttribute('role') === 'radiogroup'
      ? target.querySelector<HTMLElement>('input:checked:not(:disabled)') ?? target.querySelector<HTMLElement>('input:not(:disabled)')
      : target
  focusTarget?.focus({ preventScroll: true })
}
function errorText(error: unknown) { return error instanceof Error ? error.message : String(error) }
function notify(text: string, error = false) {
  clearTimeout(toastTimer)
  toast.value = { text, error }
  toastTimer = setTimeout(() => { toast.value = null }, error ? 6500 : 4000)
}
function flushConfig(): Promise<void> {
  clearTimeout(saveTimer)
  return persistConfig(true)
}
function persistConfig(flush = false): Promise<void> {
  if (flush) drainConfigSaves = true
  else configSaveDue = true
  if (configSavePromise) return configSavePromise
  if (!desktop || !pendingConfig) {
    drainConfigSaves = false
    configSaveDue = false
    return Promise.resolve()
  }
  savingConfig.value = true
  configSavePromise = (async () => {
    while (pendingConfig && !disposed && (drainConfigSaves || configSaveDue)) {
      const snapshot: ScanConfig = pendingConfig
      configSaveDue = false
      await invoke('save_config', { config: snapshot })
      // Edits made while this write was in flight need their own subsequent save.
      if (pendingConfig === snapshot) pendingConfig = null
      configSaveError.value = ''
    }
  })().catch(error => {
    configSaveError.value = `设置保存失败：${errorText(error)}`
    if (!disposed) notify(configSaveError.value, true)
    throw error
  }).finally(() => {
    savingConfig.value = false
    configSavePromise = undefined
    drainConfigSaves = false
    configSaveDue = false
  })
  return configSavePromise
}
function closeSettings() {
  if (!finishingSettings.value && !windowClosing.value) advancedOpen.value = false
}
async function finishSettings() {
  if (finishingSettings.value || windowClosing.value || validation.value) return
  finishingSettings.value = true
  try {
    await flushConfig()
    advancedOpen.value = false
  } catch { /* Keep the drawer and pending settings available for a retry. */ }
  finally { finishingSettings.value = false }
}
async function requestWindowClose(event: CloseRequestedEvent) {
  event.preventDefault()
  if (windowClosing.value) return
  windowClosing.value = true
  try {
    await flushConfig()
    // destroy follows Tauri's normal close-handler path without requesting close again.
    await appWindow?.destroy()
  } catch (error) {
    if (!configSaveError.value) notify(`关闭窗口失败：${errorText(error)}`, true)
    windowClosing.value = false
  }
}
function toggleRegion(code: string) {
  if (locked.value) return
  config.value.regions = config.value.regions.includes(code) ? config.value.regions.filter(region => region !== code) : [...config.value.regions, code]
}
function resetParameters() {
  const selected = [...config.value.regions]
  config.value = { ...defaultConfig(), regions: selected }
  notify('已恢复默认参数，保留所选地区')
}
async function importIps(event: Event) {
  const input = event.target as HTMLInputElement
  const file = input.files?.[0]
  if (!file) return
  try {
    if (file.size > 2 * 1024 * 1024) throw new Error('列表文件不能超过 2 MiB')
    config.value.customIps = await file.text()
    config.value.source = 'custom'
    notify(`已载入 ${file.name}，开始前可检查 IP 列表`)
  } catch (error) { notify(`导入失败：${errorText(error)}`, true) }
  finally { input.value = '' }
}
function receiveRun(next: ScanRun, allowNewerSnapshot = false) {
  // Events can finish a short scan before an earlier command snapshot arrives.
  if (run.value?.id === next.id && run.value.status !== 'running' && next.status === 'running') return
  if (starting.value && next.id === previousRunIdForStart) return
  if (run.value && run.value.id !== next.id && !starting.value) {
    if (!allowNewerSnapshot || Date.parse(next.startedAt) <= Date.parse(run.value.startedAt)) return
  }
  const wasRunning = run.value?.status === 'running'
  run.value = next
  if (next.status !== 'running') {
    stopping.value = false
    if (wasRunning) notify(next.status === 'completed' ? '测试已完成，可在结果表筛选 IP' : next.status === 'cancelled' ? '测试已停止，已完成的候选已保留' : next.error ?? '测试未完成，请查看错误详情', next.status === 'failed')
  }
}
async function startScan() {
  if (!desktop || locked.value || validation.value) return
  starting.value = true
  previousRunIdForStart = run.value?.id
  pageError.value = ''
  advancedOpen.value = false
  try {
    const next = await invoke<ScanRun>('start_scan', { config: JSON.parse(JSON.stringify(config.value)) })
    if (run.value?.id !== next.id) run.value = next
    await nextTick()
    if (progressPanel.value) {
      progressPanel.value.scrollIntoView({ behavior: scrollBehavior(), block: 'start' })
      progressPanel.value.focus({ preventScroll: true })
    }
    const latest = await invoke<ScanRun | null>('get_scan')
    if (latest?.id === next.id) receiveRun(latest)
  } catch (error) { pageError.value = errorText(error) }
  finally { starting.value = false }
}
async function stopScan() {
  if (!run.value || stopping.value) return
  stopping.value = true
  try { await invoke('cancel_scan', { runId: run.value.id }) }
  catch (error) { stopping.value = false; notify(`停止失败：${errorText(error)}`, true) }
}
async function showFavorites(open: boolean) {
  dismissPopovers()
  favoritesOpen.value = open
  await nextTick()
  if (favoritesOpen.value) favoritesPanel.value?.focus({ preventScroll: true })
  else scanToggle.value?.focus({ preventScroll: true })
}
function updateFavorites(library: FavoriteLibrary) {
  if (!acceptsFavoriteLibrary(favorites.value, library)) return
  favorites.value = library
}
async function refreshFavorites() {
  try {
    const library = await invoke<FavoriteLibrary>('get_favorites')
    if (!disposed) updateFavorites(library)
  } catch (error) { if (!disposed) notify(`读取收藏失败：${errorText(error)}`, true) }
}
function receiveFavoriteTest(next: FavoriteTestJob) {
  const current = favoriteTest.value
  if (current?.run.id === next.run.id && current.run.status !== 'running' && next.run.status === 'running') return
  if (current && current.run.id !== next.run.id && Date.parse(next.run.startedAt) < Date.parse(current.run.startedAt)) return
  const finishedNow = current?.run.id === next.run.id && current.run.status === 'running' && next.run.status !== 'running'
  favoriteTest.value = next
  if (finishedNow) {
    const passed = next.run.results.filter(result => result.measurementPassed).length
    notify(next.run.status === 'completed' ? `收藏复测完成，${passed} / ${next.favoriteIds.length} 个 IP 完整测速通过`
      : next.run.status === 'cancelled' ? '收藏复测已停止；未开始的 IP 保留原测试记录'
      : next.run.error ?? '收藏复测失败，请查看详情', next.run.status === 'failed')
    void refreshFavorites()
  }
}
function onFavoriteTestStarted(start: FavoriteTestStart) {
  const finishedBeforeResponse = favoriteTest.value?.run.id === start.job.run.id && favoriteTest.value.run.status !== 'running'
  if (!finishedBeforeResponse) updateFavorites(start.favorites)
  receiveFavoriteTest(start.job)
  if (finishedBeforeResponse) void refreshFavorites()
}
async function openFavorite(result: IpResult, event: MouseEvent) {
  if (!desktop || loading.value || favoriteTesting.value || favoritePending.value || !run.value) return
  if (savedIps.value.has(result.ip)) {
    favoritePending.value = true
    try {
      updateFavorites(await invoke<FavoriteLibrary>('delete_favorites_for_ip', { ip: result.ip }))
      notify(`已取消 ${result.ip} 的收藏`)
    } catch (error) { notify(`取消收藏失败：${errorText(error)}`, true) }
    finally { favoritePending.value = false }
    return
  }
  favoriteTrigger = event.currentTarget instanceof HTMLElement ? event.currentTarget : null
  favoriteTarget.value = { runId: run.value.id, ip: result.ip }
}
async function closeFavorite() {
  favoriteTarget.value = null
  await nextTick()
  if (favoriteTrigger?.isConnected) favoriteTrigger.focus({ preventScroll: true })
  else favoritesToggle.value?.focus({ preventScroll: true })
}
watch(() => run.value?.id, id => {
  if (favoriteTarget.value && favoriteTarget.value.runId !== id) void closeFavorite()
})

watch(() => config.value.precisionMode, mode => {
  if (mode === 'adaptive') {
    const normalized = normalizeActiveConfig(config.value)
    config.value.candidatesPerRegion = normalized.candidatesPerRegion
    config.value.candidatePoolSize = normalized.candidatePoolSize
  } else config.value.candidatePoolSize = normalizeActiveConfig(config.value).candidatePoolSize
})

watch(() => config.value.downloadBudgetEnabled, enabled => {
  const budget = config.value.downloadBudgetMiB
  if (!enabled && (typeof budget !== 'number' || !Number.isInteger(budget) || budget < 1 || budget > 4096)) config.value.downloadBudgetMiB = defaultConfig().downloadBudgetMiB
}, { flush: 'sync' })

let previousBudgetEnabled = config.value.downloadBudgetEnabled
watch(config, () => {
  const budgetChanged = config.value.downloadBudgetEnabled !== previousBudgetEnabled
  previousBudgetEnabled = config.value.downloadBudgetEnabled
  if (!initialized.value || !desktop || locked.value) return
  clearTimeout(saveTimer)
  if (!validation.value) pendingConfig = JSON.parse(JSON.stringify(config.value))
  const persist = () => { void persistConfig().catch(() => {}) }
  if (budgetChanged) persist()
  else saveTimer = setTimeout(persist, 500)
}, { deep: true, flush: 'sync' })

onMounted(async () => {
  clock = setInterval(() => { now.value = Date.now() }, 1000)
  if (!desktop) { initialized.value = true; return }
  try {
    const stopClose = await appWindow!.onCloseRequested(requestWindowClose)
    if (disposed) { stopClose(); return }
    unlistenClose = stopClose
    const stopUpdates = await listen<ScanRun>('scan-update', event => receiveRun(event.payload))
    if (disposed) { stopUpdates(); return }
    unlisten = stopUpdates
    const stopWarnings = await listen<string>('storage-warning', event => { storageWarning.value = event.payload })
    if (disposed) { stopWarnings(); return }
    unlistenStorage = stopWarnings
    const stopFavoriteTests = await listen<FavoriteTestJob>('favorite-test-update', event => receiveFavoriteTest(event.payload))
    if (disposed) { stopFavoriteTests(); return }
    unlistenFavoriteTest = stopFavoriteTests
    const stopFavorites = await listen<FavoriteLibrary>('favorites-update', event => updateFavorites(event.payload))
    if (disposed) { stopFavorites(); return }
    unlistenFavorites = stopFavorites
    const saved = await invoke<SavedState>('load_state')
    if (disposed) return
    config.value = normalizeActiveConfig(saved.config)
    updateFavorites(saved.favorites ?? defaultFavoriteLibrary())
    if (saved.favoriteTest) receiveFavoriteTest(saved.favoriteTest)
    if (!storageWarning.value) storageWarning.value = saved.warning ?? ''
    if (!run.value) run.value = saved.lastRun ?? null
    const current = await invoke<ScanRun | null>('get_scan')
    if (disposed) return
    if (current) receiveRun(current, true)
    const latestFavoriteTest = await invoke<FavoriteTestJob | null>('get_favorite_test')
    if (disposed) return
    if (latestFavoriteTest) receiveFavoriteTest(latestFavoriteTest)
    if (run.value?.status === 'running') config.value = JSON.parse(JSON.stringify(run.value.config))
  } catch (error) { pageError.value = `初始化失败：${errorText(error)}` }
  finally { initialized.value = true; loading.value = false }
})
onBeforeUnmount(() => { disposed = true; unlisten?.(); unlistenStorage?.(); unlistenFavoriteTest?.(); unlistenFavorites?.(); unlistenClose?.(); clearInterval(clock); clearTimeout(saveTimer); clearTimeout(toastTimer) })
</script>

<template>
  <div ref="appShell" class="app-shell">
    <header class="app-header">
      <div class="brand"><span class="brand-mark"><Network :size="17" :stroke-width="1.7" /></span><span>IP Picker</span></div>
      <nav class="workspace-nav" aria-label="主导航">
        <button ref="scanToggle" class="nav-item" :class="{ active: !favoritesOpen }" :aria-current="!favoritesOpen ? 'page' : undefined" @click="showFavorites(false)"><Activity :size="15" />地区优选</button>
        <button ref="favoritesToggle" class="nav-item" :class="{ active: favoritesOpen }" :aria-current="favoritesOpen ? 'page' : undefined" aria-controls="favorites-library" @click="showFavorites(true)"><Star :size="15" />收藏夹<span v-if="favorites.entries.length" class="favorites-count">{{ favorites.entries.length }}</span></button>
      </nav>
      <div class="header-actions"><span class="local-indicator"><span class="status-dot" :class="{ active: running || favoriteTesting }" />{{ !desktop ? '界面预览' : favoriteTesting ? '收藏复测中' : running ? '正在测速' : '就绪' }}</span><button v-if="running && favoritesOpen" class="button stop-button header-stop" :disabled="stopping" @click="stopScan"><Square :size="12" />{{ stopping ? '正在停止…' : '停止测速' }}</button></div>
    </header>

    <main>


      <div v-show="favoritesOpen" id="favorites-library" ref="favoritesPanel" tabindex="-1" class="favorites-view"><FavoritesPanel :library="favorites" :desktop="desktop" :loading="loading" :config="config" :test-job="favoriteTest" :scan-busy="!!running || starting || stopping || windowClosing" @change="updateFavorites" @test-started="onFavoriteTestStarted" @notify="notify" /></div>

      <div v-show="!favoritesOpen" class="scan-view">
        <div class="scan-controls">
        <section class="configuration" aria-label="测速配置" :aria-busy="loading">
          <div class="configuration-primary workspace-toolbar">
            <div class="region-setting"><span class="toolbar-label">测试地区</span>
              <div id="region-choices" class="region-options" role="group" aria-label="选择目标国家或地区" :aria-invalid="fieldInvalid('regions')" :aria-describedby="fieldDescription('regions')">
                <button v-for="region in selectableRegions" :key="region.code" class="region-choice" :class="{ selected: config.regions.includes(region.code) }" :aria-pressed="config.regions.includes(region.code)" :disabled="locked" @click="toggleRegion(region.code)"><span class="region-check"><Check v-if="config.regions.includes(region.code)" :size="10" /></span><span>{{ region.name }}</span><span class="region-code">{{ region.code }}</span></button>
              </div>
            </div>
            <div class="configuration-actions workspace-actions">
              <button ref="settingsTrigger" class="button advanced-trigger workspace-secondary-action" :aria-expanded="advancedOpen" aria-controls="advanced-settings" @click="advancedOpen = true"><Settings2 :size="14" />高级设置</button>
              <button v-if="running" class="button stop-button start-button workspace-primary-action" :disabled="stopping" @click="stopScan"><LoaderCircle v-if="stopping" :size="14" class="spinning" /><Square v-else :size="12" />{{ stopping ? '正在停止…' : '停止测速' }}</button>
              <button v-else class="button primary start-button workspace-primary-action" :disabled="!desktop || locked || !!validation" @click="startScan"><LoaderCircle v-if="starting || loading" :size="14" class="spinning" /><Play v-else :size="12" fill="currentColor" />{{ scanActionLabel }}</button>
            </div>
          </div>
          <div class="configuration-secondary">
            <span class="target-summary" :title="config.targetUrl"><Globe2 :size="13" /><span>{{ targetHost }}</span></span><span class="config-divider" /><span class="config-meta">抽样 {{ Number(config.maxCandidates).toLocaleString('zh-CN') }}</span><span class="config-meta">TCP {{ stabilityMetricLabel(config.stabilityLatencyMetric) }} ≤ {{ config.maxTcpMs }} ms</span>
            <div class="budget-settings" aria-label="下载流量预算"><label class="check-field budget-toggle"><input id="setting-downloadBudgetEnabled" v-model="config.downloadBudgetEnabled" type="checkbox" :disabled="locked" aria-label="限制下载流量预算" aria-describedby="budget-help" /><span>限制下载流量</span></label><label v-if="config.downloadBudgetEnabled" class="budget-amount"><input id="setting-downloadBudgetMiB" v-model.number="config.downloadBudgetMiB" type="number" min="1" max="4096" step="10" :disabled="locked" :aria-invalid="fieldInvalid('downloadBudgetMiB')" :aria-describedby="fieldDescription('downloadBudgetMiB', 'budget-help')" aria-label="全局下载数据预算（MiB）" /><span>MiB</span></label><span id="budget-help" class="visually-hidden">所有下载共用预算，不含协议开销；关闭时不限制下载流量。</span></div>
          </div>
        </section>

          <div class="scan-status-slot" :class="{ 'is-error': !!validationIssue || run?.status === 'failed', 'is-warning': dirty && !running && !validationIssue, 'is-active': running }">
            <div v-if="validationIssue" class="scan-status-content" role="alert"><CircleAlert :size="13" /><span id="config-validation" :title="validation">{{ validation }}</span><button class="text-button" @click="focusSetting(validationIssue.field)">定位问题<ArrowRight :size="12" /></button></div>
            <div v-else-if="run?.status === 'failed'" class="scan-status-content" role="alert"><CircleAlert :size="13" /><span :title="run.error || run.progress.message">{{ run.error || run.progress.message || '测试未完成，请检查网络和测速地址。' }}</span></div>
            <details v-else-if="run && (running || starting)" ref="progressPanel" class="scan-progress-disclosure" aria-label="测速进度" tabindex="-1"><summary><LoaderCircle :size="12" class="spinning" /><span>{{ scanPhaseTitle }}</span><span class="muted">{{ elapsed }}</span><span class="scan-progress-label">查看进度<ChevronDown :size="12" /></span></summary><div class="scan-progress-popover"><ScanProgressPanel :run="run" :elapsed="elapsed" /></div></details>
            <div v-else-if="favoriteTesting" class="scan-status-content" role="status"><LoaderCircle :size="12" class="spinning" /><span>收藏复测进行中，结束后可开始优选。</span></div>
            <div v-else-if="dirty && run" class="scan-status-content" role="status"><Info :size="13" /><span>参数已修改，以下仍为上次测速结果。</span><button class="text-button" :disabled="locked" @click="config = normalizeActiveConfig(JSON.parse(JSON.stringify(run.config)))">沿用上次参数</button></div>
            <span v-else class="scan-idle-note">{{ run ? '本次结果保留测速时的参数与记录' : '选择目标地区后开始测速' }}</span>
          </div>
        </div>
        <div class="results-column">
          <AllResultsPanel v-if="run" :key="run.id" :run="run" :saved-ips="savedIps" :favorite-disabled="!desktop || loading || favoriteTesting || favoritePending || windowClosing" @favorite="openFavorite" @notify="notify" />
          <section v-else class="initial-empty" aria-labelledby="welcome-title"><Network :size="32" :stroke-width="1.2" /><h1 id="welcome-title">开始一次地区优选</h1><p>选择目标地区并开始测速，完成后在这里查看、筛选和收藏 IP。</p><span class="small muted">TCP 连通性 → 稳定性复测 → 服务验证 → 下载测速</span></section>
        </div>
      </div>
    </main>
    <footer class="app-statusbar"><span>Cloudflare IPv4</span><span>{{ favoritesOpen ? '收藏保存在本机' : running ? `已耗时 ${elapsed}` : 'TCP / HTTPS 实测' }}</span><span class="statusbar-version">v{{ appVersion }}</span></footer>

    <dialog id="advanced-settings" ref="settingsDialog" class="settings-drawer" aria-labelledby="settings-title" @cancel.prevent="closeSettings" @close="advancedOpen = false" @click="event => { if (event.target === settingsDialog) closeSettings() }">
      <div class="drawer-header"><div class="drawer-title-group"><span class="drawer-title-icon"><Settings2 :size="17" /></span><div><h2 id="settings-title">高级设置</h2><p>自动保存 · 下次测速生效</p></div></div><div class="drawer-header-actions"><button class="text-button" :disabled="locked" title="恢复默认参数，保留所选地区" @click="resetParameters"><RotateCcw :size="12" />恢复默认</button><button class="icon-button" autofocus aria-label="关闭高级设置" :disabled="finishingSettings || windowClosing" @click="closeSettings"><X :size="18" /></button></div></div>
      <nav class="settings-tabs" role="tablist" aria-label="设置分类" @keydown="onSettingsTabKey"><button v-for="section in settingsSections" :id="`settings-tab-${section.id}`" :key="section.id" role="tab" :class="{ selected: settingsSection === section.id }" :aria-selected="settingsSection === section.id" :aria-controls="`settings-panel-${section.id}`" :tabindex="settingsSection === section.id ? 0 : -1" @click="settingsSection = section.id"><component :is="section.icon" :size="14" />{{ section.label }}</button></nav>
      <div ref="settingsBody" class="drawer-body">
        <section v-show="settingsSection === 'target'" id="settings-panel-target" class="settings-section" role="tabpanel" aria-labelledby="settings-tab-target">
          <div class="settings-section-heading"><h3>目标与来源</h3><p>选择候选网段及用于验证、下载的目标。</p></div>
          <div class="settings-grid">
            <label class="field"><span>候选来源</span><SelectControl><select v-model="config.source" id="setting-source" :aria-invalid="fieldInvalid('source')" :aria-describedby="fieldDescription('source')" :disabled="locked"><option value="cloudflare">内置 Cloudflare IPv4 网段</option><option value="custom">自定义 IPv4 / CIDR</option></select></SelectControl></label>
            <label class="field span-two"><span>HTTPS 下载测速地址</span><input v-model.trim="config.targetUrl" id="setting-targetUrl" :aria-invalid="fieldInvalid('targetUrl')" :aria-describedby="fieldDescription('targetUrl', 'target-help')" type="url" :disabled="locked" spellcheck="false" placeholder="https://speed.cloudflare.com/__down?bytes=25000000" /></label>
            <div v-if="config.source === 'custom'" class="field full-width"><div class="section-line"><label for="custom-ips" class="muted small">IPv4 / CIDR 列表 · 每行一个，支持 # 注释</label><button class="button" :disabled="locked" @click="fileInput?.click()"><ArrowDownToLine :size="12" />载入 TXT 文件</button></div><textarea id="custom-ips" v-model="config.customIps" :aria-invalid="fieldInvalid('customIps')" :aria-describedby="fieldDescription('customIps')" rows="4" :disabled="locked" spellcheck="false" placeholder="104.16.0.0/24&#10;172.64.0.1" /><input ref="fileInput" class="hidden-file" type="file" accept=".txt,.list,text/plain" tabindex="-1" aria-hidden="true" @change="importIps" /></div>
          </div>

          <div class="settings-inline-note"><ShieldCheck :size="14" /><span>连接指定 IP，保留域名与 TLS 证书验证。</span></div>
          <details class="settings-help"><summary><Info :size="13" /><span>关于自定义目标</span><ChevronDown :size="12" /></summary>
          <p id="target-help" class="field-note">使用同一域名的 /cdn-cgi/trace 识别 POP；连接指定 IP，并保留域名与 TLS 证书验证。自定义目标需由 Cloudflare 代理且提供足够大的下载内容。</p>
          </details>
        </section>
        <section v-show="settingsSection === 'quality'" id="settings-panel-quality" class="settings-section" role="tabpanel" aria-labelledby="settings-tab-quality">
          <div class="settings-section-heading"><h3>筛选与下载</h3><p>设置候选规模、连接超时与下载复测。</p></div>
          <div class="settings-grid">
            <label class="field"><span>稳定性延迟上限（ms）</span><input v-model.number="config.maxTcpMs" id="setting-maxTcpMs" :aria-invalid="fieldInvalid('maxTcpMs')" :aria-describedby="fieldDescription('maxTcpMs', 'max-tcp-help')" type="number" min="1" max="30000" step="0.1" :disabled="locked" /></label>
            <label class="field"><span>抽样 IP 上限</span><input v-model.number="config.maxCandidates" id="setting-maxCandidates" :aria-invalid="fieldInvalid('maxCandidates')" :aria-describedby="fieldDescription('maxCandidates')" type="number" min="1" max="10000" step="1" :disabled="locked" /></label>
            <label class="field"><span>TCP 连通性并发</span><input v-model.number="config.tcpConcurrency" id="setting-tcpConcurrency" :aria-invalid="fieldInvalid('tcpConcurrency')" :aria-describedby="fieldDescription('tcpConcurrency')" type="number" min="1" max="256" :disabled="locked" /></label>
            <label class="field"><span>连接等待超时（ms）</span><input v-model.number="config.connectTimeoutMs" id="setting-connectTimeoutMs" :aria-invalid="fieldInvalid('connectTimeoutMs')" :aria-describedby="fieldDescription('connectTimeoutMs', 'max-tcp-help')" type="number" min="100" max="30000" step="100" :disabled="locked" /></label>
            <label class="field"><span>HTTPS 请求超时（毫秒）</span><input v-model.number="config.requestTimeoutMs" id="setting-requestTimeoutMs" :aria-invalid="fieldInvalid('requestTimeoutMs')" :aria-describedby="fieldDescription('requestTimeoutMs')" type="number" min="100" max="60000" step="500" :disabled="locked" /></label>
            <label class="field"><span>每区稳定性入围上限</span><input v-model.number="config.regionalCandidateLimit" id="setting-regionalCandidateLimit" :aria-invalid="fieldInvalid('regionalCandidateLimit')" :aria-describedby="fieldDescription('regionalCandidateLimit', 'candidates-per-region-help')" type="number" min="1" max="10000" :disabled="locked" /></label>
            <label class="field"><span>稳定性候选范围</span><SelectControl><select v-model="config.precisionMode" id="setting-precisionMode" :aria-invalid="fieldInvalid('precisionMode')" :aria-describedby="fieldDescription('precisionMode', 'candidates-per-region-help')" :disabled="locked"><option value="adaptive">按入围上限测试</option><option value="limited">叠加手动入围上限</option></select></SelectControl></label>
            <label v-if="config.precisionMode === 'limited'" class="field"><span>每区手动入围上限</span><input v-model.number="config.candidatesPerRegion" id="setting-candidatesPerRegion" :aria-invalid="fieldInvalid('candidatesPerRegion')" :aria-describedby="fieldDescription('candidatesPerRegion', 'candidates-per-region-help')" type="number" min="1" max="10000" :disabled="locked" /></label>
            <label class="field"><span>可用性验证次数</span><input v-model.number="config.verificationAttempts" id="setting-verificationAttempts" :aria-invalid="fieldInvalid('verificationAttempts')" :aria-describedby="fieldDescription('verificationAttempts')" type="number" min="2" max="10" :disabled="locked" /></label>
            <label class="field"><span>下载复测轮数</span><input v-model.number="config.downloadRounds" id="setting-downloadRounds" :aria-invalid="fieldInvalid('downloadRounds')" :aria-describedby="fieldDescription('downloadRounds')" type="number" min="2" max="5" :disabled="locked" /></label>
            <label class="field"><span>每轮下载时长（秒）</span><input v-model.number="config.downloadSeconds" id="setting-downloadSeconds" :aria-invalid="fieldInvalid('downloadSeconds')" :aria-describedby="fieldDescription('downloadSeconds')" type="number" min="1" max="15" :disabled="locked" /></label>
          </div>

          <details class="settings-help"><summary><Info :size="13" /><span>候选入围、超时与流量规则</span><ChevronDown :size="12" /></summary>
          <p id="max-tcp-help" class="field-note">初筛仅检查连通性，连接成功先识别地区；最终稳定性复测按所选指标与此上限判定。连接等待超时只控制等待连接成功的最长时间（默认 1500 ms），不是延迟准入上限。</p>
          <p id="candidates-per-region-help" class="field-note">内置网段默认每个 /24 随机抽取 1 个 IP。{{ precisionPolicyDescription(config) }}抽样 IP 不一定全部进入稳定性测试；下载仅测试各区稳定性达标前 10 名，也不是遍历网段内的所有地址。</p>
          <p class="field-note"><ShieldCheck :size="14" aria-hidden="true" /><span>不使用普通 HTTP 代理 · 各地区轮流下载 · {{ config.downloadBudgetEnabled ? '下载响应体受全局预算控制，不含协议开销和在途数据；' : '下载流量不设预算，每轮仍按设定时长结束；' }}TUN 与路由器代理仍可能影响路径。</span></p>

          </details>
        </section>
        <section v-show="settingsSection === 'stability'" id="settings-panel-stability" class="settings-section" role="tabpanel" aria-labelledby="settings-tab-stability">
          <div class="settings-section-heading"><h3>TCP 稳定性</h3><p>用多次独立连接评估延迟与连接失败率。</p></div>
            <div class="settings-grid">
              <label class="field"><span>延迟判定指标</span><SelectControl><select v-model="config.stabilityLatencyMetric" id="setting-stabilityLatencyMetric" :aria-invalid="fieldInvalid('stabilityLatencyMetric')" :aria-describedby="fieldDescription('stabilityLatencyMetric', 'tcp-stability-help')" :disabled="locked"><option value="p95">P95 延迟（95 分位）</option><option value="average">平均延迟</option><option value="max">每次最大延迟</option></select></SelectControl></label>
              <label class="field"><span>连续探测次数</span><input v-model.number="config.stabilityAttempts" id="setting-stabilityAttempts" :aria-invalid="fieldInvalid('stabilityAttempts')" :aria-describedby="fieldDescription('stabilityAttempts', 'tcp-stability-help')" type="number" min="10" max="1000" step="1" :disabled="locked" /></label>
              <label class="field"><span>探测间隔（ms）</span><input v-model.number="config.stabilityIntervalMs" id="setting-stabilityIntervalMs" :aria-invalid="fieldInvalid('stabilityIntervalMs')" :aria-describedby="fieldDescription('stabilityIntervalMs', 'tcp-stability-help')" type="number" min="50" max="5000" step="1" :disabled="locked" /></label>
              <label class="field"><span>允许连接失败率（%）</span><input v-model.number="config.maxStabilityFailurePercent" id="setting-maxStabilityFailurePercent" :aria-invalid="fieldInvalid('maxStabilityFailurePercent')" :aria-describedby="fieldDescription('maxStabilityFailurePercent', 'tcp-stability-help')" type="number" min="0" max="100" step="0.1" :disabled="locked" /></label>
            </div>

          <div class="stability-rule"><span class="stability-rule-label"><ShieldCheck :size="14" />本次判定</span><strong>{{ stabilityMetricLabel(config.stabilityLatencyMetric) }} ≤ {{ config.maxTcpMs }} ms <span>·</span> 连接失败率 ≤ {{ config.maxStabilityFailurePercent }}%</strong><p>累计 2 次连接失败立即淘汰；其余候选测满 {{ config.stabilityAttempts }} 次后判定。</p></div>
          <details class="settings-help"><summary><Info :size="13" /><span>探测方式与判定说明</span><ChevronDown :size="12" /></summary>
            <p id="tcp-stability-help" class="field-note">累计 2 次 TCP 连接失败立即淘汰，与允许连接失败率无关；其余候选完成全部 {{ config.stabilityAttempts }} 次探测，不因延迟偏高提前结束。测满后按 {{ stabilityMetricLabel(config.stabilityLatencyMetric) }}延迟 ≤ {{ config.maxTcpMs }} ms 与允许连接失败率判定。初筛只检查连接，单次初筛延迟不会淘汰 IP；连接失败不是 ICMP 丢包。全部稳定性测完后，每区仅选达标前 10 名进入下载测速。单个 IP 测满默认约 20 秒起，可随时停止。</p>
          </details>
        </section>
      </div>
      <div v-if="toast" class="drawer-notice" :class="{ error: toast.error }" role="status"><CircleAlert v-if="toast.error" :size="14" /><Check v-else :size="14" /><span>{{ toast.text }}</span></div>
      <div class="drawer-footer"><span id="settings-validation" class="drawer-save-status" role="status" :class="{ invalid: validation || configSaveError }"><CircleAlert v-if="validation || configSaveError" :size="13" /><LoaderCircle v-else-if="savingConfig || locked" :size="13" /><Check v-else :size="13" /><span>{{ validation || configSaveError || (savingConfig ? '正在保存设置…' : windowClosing ? '正在关闭窗口…' : locked ? '测试期间参数不可修改' : '有效参数自动保存；无效修改不会覆盖上次有效设置') }}</span></span><button class="button primary" :disabled="finishingSettings || windowClosing || !!validation" @click="finishSettings">{{ finishingSettings ? '保存中…' : '完成' }}<Check :size="14" /></button></div>
    </dialog>
    <FavoriteSaveDialog v-if="favoriteTarget && run && favoriteResult" :open="true" :library="favorites" :run="run" :result="favoriteResult" :desktop="desktop" @saved="updateFavorites" @change="updateFavorites" @close="closeFavorite" @notify="notify" />
    <div class="feedback-stack">
      <div v-if="pageError || storageWarning || configSaveError" class="global-notices">
      <div v-if="configSaveError" class="banner error-banner" role="alert"><CircleAlert :size="16" /><span>{{ configSaveError }}</span><button class="icon-button" aria-label="关闭设置保存提示" @click="configSaveError = ''"><X :size="14" /></button></div>
      <div v-if="pageError" class="banner error-banner" role="alert"><CircleAlert :size="16" /><span>{{ pageError }}</span><button class="icon-button" aria-label="关闭错误提示" @click="pageError = ''"><X :size="14" /></button></div>
      <div v-if="storageWarning" class="banner warning-banner" role="status"><CircleAlert :size="16" /><span>{{ storageWarning }}</span><button class="icon-button" aria-label="关闭存储提示" @click="storageWarning = ''"><X :size="14" /></button></div>
      </div>
    <Transition name="toast"><div v-if="toast" class="toast-message" :class="{ error: toast.error }" role="status"><CircleAlert v-if="toast.error" :size="17" /><Check v-else :size="17" /><span>{{ toast.text }}</span><button class="icon-button" aria-label="关闭通知" @click="toast = null"><X :size="14" /></button></div></Transition>
    </div>
  </div>
</template>
