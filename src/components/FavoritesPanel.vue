<script setup lang="ts">
import { computed, nextTick, onBeforeUnmount, ref, watch } from 'vue'
import { invoke } from '@tauri-apps/api/core'
import { Check, ChevronDown, ChevronRight, CircleAlert, Clipboard, FolderOpen, LoaderCircle, Pencil, Plus, RotateCcw, Search, Settings2, Square, Star, Trash2, X } from '@lucide/vue'
import { regionName, type ScanConfig } from '../types'
import { candidateExclusionReason, candidateMeasurementStatus, precisionPolicyLabel, stabilityMetricLabel, stabilityVerdict, targetHost } from '../ui-state'
import { favoriteDate, favoriteError, favoriteHasTestData, favoriteNumber, favoriteProbes, favoriteRunStatus, favoriteSnapshotRun, favoriteTcpSummary, favoriteTestPhase, validManualIpv4, type FavoriteEntry, type FavoriteFolder, type FavoriteLibrary, type FavoriteTestJob, type FavoriteTestStart } from '../favorites'
import SelectControl from './SelectControl.vue'

const props = defineProps<{ library: FavoriteLibrary; desktop: boolean; config: ScanConfig; loading?: boolean; testJob?: FavoriteTestJob | null; scanBusy?: boolean }>()
const emit = defineEmits<{
  change: [library: FavoriteLibrary]
  notify: [text: string, error?: boolean]
  'test-started': [start: FavoriteTestStart]
}>()

const panel = ref<HTMLElement | null>(null)
const selectedFolderId = ref('')
const search = ref('')
const table = ref<HTMLElement | null>(null)
const expandedEntryId = ref<string | null>(null)
const dialog = ref<HTMLDialogElement | null>(null)
const firstInput = ref<HTMLInputElement | HTMLSelectElement | null>(null)
const cancelButton = ref<HTMLButtonElement | null>(null)
const action = ref<'createFolder' | 'renameFolder' | 'deleteFolder' | 'editEntry' | 'deleteEntry' | 'addIp' | null>(null)
const actionFolder = ref<FavoriteFolder | null>(null)
const actionEntry = ref<FavoriteEntry | null>(null)
const name = ref('')
const note = ref('')
const folderId = ref('')
const manualIp = ref('')
const pending = ref(false)
const error = ref('')
const testError = ref('')
const starting = ref(false)
const stopping = ref(false)
const localJob = ref<FavoriteTestJob | null>(null)
let returnFocus: HTMLElement | null = null

const selectedFolder = computed(() => props.library.folders.find(folder => folder.id === selectedFolderId.value))
const entries = computed(() => props.library.entries.filter(entry => !selectedFolderId.value || entry.folderId === selectedFolderId.value)
  .slice().sort((a, b) => b.savedAt.localeCompare(a.savedAt)))
const folderEntryCount = (id: string) => props.library.entries.filter(entry => entry.folderId === id).length
const folderName = (id: string) => props.library.folders.find(folder => folder.id === id)?.name ?? '未知收藏夹'
const visibleEntries = computed(() => {
  const query = search.value.trim().toLocaleLowerCase()
  return query ? entries.value.filter(entry => [entry.result.ip, entry.result.colo, entry.result.city, entry.result.regionCode, regionName(entry.result.regionCode), ...entry.result.observedColos, entry.note, folderName(entry.folderId)]
    .some(value => value.toLocaleLowerCase().includes(query))) : entries.value
})
const folderAction = computed(() => action.value === 'createFolder' || action.value === 'renameFolder')
const deleting = computed(() => action.value === 'deleteFolder' || action.value === 'deleteEntry')
const title = computed(() => ({ createFolder: '新建收藏夹', renameFolder: '重命名收藏夹', deleteFolder: '删除收藏夹', editEntry: '编辑收藏', deleteEntry: '移除收藏', addIp: '手动添加 IP' })[action.value ?? 'createFolder'])
const duplicateEntry = computed(() => action.value === 'editEntry' && props.library.entries.some(entry => entry.id !== actionEntry.value?.id && entry.folderId === folderId.value && entry.result.ip === actionEntry.value?.result.ip))
const existingManualEntry = computed(() => props.library.entries.find(entry => entry.folderId === folderId.value && entry.result.ip === manualIp.value.trim()))
const invalidName = computed(() => !name.value.trim() || name.value.trim().length > 40 || props.library.folders.some(folder => folder.id !== actionFolder.value?.id && folder.name === name.value.trim()))
const job = computed(() => props.testJob ?? localJob.value)
const testRunning = computed(() => job.value?.run.status === 'running')
const disabled = computed(() => !props.desktop || props.loading || pending.value || starting.value || !!props.scanBusy || testRunning.value)
const formInvalid = computed(() => folderAction.value ? invalidName.value : action.value === 'editEntry' ? !folderId.value || duplicateEntry.value || note.value.length > 500 : action.value === 'addIp' ? !folderId.value || !validManualIpv4(manualIp.value) || note.value.length > 500 : false)
const currentSettings = computed(() => `${targetHost(props.config.targetUrl)} · TCP ${props.config.stabilityAttempts ?? '默认'} 次 · HTTPS ${props.config.verificationAttempts} 次 · 下载 ${props.config.downloadRounds} 轮 × ${props.config.downloadSeconds} 秒 · ${downloadBudgetLabel(props.config)}`)
const testTargetCount = computed(() => job.value?.favoriteIds.length ?? 0)
const testPassedCount = computed(() => job.value?.run.results.filter(result => result.measurementPassed).length ?? 0)
const testPercent = computed(() => {
  if (!job.value || !job.value.run.progress.total) return 0
  return Math.min(100, Math.round(job.value.run.progress.completed / job.value.run.progress.total * 100))
})
const testTargetFolders = computed(() => {
  if (!job.value) return ''
  const ids = new Set(job.value.favoriteIds)
  return [...new Set(props.library.entries.filter(entry => ids.has(entry.id)).map(entry => folderName(entry.folderId)))].join('、')
})
const isEntryTesting = (entry: FavoriteEntry) => !!testRunning.value && !!job.value?.favoriteIds.includes(entry.id)
function entryState(entry: FavoriteEntry) {
  if (isEntryTesting(entry)) return favoriteHasTestData(entry) ? '复测中 · 显示上次结果' : '等待测速结果'
  return favoriteHasTestData(entry) ? candidateMeasurementStatus(entry.result, favoriteSnapshotRun(entry)) : '尚未测速'
}
function detailId(id: string) { return `favorite-detail-${encodeURIComponent(id)}` }
function toggleId(id: string) { return `favorite-toggle-${encodeURIComponent(id)}` }
function toggleEntry(entry: FavoriteEntry) { expandedEntryId.value = expandedEntryId.value === entry.id ? null : entry.id }
async function collapseEntry() {
  const id = expandedEntryId.value
  expandedEntryId.value = null
  await nextTick()
  if (id) document.getElementById(toggleId(id))?.focus({ preventScroll: true })
}
watch(visibleEntries, list => {
  if (expandedEntryId.value && !list.some(entry => entry.id === expandedEntryId.value)) expandedEntryId.value = null
})
watch([search, selectedFolderId], async () => { await nextTick(); if (table.value) table.value.scrollTop = 0 })


watch(() => props.testJob, value => { if (value) localJob.value = value }, { immediate: true })
watch([manualIp, folderId], () => { error.value = '' })
watch(existingManualEntry, (entry, previous) => {
  if (action.value === 'addIp' && entry?.id !== previous?.id) note.value = entry?.note ?? ''
})

watch(() => props.library.folders, folders => {
  if (selectedFolderId.value && !folders.some(folder => folder.id === selectedFolderId.value)) selectedFolderId.value = ''
  if (folderId.value && !folders.some(folder => folder.id === folderId.value)) folderId.value = folders[0]?.id ?? ''
})

async function showDialog(kind: NonNullable<typeof action.value>, folder: FavoriteFolder | null = null, entry: FavoriteEntry | null = null) {
  if (disabled.value) return
  returnFocus = document.activeElement instanceof HTMLElement ? document.activeElement : null
  action.value = kind
  actionFolder.value = folder
  actionEntry.value = entry
  name.value = kind === 'renameFolder' ? folder?.name ?? '' : ''
  note.value = entry?.note ?? ''
  manualIp.value = ''
  folderId.value = entry?.folderId ?? (selectedFolderId.value || props.library.folders[0]?.id || '')
  error.value = ''
  await nextTick()
  dialog.value?.showModal()
  if (deleting.value) cancelButton.value?.focus()
  else firstInput.value?.focus()
}

async function closeDialog() {
  if (pending.value || starting.value) return
  dialog.value?.close()
  action.value = null
  const trigger = returnFocus
  returnFocus = null
  await nextTick()
  if (trigger?.isConnected && !trigger.matches(':disabled')) trigger.focus({ preventScroll: true })
  else panel.value?.querySelector<HTMLElement>('.favorite-folder.selected, .favorite-search input')?.focus({ preventScroll: true })
}

async function submit() {
  if (disabled.value || formInvalid.value || !action.value) return
  if (action.value === 'addIp') {
    const started = await startTest({ folderId: folderId.value, ip: manualIp.value.trim(), note: note.value.trim() }, true)
    if (started) closeDialog()
    return
  }
  pending.value = true
  error.value = ''
  const kind = action.value
  try {
    let library: FavoriteLibrary
    if (kind === 'createFolder') library = await invoke<FavoriteLibrary>('create_favorite_folder', { name: name.value.trim() })
    else if (kind === 'renameFolder' && actionFolder.value) library = await invoke<FavoriteLibrary>('rename_favorite_folder', { folderId: actionFolder.value.id, name: name.value.trim() })
    else if (kind === 'deleteFolder' && actionFolder.value) library = await invoke<FavoriteLibrary>('delete_favorite_folder', { folderId: actionFolder.value.id })
    else if (kind === 'editEntry' && actionEntry.value) library = await invoke<FavoriteLibrary>('update_favorite', { favoriteId: actionEntry.value.id, folderId: folderId.value, note: note.value.trim() })
    else if (kind === 'deleteEntry' && actionEntry.value) library = await invoke<FavoriteLibrary>('delete_favorite', { favoriteId: actionEntry.value.id })
    else throw new Error('收藏已发生变化，请关闭后重试。')
    emit('change', library)
    emit('notify', ({ createFolder: '已新建收藏夹', renameFolder: '已重命名收藏夹', deleteFolder: '已删除收藏夹及其中的收藏', editEntry: '已更新收藏', deleteEntry: '已移除收藏' })[kind])
    pending.value = false
    closeDialog()
  } catch (cause) {
    error.value = favoriteError(cause)
  } finally { pending.value = false }
}

async function startTest(target: { folderId?: string; favoriteId?: string; ip?: string; note?: string }, inDialog = false): Promise<boolean> {
  if (disabled.value) return false
  const updating = !!target.ip && !!existingManualEntry.value
  starting.value = true
  testError.value = ''
  error.value = ''
  try {
    const start = await invoke<FavoriteTestStart>('start_favorite_test', { config: JSON.parse(JSON.stringify(props.config)), ...target })
    localJob.value = start.job
    emit('test-started', start)
    if (target.folderId) selectedFolderId.value = target.folderId
    const finishedBeforeResponse = job.value?.run.id === start.job.run.id && job.value.run.status !== 'running'
    if (!finishedBeforeResponse) emit('notify', target.ip ? (updating ? '已开始重新测速，完成后更新此收藏' : '已收藏 IP 并开始测速') : `已开始复测 ${start.job.favoriteIds.length} 个收藏 IP`)
    return true
  } catch (cause) {
    if (inDialog) error.value = favoriteError(cause)
    else testError.value = favoriteError(cause)
    return false
  } finally { starting.value = false }
}

async function stopTest() {
  if (!props.desktop || !testRunning.value || stopping.value || !job.value) return
  stopping.value = true
  testError.value = ''
  try { await invoke('cancel_favorite_test', { runId: job.value.run.id }) }
  catch (cause) { testError.value = favoriteError(cause) }
  finally { stopping.value = false }
}

async function copyIp(ip: string) {
  if (!props.desktop) return
  try { await invoke('copy_text', { text: ip }); emit('notify', '已复制 IP') }
  catch (cause) { emit('notify', `复制失败：${favoriteError(cause)}`, true) }
}

function stabilityLabel(entry: FavoriteEntry) {
  const stats = entry.result.tcpStability
  if (!stats) return '未测试'
  if (stats.earlyRejection) return '提前淘汰'
  return ({ passed: '达标', failed: '未达标', incomplete: '未完成' })[stabilityVerdict(entry.result, entry.config)]
}

function roleLabel(entry: FavoriteEntry) {
  return entry.result.role === 'primary' ? '首选' : entry.result.role === 'backup' ? '备选' : entry.result.eligible ? '达标候选' : '候选'
}

function downloadBudgetLabel(config: ScanConfig) {
  return config.downloadBudgetEnabled !== false ? `预算 ${config.downloadBudgetMiB} MiB` : '不限制流量'
}

onBeforeUnmount(() => {
  dialog.value?.close()
  returnFocus?.focus()
})
</script>

<template>
  <section ref="panel" class="panel favorites-panel" aria-labelledby="favorites-title" :aria-busy="loading || pending || starting">
    <div class="favorite-top-controls">
    <div class="favorites-toolbar workspace-toolbar">
      <div class="favorites-identity"><span class="favorites-symbol"><Star :size="15" aria-hidden="true" /></span><div><h2 id="favorites-title" class="favorites-heading">IP 收藏夹<span class="favorite-total">{{ library.entries.length }}</span></h2><p class="muted small favorites-intro">按场景整理，保留实测快照</p></div></div>
      <div class="favorite-search"><Search :size="14" aria-hidden="true" /><input v-model="search" type="search" aria-label="搜索收藏 IP、POP、备注或分类" placeholder="搜索 IP、POP、备注或分类" /><button v-if="search" class="icon-button" type="button" aria-label="清除收藏搜索" @click="search = ''"><X :size="12" aria-hidden="true" /></button></div>
      <div class="actions favorite-top-actions workspace-actions"><button class="button workspace-secondary-action" :disabled="disabled" @click="showDialog('createFolder')"><FolderOpen :size="13" aria-hidden="true" />新建收藏夹</button><button class="button primary workspace-primary-action" :disabled="disabled || !library.folders.length" @click="showDialog('addIp')"><Plus :size="13" aria-hidden="true" />手动添加 IP</button></div>
    </div>
    <p v-if="loading" class="favorites-empty" role="status">正在读取收藏…</p>
    <template v-else>

      <div v-if="job" class="favorite-test-job" :class="{ 'is-running': testRunning }">
        <div class="favorite-job-summary" role="status" aria-live="polite">
          <strong class="favorite-job-status"><LoaderCircle v-if="testRunning" :size="13" class="spinning" aria-hidden="true" /><Check v-else-if="job.run.status === 'completed'" :size="13" aria-hidden="true" />{{ testRunning ? '收藏复测中' : favoriteRunStatus(job.run.status) }}</strong>
          <span class="favorite-job-target" :title="testTargetFolders">{{ testRunning ? favoriteTestPhase(job) : `${testTargetFolders || '收藏 IP'} · ${testTargetCount} 个 IP` }}</span>
          <span class="favorite-job-numbers">完整通过 <strong>{{ testPassedCount }} / {{ testTargetCount }}</strong><span>·</span>{{ (job.run.progress.downloadedBytes / 1024 / 1024).toFixed(1) }} MiB</span>
          <button v-if="testRunning" class="button stop-button favorite-compact" :disabled="stopping" @click="stopTest"><LoaderCircle v-if="stopping" :size="12" class="spinning" aria-hidden="true" /><Square v-else :size="11" aria-hidden="true" />{{ stopping ? '正在停止…' : '停止复测' }}</button>
          <details class="favorite-job-details"><summary><span>记录详情</span><ChevronDown :size="12" aria-hidden="true" /></summary><div class="favorite-job-detail-body"><strong>{{ testTargetFolders || '收藏 IP' }} · {{ testTargetCount }} 个 IP</strong><p>测试于 {{ favoriteDate(job.run.startedAt) }} · {{ favoriteRunStatus(job.run.status) }}</p><p>{{ favoriteTestPhase(job) }} · {{ downloadBudgetLabel(job.run.config) }}</p><p v-if="testRunning && job.run.progress.total">当前阶段 {{ job.run.progress.completed }} / {{ job.run.progress.total }}</p><p v-if="job.run.error" class="favorite-form-error">{{ job.run.error }}</p><p v-if="job.run.progress.message">{{ job.run.progress.message }}</p><p>{{ testRunning ? '复测期间保留上次测试快照，结束后更新取得的结果。' : job.run.status === 'cancelled' ? '已停止；收藏列表显示最近成功保存的记录，未开始测试的 IP 保留原结果。' : job.run.status === 'failed' ? '本次测试未完成；收藏列表显示最近成功保存的记录，可重新测速。' : '本次测试已结束；收藏列表显示最近成功保存的记录。' }}</p></div></details>
        </div>
        <div v-if="testRunning && job.run.progress.total" class="progress-track favorite-test-track" role="progressbar" aria-label="收藏复测当前阶段进度" :aria-valuenow="testPercent" aria-valuemin="0" aria-valuemax="100"><span :style="{ width: `${testPercent}%` }" /></div>
        <p v-if="job.run.error" class="favorite-form-error" role="alert" :title="job.run.error">{{ job.run.error }}</p>
      </div>
      <nav class="favorite-folder-list" aria-label="选择收藏夹">
        <button class="favorite-folder" :class="{ selected: !selectedFolderId }" :aria-pressed="!selectedFolderId" @click="selectedFolderId = ''"><Star :size="14" aria-hidden="true" />全部收藏<span class="favorite-folder-count">{{ library.entries.length }}</span></button>
        <button v-for="folder in library.folders" :key="folder.id" class="favorite-folder" :title="folder.name" :class="{ selected: selectedFolderId === folder.id }" :aria-pressed="selectedFolderId === folder.id" @click="selectedFolderId = folder.id"><FolderOpen :size="14" aria-hidden="true" /><span class="favorite-folder-name">{{ folder.name }}</span><span class="favorite-folder-count">{{ folderEntryCount(folder.id) }}</span></button>
      </nav>
      <div class="favorite-context-row">
        <span v-if="testError" class="favorite-context-message is-error" role="alert" :title="testError">{{ testError }}</span><span v-else-if="scanBusy" class="favorite-context-message" role="status" title="分区优选正在进行，请等待结束后再添加或复测收藏。">分区优选进行中，稍后可复测收藏</span><span v-else-if="!desktop" class="favorite-context-message" role="status">界面预览 · 请在桌面应用保存收藏</span>
        <span v-if="!testError && !scanBusy && desktop" class="favorite-result-count muted small" aria-live="polite">{{ search.trim() ? `找到 ${visibleEntries.length} / ${entries.length} 个收藏` : `${entries.length} 个 IP · 按收藏时间排列` }}</span>
        <div v-if="selectedFolder" class="actions favorite-folder-actions"><button class="text-button" :disabled="disabled || !entries.length" :aria-label="`一键复测本夹 ${selectedFolder.name}（${entries.length} 个 IP）`" @click="startTest({ folderId: selectedFolder.id })"><RotateCcw :size="12" aria-hidden="true" />复测本夹（{{ entries.length }}）</button><button class="icon-button" :disabled="disabled" :aria-label="`重命名收藏夹 ${selectedFolder.name}`" title="重命名收藏夹" @click="showDialog('renameFolder', selectedFolder)"><Pencil :size="12" aria-hidden="true" /></button><button class="icon-button favorite-danger" :disabled="disabled" :aria-label="`删除收藏夹 ${selectedFolder.name}`" title="删除收藏夹" @click="showDialog('deleteFolder', selectedFolder)"><Trash2 :size="12" aria-hidden="true" /></button></div>
        <span v-else-if="library.entries.length && !testRunning" class="favorite-folder-hint muted small">选择分类可整夹复测</span><span v-if="testRunning" class="favorite-table-live-note">复测中 · 表格暂显示上次记录，未取得数据记为 —</span>
        <details class="favorite-test-settings"><summary><Settings2 :size="12" aria-hidden="true" /><span>复测参数</span><ChevronDown :size="12" aria-hidden="true" /></summary><div class="favorite-test-settings-body"><strong class="small">{{ targetHost(config.targetUrl) }}</strong><p class="small" :title="config.targetUrl">{{ currentSettings }}</p><p class="muted small">收藏夹复测会测试指定的全部 IP，不按地区或前 10 名筛选。{{ config.downloadBudgetEnabled !== false ? '下载预算用尽时，未完成的下载会标明原因。' : '不限制流量，全部 IP 测完或主动停止时结束。' }}</p></div></details>
      </div>
    </template>
    </div>
    <template v-if="!loading">
      <div v-if="!entries.length" class="favorites-empty"><FolderOpen :size="26" aria-hidden="true" /><strong>{{ !library.folders.length ? '还没有收藏夹' : selectedFolder ? '这个收藏夹还没有 IP' : '还没有收藏 IP' }}</strong><p>{{ !library.folders.length ? '新建一个收藏夹，按运营商或使用场景整理。' : '手动添加 IP 并测速，或在分区优选后收藏测试结果。' }}</p></div>
      <div v-else-if="!visibleEntries.length" class="favorites-empty"><Search :size="26" aria-hidden="true" /><strong>没有找到匹配的收藏</strong><p>试试其他 IP、POP、备注或分类名称。</p><button class="button" @click="search = ''">清除搜索</button></div>
      <div v-else ref="table" class="favorites-table" role="region" aria-label="收藏 IP 表格，可滚动" tabindex="0">
        <table>
          <colgroup><col class="favorite-col-ip" /><col class="favorite-col-folder" /><col class="favorite-col-region" /><col class="favorite-col-tcp" /><col class="favorite-col-speed" /><col class="favorite-col-actions" /></colgroup>
          <thead><tr><th scope="col">IP 地址 <span class="favorite-heading-hint">点击展开详情</span></th><th scope="col">分类</th><th scope="col">地区 / POP</th><th scope="col">TCP 延迟 <span class="favorite-table-unit">ms</span></th><th scope="col">下载 <span class="favorite-table-unit">Mbps</span></th><th scope="col">操作</th></tr></thead>
          <tbody>
            <template v-for="(entry, index) in visibleEntries" :key="entry.id">
              <tr class="favorite-data-row" :class="{ 'is-alt': index % 2 === 1, 'is-expanded': expandedEntryId === entry.id, 'is-testing': isEntryTesting(entry) }">
                <td><button :id="toggleId(entry.id)" class="favorite-row-toggle" :aria-expanded="expandedEntryId === entry.id" :aria-controls="detailId(entry.id)" :aria-label="`${expandedEntryId === entry.id ? '收起' : '展开'} ${entry.result.ip} 在${folderName(entry.folderId)}中的测试详情`" :title="entryState(entry)" @click="toggleEntry(entry)"><ChevronRight :size="12" class="favorite-expand-icon" :class="{ expanded: expandedEntryId === entry.id }" aria-hidden="true" /><span class="favorite-state-dot" :class="{ good: entry.result.measurementPassed && !isEntryTesting(entry), testing: isEntryTesting(entry) }" aria-hidden="true" /><span class="mono">{{ entry.result.ip }}</span></button></td>
                <td><span class="favorite-table-folder" :title="folderName(entry.folderId)">{{ folderName(entry.folderId) }}</span></td>
                <td><span class="favorite-table-location" :title="`${entry.result.city || '城市未识别'} · ${roleLabel(entry)}`"><span>{{ regionName(entry.result.regionCode) || '地区未识别' }}</span><span v-if="entry.result.colo" class="favorite-pop">{{ entry.result.colo }}</span></span></td>
                <td :title="`TCP ${favoriteTcpSummary(entry.result, entry.config).label}，按本条收藏的测试判据显示`"><span class="favorite-table-metric"><span class="favorite-metric-label">{{ favoriteTcpSummary(entry.result, entry.config).label }}</span><span>{{ favoriteNumber(favoriteTcpSummary(entry.result, entry.config).value) }}</span></span></td>
                <td><span class="favorite-table-speed" :class="{ measured: entry.result.downloadSamplesMbps.length > 0 }">{{ entry.result.downloadSamplesMbps.length ? favoriteNumber(entry.result.downloadMbps) : '—' }}</span></td>
                <td><div class="favorite-row-actions"><button class="icon-button" :disabled="!desktop" :aria-label="`复制收藏的 ${entry.result.ip}（${folderName(entry.folderId)}）`" title="复制 IP" @click="copyIp(entry.result.ip)"><Clipboard :size="13" aria-hidden="true" /></button><button class="icon-button" :disabled="disabled" :aria-label="`重新测速 ${entry.result.ip}（${folderName(entry.folderId)}）`" title="重新测速" @click="startTest({ favoriteId: entry.id })"><RotateCcw :size="13" aria-hidden="true" /></button><button class="icon-button" :disabled="disabled" :aria-label="`编辑收藏 ${entry.result.ip}（${folderName(entry.folderId)}）`" title="编辑分类与备注" @click="showDialog('editEntry', null, entry)"><Pencil :size="13" aria-hidden="true" /></button><button class="icon-button favorite-danger" :disabled="disabled" :aria-label="`移除收藏 ${entry.result.ip}（${folderName(entry.folderId)}）`" title="移除本条收藏" @click="showDialog('deleteEntry', null, entry)"><Trash2 :size="13" aria-hidden="true" /></button></div></td>
              </tr>
              <tr v-if="expandedEntryId === entry.id" class="favorite-detail-row"><td colspan="6"><section :id="detailId(entry.id)" class="favorite-inline-detail" :aria-label="`${entry.result.ip} 在${folderName(entry.folderId)}中的测试详情`"><div class="favorite-detail-heading"><div><strong class="mono">{{ entry.result.ip }}</strong><span class="favorite-detail-status" :class="{ passed: entry.result.measurementPassed && !isEntryTesting(entry) }">{{ entryState(entry) }}</span><span>{{ folderName(entry.folderId) }}</span></div><button class="text-button" :aria-label="`收起 ${entry.result.ip} 的详情`" @click="collapseEntry">收起<ChevronDown :size="12" class="favorite-collapse-icon" /></button></div>
            <dl class="favorite-detail-grid">
              <div><dt>收藏时间</dt><dd>{{ favoriteDate(entry.savedAt) }}</dd></div><div><dt>最近测试</dt><dd>{{ favoriteHasTestData(entry) ? `${favoriteDate(entry.runFinishedAt || entry.runStartedAt)} · ${favoriteRunStatus(entry.runStatus)}` : '尚无测试记录' }}</dd></div>
              <div><dt>地区 / 城市 / POP</dt><dd>{{ regionName(entry.result.regionCode) || '地区未识别' }} · {{ entry.result.city || '城市未识别' }} · {{ entry.result.colo || 'POP 未识别' }}</dd></div><div><dt>收藏时角色</dt><dd>{{ roleLabel(entry) }}</dd></div>
              <div v-if="entry.note" class="favorite-full"><dt>备注</dt><dd class="favorite-note-full">{{ entry.note }}</dd></div>
              <div class="favorite-full"><dt>测速结论</dt><dd>{{ candidateExclusionReason(entry.result) || (entry.result.measurementPassed ? '测速通过' : '尚未完成完整测速') }}{{ entry.result.eligible ? ' · 符合本次优选条件' : '' }}</dd></div>
              <div><dt>TCP 连接失败</dt><dd>{{ entry.result.tcpStability ? `${entry.result.tcpStability.failed} / ${entry.result.tcpStability.attempted} · ${favoriteNumber(entry.result.tcpStability.failurePercent)}%` : '尚无测试数据' }}</dd></div>
              <div><dt>HTTPS 冷连接首响</dt><dd>{{ entry.result.successCount ? `${favoriteNumber(entry.result.httpsMs)} ms` : '尚无成功数据' }}</dd></div>
              <div><dt>测试开始</dt><dd>{{ favoriteDate(entry.runStartedAt) }}</dd></div><div><dt>测试结束</dt><dd>{{ entry.runFinishedAt ? favoriteDate(entry.runFinishedAt) : '收藏时尚未结束' }}</dd></div>
              <div class="favorite-full"><dt>测速地址</dt><dd class="mono">{{ entry.config.targetUrl }}</dd></div>
              <div><dt>测试偏好</dt><dd>{{ entry.config.preference === 'daily' ? `日常优选 · 至少 ${entry.config.minDownloadMbps} Mbps` : '下载优先' }}</dd></div><div><dt>本次观测 POP</dt><dd>{{ entry.result.observedColos.join(' / ') || entry.result.colo || '未识别' }}</dd></div>
              <div class="favorite-full"><dt>精测策略</dt><dd>{{ entry.targeted ? `指定 IP 复测 · 全部测试，不按地区或前 10 名筛选；累计 2 次连接失败立即淘汰，${entry.config.downloadBudgetEnabled !== false ? '下载预算用尽、全部 IP 测完或主动停止时结束。' : '不限制流量，全部 IP 测完或主动停止时结束。'}` : precisionPolicyLabel(entry.config) }}</dd></div>
              <div><dt>HTTPS 服务校验</dt><dd>成功 {{ entry.result.successCount }} / 已测 {{ entry.result.attempts }} · 计划 {{ entry.config.verificationAttempts }} 次</dd></div><div><dt>HTTPS 首响波动</dt><dd>{{ entry.result.successCount ? `${favoriteNumber(entry.result.jitterMs)} ms` : '尚无成功数据' }}</dd></div>
              <div><dt>下载样本</dt><dd>{{ entry.result.downloadSamplesMbps.length ? entry.result.downloadSamplesMbps.map(favoriteNumber).join(' / ') + ' Mbps' : '尚无下载样本' }}</dd></div><div><dt>下载计划</dt><dd>{{ entry.config.downloadRounds }} 轮 × {{ entry.config.downloadSeconds }} 秒 · {{ downloadBudgetLabel(entry.config) }}</dd></div>
              <div><dt>TCP 初筛延迟</dt><dd>{{ entry.result.tcpMs > 0 || entry.result.attempts ? `${favoriteNumber(entry.result.tcpMs)} ms` : '尚无成功数据' }}</dd></div><div><dt>TCP 稳定性</dt><dd>{{ stabilityLabel(entry) }} · 计划 {{ entry.config.stabilityAttempts ?? '历史未记录' }} 次 · 间隔 {{ entry.config.stabilityIntervalMs }} ms</dd></div>
              <template v-if="entry.result.tcpStability"><div><dt>TCP 稳定性进度</dt><dd>已测 {{ entry.result.tcpStability.attempted }} / {{ entry.result.tcpStability.requested }} · 成功 {{ entry.result.tcpStability.succeeded }} · 失败 {{ entry.result.tcpStability.failed }}</dd></div><div><dt>TCP 统计（ms）</dt><dd>均值 {{ favoriteNumber(entry.result.tcpStability.averageMs) }} · 中位 {{ favoriteNumber(entry.result.tcpStability.medianMs) }} · P95 {{ favoriteNumber(entry.result.tcpStability.p95Ms) }}</dd></div><div><dt>TCP 范围与波动（ms）</dt><dd>最小 {{ favoriteNumber(entry.result.tcpStability.minMs) }} · 最大 {{ favoriteNumber(entry.result.tcpStability.maxMs) }} · 波动 {{ favoriteNumber(entry.result.tcpStability.jitterMs) }}</dd></div><div><dt>TCP 延迟超限样本</dt><dd>{{ entry.result.tcpStability.overLimitCount }} 次 · 本次上限 {{ entry.result.tcpStability.delayLimitMs }} ms</dd></div><div v-if="entry.result.tcpStability.earlyRejection" class="favorite-full"><dt>提前淘汰原因</dt><dd>{{ entry.result.tcpStability.earlyRejection }}</dd></div></template>
              <div><dt>稳定性筛选条件</dt><dd>{{ stabilityMetricLabel(entry.config.stabilityLatencyMetric) }} ≤ {{ entry.config.maxTcpMs ?? '不限' }} ms · 失败率 ≤ {{ entry.config.maxStabilityFailurePercent }}%</dd></div><div><dt>请求超时</dt><dd>TCP {{ entry.config.connectTimeoutMs }} ms · HTTPS {{ entry.config.requestTimeoutMs }} ms</dd></div>
              <template v-if="entry.networkPath"><div><dt>测试网络接口</dt><dd>{{ entry.networkPath.interfaceName }}{{ entry.networkPath.tunnelInterface ? ' · 隧道接口' : '' }}</dd></div><div><dt>网络路径采样</dt><dd>{{ entry.networkPath.sampleIp }} · {{ favoriteDate(entry.networkPath.capturedAt) }}</dd></div></template>
              <div class="favorite-full"><dt>测试记录 ID</dt><dd class="mono">{{ entry.runId }}</dd></div>
            </dl>
            <details v-if="entry.result.tcpStability?.probesMs.length" class="favorite-probes"><summary>TCP 逐次探测 · {{ entry.result.tcpStability.probesMs.length }} 次（ms）</summary><p class="mono">{{ favoriteProbes(entry.result.tcpStability.probesMs) }}</p></details>
              </section></td></tr>
            </template>
          </tbody>
        </table>
      </div>

    </template>
    <div class="favorite-table-footer"><span>{{ search.trim() ? `显示 ${visibleEntries.length} / ${entries.length} 条收藏` : `共 ${entries.length} 条收藏` }}</span><span>点击 IP 左侧箭头展开详情</span></div>
    <Teleport to="body">
      <dialog ref="dialog" class="favorite-modal" aria-labelledby="favorite-maintain-title" @cancel.prevent="closeDialog" @click.self="closeDialog">
        <form @submit.prevent="submit">
          <div class="favorite-modal-heading"><div class="favorite-modal-identity"><span class="favorite-modal-symbol" :class="{ danger: deleting }"><Trash2 v-if="deleting" :size="16" aria-hidden="true" /><Plus v-else-if="action === 'addIp'" :size="16" aria-hidden="true" /><FolderOpen v-else-if="folderAction" :size="16" aria-hidden="true" /><Pencil v-else :size="16" aria-hidden="true" /></span><div><h2 id="favorite-maintain-title">{{ title }}</h2><p>{{ deleting ? '请确认要移除的收藏内容' : action === 'addIp' ? '保存地址，并立即验证当前网络表现' : action === 'editEntry' ? '调整分类与备注，保留测试快照' : '用网络或使用场景为收藏分类' }}</p></div></div><button type="button" class="icon-button" :disabled="pending || starting" aria-label="关闭" @click="closeDialog"><X :size="16" aria-hidden="true" /></button></div>
          <div class="favorite-modal-body">
          <template v-if="folderAction"><label class="field"><span>收藏夹名称</span><input ref="firstInput" v-model="name" maxlength="40" :disabled="pending" required placeholder="例如：联通流量" :aria-describedby="name.trim() && invalidName ? 'favorite-folder-name-error' : undefined" /></label><p v-if="name.trim() && invalidName" id="favorite-folder-name-error" class="favorite-form-error">收藏夹名称已存在，或名称超过 40 个字。</p><p class="small muted favorite-input-hint">最多 40 个字。</p></template>
          <template v-else-if="action === 'addIp'">
            <label class="field"><span>IPv4 地址</span><input ref="firstInput" v-model="manualIp" :disabled="starting || scanBusy || testRunning" maxlength="15" required spellcheck="false" autocomplete="off" inputmode="decimal" placeholder="例如：104.16.0.1" :aria-invalid="manualIp.trim() && !validManualIpv4(manualIp) || undefined" aria-describedby="favorite-manual-ip-hint" /></label>
            <p id="favorite-manual-ip-hint" class="small favorite-input-hint" :class="manualIp.trim() && !validManualIpv4(manualIp) ? 'favorite-form-error' : 'muted'">{{ manualIp.trim() && !validManualIpv4(manualIp) ? '请输入一个有效的 IPv4 地址，例如 104.16.0.1。' : '一次添加一个 IPv4 地址，不支持网段、域名或 IPv6。' }}</p>
            <label class="field favorite-note-field"><span>保存到收藏夹</span><SelectControl><select v-model="folderId" :disabled="starting || scanBusy || testRunning" required><option v-for="folder in library.folders" :key="folder.id" :value="folder.id">{{ folder.name }}</option></select></SelectControl></label>
            <p v-if="existingManualEntry" class="favorite-existing-note">此收藏夹已收藏该 IP。将重新测速并更新现有收藏，不会重复添加；测试结束前保留原结果。</p>
            <label class="field favorite-note-field"><span>备注（可选）</span><textarea v-model="note" :disabled="starting || scanBusy || testRunning" maxlength="500" placeholder="记录适用网络、使用体验等" /></label><p class="small muted favorite-input-hint">{{ note.length }} / 500</p>
            <div class="favorite-manual-settings"><p class="small" :title="config.targetUrl">按当前参数：{{ currentSettings }}</p><p class="small muted">收藏后立即测试这个 IP，完成后保存测试结果。</p></div>
          </template>
          <template v-else-if="action === 'editEntry'"><p class="favorite-modal-subtitle mono">{{ actionEntry?.result.ip }}</p><label class="field"><span>移至收藏夹</span><SelectControl><select ref="firstInput" v-model="folderId" :disabled="pending" required><option v-for="folder in library.folders" :key="folder.id" :value="folder.id">{{ folder.name }}</option></select></SelectControl></label><p v-if="duplicateEntry" class="favorite-form-error">该收藏夹已存在这个 IP，请选择其他收藏夹。</p><label class="field favorite-note-field"><span>备注（可选）</span><textarea v-model="note" :disabled="pending" maxlength="500" placeholder="记录适用网络、使用体验等" /></label><p class="small muted favorite-input-hint">{{ note.length }} / 500 · 测试快照保持当前内容。</p></template>
          <p v-else-if="action === 'deleteFolder'" class="favorite-delete-copy"><CircleAlert :size="16" aria-hidden="true" /><span>删除「{{ actionFolder?.name }}」会同时移除其中的 <strong>{{ folderEntryCount(actionFolder?.id ?? '') }} 个收藏及测试快照</strong>。此操作无法撤销。</span></p>
          <p v-else-if="action === 'deleteEntry'" class="favorite-delete-copy"><CircleAlert :size="16" aria-hidden="true" /><span>将从「{{ folderName(actionEntry?.folderId ?? '') }}」移除 <strong class="mono">{{ actionEntry?.result.ip }}</strong> 及其测试快照。此操作无法撤销。</span></p>
          </div>
          <p v-if="error" class="favorite-dialog-error" role="alert" tabindex="0" aria-label="提交错误，可滚动查看完整内容"><CircleAlert :size="13" aria-hidden="true" /><span>{{ error }}</span></p>
          <div class="favorite-modal-actions"><button ref="cancelButton" type="button" class="button" :disabled="pending || starting" @click="closeDialog">取消</button><button class="button" :class="deleting ? 'favorite-delete-button' : 'primary'" type="submit" :disabled="disabled || formInvalid"><LoaderCircle v-if="starting" :size="14" class="spinning" aria-hidden="true" />{{ starting ? '正在启动测速…' : pending ? '正在保存…' : deleting ? '确认删除' : action === 'createFolder' ? '新建收藏夹' : action === 'addIp' ? existingManualEntry ? '重新测速并更新收藏' : '收藏并测速' : '保存修改' }}</button></div>
        </form>
      </dialog>
    </Teleport>
  </section>
</template>

<style scoped>
.favorites-panel { display: flex; flex: 1; flex-direction: column; min-height: 0; min-width: 0; overflow: hidden; margin: 0; padding: 0; border: 0; border-radius: 0; background: var(--panel); }
.favorites-panel>div:not(.favorites-table):not(.favorites-empty), .favorites-panel>nav, .favorites-panel>p { flex: 0 0 auto; }
.favorite-top-controls { flex: 0 0 var(--workspace-controls-height,208px); height: var(--workspace-controls-height,208px); min-height: 0; display: flex; flex-direction: column; position: relative; z-index: 3; overflow: visible; }
.favorite-top-controls>* { flex-shrink: 0; }
.favorites-identity { display: flex; align-items: center; gap: 9px; min-width: 170px; margin-right: auto; }
.favorites-symbol, .favorite-modal-symbol { display: inline-grid; place-items: center; flex: 0 0 auto; width: 30px; height: 30px; color: var(--brand); background: var(--soft); border: 1px solid var(--selected-border); border-radius: 8px; box-shadow: inset 0 1px 0 color-mix(in srgb,var(--panel) 70%,transparent); }
.favorites-heading { display: flex; align-items: center; gap: 8px; line-height: 20px; font-size: 14px; }
.favorite-total { display: inline-grid; place-items: center; min-width: 19px; height: 17px; padding: 0 5px; border-radius: 5px; color: var(--brand); background: var(--soft); font-size: 10px; font-variant-numeric: tabular-nums; font-weight: 500; }
.favorites-intro { margin-top: 2px; }
.favorite-search { position: relative; display: flex; align-items: center; width: clamp(190px,25vw,280px); }
.favorite-search>svg { position: absolute; left: 10px; color: var(--muted); pointer-events: none; }
.favorite-search>input[type=search] { height: 30px; padding-left: 31px; padding-right: 29px; font-size: 11px; background: var(--surface-raised); border-color: var(--line); box-shadow: var(--control-shadow); }
.favorite-search>.icon-button { position: absolute; right: 2px; width: 24px; height: 24px; min-height: 24px; }
.favorite-folder-list { display: flex; flex-wrap: nowrap; align-items: center; gap: 6px; margin: 0; padding: 4px 20px 8px; overflow-x: auto; scrollbar-width: thin; border-bottom: 1px solid var(--line); }
.favorite-folder { display: inline-flex; align-items: center; gap: 6px; min-height: 29px; padding: 0 9px; border: 1px solid var(--line); border-radius: 7px; color: var(--muted); background: var(--surface-raised); font-size: 11px; flex: 0 0 auto; max-width: 260px; white-space: nowrap; box-shadow: var(--control-shadow); transition: border-color .14s,background .14s; }
.favorite-folder-name { min-width: 0; overflow: hidden; text-overflow: ellipsis; }
.favorite-folder>.favorite-folder-count { flex: 0 0 auto; }
.favorite-folder:hover { color: var(--text); border-color: var(--line-hover); background: var(--bg); }
.favorite-folder>.favorite-folder-count { display: inline-grid; place-items: center; min-width: 16px; height: 16px; padding: 0 4px; border-radius: 4px; color: var(--muted); background: var(--surface-sunken); font-size: 10px; font-variant-numeric: tabular-nums; }
.favorite-folder.selected { color: var(--brand); border-color: var(--selected-border); background: var(--soft); box-shadow: inset 0 1px 0 color-mix(in srgb,var(--panel) 75%,transparent),var(--control-shadow); }
.favorite-folder.selected>.favorite-folder-count { color: var(--brand); background: color-mix(in srgb,var(--brand) 10%,var(--panel)); }
.favorite-context-row { margin-top: auto; display: flex; align-items: center; gap: 12px; min-height: 34px; padding: 0 20px; font-size: 11px; }
.favorite-folder-actions { gap: 3px; }
.favorite-folder-actions>.icon-button { width: 24px; height: 24px; min-height: 24px; padding: 6px; color: var(--muted); }
.favorite-folder-actions>.text-button { margin-right: 4px; }
.favorite-folder-hint { color: color-mix(in srgb,var(--muted) 75%,var(--line-hover)); }
.favorite-test-settings { position: relative; margin-left: auto; }
.favorite-test-settings>summary { display: flex; align-items: center; gap: 5px; min-height: 26px; padding: 0 7px; border: 1px solid transparent; border-radius: 6px; color: var(--muted); list-style: none; cursor: pointer; font-size: 11px; white-space: nowrap; }
.favorite-test-settings>summary:hover, .favorite-test-settings[open]>summary { color: var(--brand); border-color: var(--selected-border); background: var(--soft); }
.favorite-test-settings-body { max-height: 50vh; overflow-y: auto; position: absolute; top: calc(100% + 4px); right: 0; z-index: 4; width: min(380px,calc(100vw - 60px)); padding: 12px; color: var(--text); background: var(--surface-raised); border: 1px solid var(--line); border-radius: 8px; box-shadow: 0 8px 24px #23284918; overflow-wrap: anywhere; }
.favorite-test-settings-body>p { margin-top: 5px; }
.favorite-test-job { position: relative; margin: 0 20px 10px; padding: 7px 10px; border: 1px solid var(--line); border-radius: 7px; background: var(--surface-raised); }
.favorite-job-summary { display: flex; align-items: center; gap: 12px; min-height: 22px; }
.favorite-job-status { display: inline-flex; align-items: center; flex: 0 0 auto; gap: 6px; font-size: 11px; font-weight: 500; line-height: 20px; }
.favorite-job-status>svg { color: var(--brand); }
.favorite-job-target { min-width: 0; max-width: 210px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; color: var(--muted); font-size: 10px; }
.favorite-job-numbers { display: inline-flex; align-items: center; gap: 7px; color: var(--muted); font-size: 10px; font-variant-numeric: tabular-nums; white-space: nowrap; }
.favorite-job-numbers>strong { color: var(--text); font-weight: 500; }
.favorite-job-numbers>span { color: var(--line-hover); }
.favorite-job-summary>.stop-button { min-height: 26px; font-size: 10px; }
.favorite-job-details { position: relative; flex: 0 0 auto; margin-left: auto; }
.favorite-job-details>summary { display: flex; align-items: center; gap: 5px; min-height: 22px; color: var(--muted); font-size: 10px; list-style: none; }
.favorite-job-details>summary:hover { color: var(--brand); }
.favorite-job-detail-body { position: absolute; top: calc(100% + 8px); right: -1px; z-index: 5; width: min(380px,calc(100vw - 60px)); max-height: 55vh; overflow: auto; padding: 12px; border: 1px solid var(--line); border-radius: 8px; color: var(--muted); background: var(--surface-raised); box-shadow: 0 8px 24px #23284918; font-size: 11px; line-height: 20px; overflow-wrap: anywhere; }
.favorite-job-detail-body>strong { color: var(--text); font-size: 11px; font-weight: 500; }
.favorite-job-detail-body>p { margin-top: 5px; }
@media (max-width: 800px) { .favorite-job-summary { gap: 8px; flex-wrap: wrap; } .favorite-job-target { max-width: 180px; } }
.favorite-test-job>.favorite-form-error { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; margin-top: 4px; font-size: 10px; line-height: 16px; }
.favorite-test-job.is-running { border-color: var(--selected-border); background: var(--soft); }
.favorite-test-track { position: absolute; inset: auto 0 0; height: 2px; margin: 0; border-radius: 0 0 7px 7px; }
.favorites-empty { flex: 1; min-height: 0; overflow: auto; justify-content: center; display: flex; flex-direction: column; align-items: center; gap: 8px; text-align: center; padding: 36px 16px; color: var(--muted); }
.favorites-empty>svg { color: var(--brand); opacity: .65; }
.favorites-empty>strong { color: var(--text); font-weight: 500; }
.favorites-empty>p { max-width: 400px; font-size: 12px; }
.favorite-compact { min-height: 27px; padding: 0 9px; font-size: 11px; }
.favorite-danger { color: var(--error); }
.favorite-danger:hover:not(:disabled) { color: var(--error); background: var(--error-bg); }
.favorites-table { flex: 1 1 0; min-height: 0; min-width: 0; overflow: auto; overscroll-behavior: contain; border-top: 1px solid var(--line); border-bottom: 1px solid var(--line); background: var(--panel); scrollbar-gutter: stable; }
.favorites-table table { width: 100%; min-width: 720px; table-layout: fixed; border-collapse: separate; border-spacing: 0; font-size: 13px; }
.favorite-col-ip { width: 23%; }
.favorite-col-folder { width: 14%; }
.favorite-col-region { width: 19%; }
.favorite-col-tcp { width: 13%; }
.favorite-col-speed { width: 14%; }
.favorite-col-actions { width: 17%; }
.favorites-table th { position: sticky; top: 0; z-index: 2; height: 32px; padding: 5px 20px; border-bottom: 1px solid var(--line); background: linear-gradient(var(--bg),color-mix(in srgb,var(--bg) 65%,var(--panel))); color: var(--muted); font-size: 11px; font-weight: 500; line-height: 21px; white-space: nowrap; }
.favorites-table th:nth-child(4), .favorites-table th:nth-child(5) { text-align: right; }
.favorites-table th:last-child { padding-inline: 8px; text-align: center; }
.favorite-data-row>td { height: 38px; padding: 3px 20px; border: 0; border-bottom: 1px solid var(--line-subtle); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; vertical-align: middle; line-height: 20px; }
.favorite-data-row.is-alt { background: color-mix(in srgb,var(--bg) 45%,var(--panel)); }
.favorite-data-row:hover, .favorite-data-row:focus-within, .favorite-data-row.is-expanded { background: color-mix(in srgb,var(--brand) 5%,var(--panel)); }
.favorite-data-row:hover>td:first-child, .favorite-data-row.is-expanded>td:first-child { box-shadow: inset 2px 0 0 color-mix(in srgb,var(--brand) 55%,var(--line)); }
.favorite-row-toggle { display: inline-flex; align-items: center; gap: 7px; max-width: 100%; min-height: 28px; padding: 0; border: 0; border-radius: 4px; background: transparent; color: var(--text); font-size: 13px; white-space: nowrap; }
.favorite-row-toggle:hover { color: var(--brand); }
.favorite-expand-icon { color: var(--muted); transition: transform .14s; }
.favorite-expand-icon.expanded { transform: rotate(90deg); color: var(--brand); }
.favorite-state-dot { width: 5px; height: 5px; flex: 0 0 auto; border-radius: 50%; background: var(--muted); opacity: .55; }
.favorite-state-dot.good { background: var(--good); opacity: 1; }
.favorite-state-dot.testing { background: var(--brand); opacity: 1; box-shadow: 0 0 0 2px var(--soft); }
.favorite-table-folder { display: inline-block; max-width: 100%; overflow: hidden; text-overflow: ellipsis; vertical-align: middle; padding: 0 5px; border: 1px solid var(--line-subtle); border-radius: 4px; color: var(--muted); background: var(--surface-sunken); font-size: 10px; line-height: 19px; }
.favorite-table-location { display: inline-flex; align-items: center; gap: 6px; max-width: 100%; font-size: 12px; }
.favorite-table-location>span:first-child { min-width: 0; overflow: hidden; text-overflow: ellipsis; }
.favorite-pop { flex: 0 0 auto; padding: 0 4px; border: 1px solid var(--line); border-radius: 4px; background: var(--surface-sunken); color: var(--muted); font-size: 10px; line-height: 17px; letter-spacing: .1px; }
.favorite-table-metric { display: flex; justify-content: flex-end; align-items: center; gap: 6px; font-variant-numeric: tabular-nums; }
.favorite-metric-label { color: var(--muted); font-size: 9px; }
.favorite-table-speed { display: block; text-align: right; font-variant-numeric: tabular-nums; }
.favorite-table-speed.measured { color: var(--brand); font-weight: 600; }
.favorite-data-row>td:last-child { min-width: 0; max-width: none; padding-inline: 8px; }
.favorite-row-actions { display: flex; justify-content: center; align-items: center; gap: 3px; }
.favorite-row-actions>.icon-button { width: 26px; height: 26px; min-height: 26px; padding: 6px; color: var(--muted); }
.favorite-row-actions>.icon-button:hover:not(:disabled) { color: var(--brand); background: var(--soft); }
.favorite-row-actions>.favorite-danger { color: var(--error); }
.favorite-row-actions>.favorite-danger:hover:not(:disabled) { color: var(--error); background: var(--error-bg); }
.favorite-heading-hint { margin-left: 5px; font-size: 9px; opacity: .8; }
.favorite-table-unit { font-size: 9px; margin-left: 2px; }
.favorite-table-live-note { color: var(--brand); font-size: 10px; }
.favorites-table .favorite-detail-row>td { height: auto; min-width: 0; max-width: none; padding: 0; border: 0; border-bottom: 1px solid var(--selected-border); overflow: visible; white-space: normal; vertical-align: top; }
.favorite-inline-detail { padding: 12px 20px 16px; background: color-mix(in srgb,var(--soft) 22%,var(--panel)); }
.favorite-detail-heading { display: flex; align-items: center; justify-content: space-between; gap: 12px; margin-bottom: 10px; }
.favorite-detail-heading>div { display: flex; align-items: center; flex-wrap: wrap; gap: 10px; min-width: 0; font-size: 11px; color: var(--muted); }
.favorite-detail-heading strong { color: var(--text); font-size: 12px; }
.favorite-detail-heading .text-button { flex: 0 0 auto; }
.favorite-collapse-icon { transform: rotate(180deg); }
.favorite-detail-status { font-size: 10px; color: var(--muted); }
.favorite-detail-status.passed { color: var(--good); }
.favorite-detail-grid { display: grid; grid-template-columns: repeat(2,minmax(0,1fr)); gap: 12px 20px; margin: 0; padding: 12px; border: 1px solid var(--line-subtle); border-radius: 6px; background: var(--surface-sunken); }
.favorite-detail-grid>div { min-width: 0; }
.favorite-detail-grid dt { color: var(--muted); font-size: 10px; }
.favorite-detail-grid dd { margin: 3px 0 0; overflow-wrap: anywhere; font-size: 11px; line-height: 19px; }
.favorite-full { grid-column: 1/-1; }
.favorite-note-full { white-space: pre-wrap; }
.favorite-probes { margin-top: 9px; font-size: 11px; }
.favorite-probes>summary { cursor: pointer; color: var(--brand); }
.favorite-probes>p { margin-top: 7px; padding: 9px; background: var(--surface-sunken); border-radius: 5px; overflow-wrap: anywhere; line-height: 19px; color: var(--muted); }
.favorite-modal { width: min(450px,calc(100vw - 32px)); max-height: calc(100vh - 40px); padding: 0; border: 1px solid var(--line-hover); border-radius: 11px; color: var(--text); background: var(--surface-raised); box-shadow: 0 20px 70px #1c204533; }
.favorite-modal::backdrop { background: #23243e42; backdrop-filter: blur(3px); }
.favorite-modal>form { display: flex; flex-direction: column; max-height: calc(100vh - 42px); }
.favorite-modal-heading { display: flex; justify-content: space-between; align-items: center; gap: 12px; flex: 0 0 auto; margin: 0; padding: 15px 16px 12px; border-bottom: 1px solid var(--line); background: linear-gradient(180deg,var(--surface-raised),var(--surface-sunken)); }
.favorite-modal-identity { display: flex; align-items: center; gap: 9px; min-width: 0; }
.favorite-modal-symbol { width: 28px; height: 28px; }
.favorite-modal-symbol.danger { color: var(--error); border-color: color-mix(in srgb,var(--error) 20%,var(--line)); background: var(--error-bg); }
.favorite-modal-heading h2 { font-size: 14px; line-height: 20px; }
.favorite-modal-heading p { margin-top: 1px; color: var(--muted); font-size: 10px; }
.favorite-modal-body { min-height: 0; overflow-y: auto; overscroll-behavior: contain; padding: 15px 16px; }
.favorite-modal-body .field { gap: 5px; }
.favorite-modal-body .field>span { font-size: 11px; line-height: 18px; }
.favorite-modal .favorite-modal-body input:not([type=checkbox]):not([type=radio]), .favorite-modal .favorite-modal-body select, .favorite-modal .favorite-modal-body textarea { font-size: 12px; }
.favorite-modal-body input, .favorite-modal-body select { background: var(--surface-raised); border-color: var(--line); box-shadow: var(--control-shadow); }
.favorites-panel :is(input,select,textarea):disabled, .favorite-modal :is(input,select,textarea):disabled { color: var(--muted); background: var(--surface-sunken); background-image: none; box-shadow: none; }
.favorite-modal .favorite-modal-body :is(input,select,textarea)[aria-invalid=true]:not(:disabled) { border-color: var(--error); }
.favorite-modal .favorite-modal-body :is(input,select,textarea)[aria-invalid=true]:not(:disabled):focus { border-color: var(--error); outline-color: var(--error); box-shadow: 0 0 0 2px color-mix(in srgb,var(--error) 18%,transparent); }
.favorite-modal-body textarea { min-height: 72px; max-height: 180px; padding: 8px 10px; font-family: inherit; background: var(--surface-sunken); }
.favorite-dialog-error { display: flex; align-items: flex-start; flex: 0 0 auto; gap: 7px; max-height: 100px; overflow: auto; margin: 0; padding: 8px 16px; border-top: 1px solid color-mix(in srgb,var(--error) 20%,var(--line)); color: var(--error); background: var(--error-bg); font-size: 11px; line-height: 18px; overflow-wrap: anywhere; }
.favorite-dialog-error>svg { margin-top: 2px; }
.favorite-dialog-error>span { min-width: 0; }
.favorite-modal-actions { display: flex; justify-content: flex-end; align-items: center; gap: 7px; flex: 0 0 auto; margin: 0; padding: 10px 16px; border-top: 1px solid var(--line); background: var(--surface-sunken); }
.favorite-modal-actions>.button { height: 32px; min-height: 32px; font-size: 12px; }
.favorite-modal-subtitle { display: inline-block; margin-bottom: 12px; padding: 4px 7px; color: var(--brand); background: var(--soft); border: 1px solid var(--selected-border); border-radius: 5px; font-size: 12px; }
.favorite-note-field { margin-top: 12px; }
.favorite-input-hint { margin-top: 4px; font-size: 10px; line-height: 17px; }
.favorite-existing-note { padding: 9px 10px; margin-top: 10px; background: var(--warn-bg); color: var(--warn); border: 1px solid color-mix(in srgb,var(--warn) 18%,var(--line)); border-radius: 6px; font-size: 10px; line-height: 18px; }
.favorite-manual-settings { margin-top: 12px; padding: 9px 10px; background: var(--soft); border: 1px solid var(--selected-border); border-radius: 6px; line-height: 18px; overflow-wrap: anywhere; }
.favorite-manual-settings .small { font-size: 10px; }
.favorite-manual-settings>p+p { margin-top: 3px; }
.favorite-context-message { min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; color: var(--muted); font-size: 10px; }
.favorite-context-message.is-error { color: var(--error); }
.favorite-table-footer { display: flex; flex: 0 0 30px; height: 30px; align-items: center; justify-content: space-between; gap: 12px; padding: 0 20px; background: var(--panel); color: var(--muted); font-size: 11px; }
.favorite-table-footer>span:last-child { font-size: 10px; }
.favorite-form-error { color: var(--error); font-size: 11px; margin-top: 7px; overflow-wrap: anywhere; }
.favorite-delete-copy { display: flex; align-items: flex-start; gap: 8px; padding: 11px; background: var(--warn-bg); border: 1px solid color-mix(in srgb,var(--warn) 20%,var(--line)); border-radius: 7px; font-size: 12px; line-height: 21px; overflow-wrap: anywhere; }
.favorite-delete-copy>svg { flex: 0 0 auto; margin-top: 2px; color: var(--warn); }
.favorite-delete-button { color: var(--error-ink); background: var(--error); border-color: var(--error); }
.favorite-delete-button:hover:not(:disabled) { color: var(--error-ink); background: var(--error); border-color: var(--error); }
@media (max-width: 900px) { .favorites-identity { min-width: 150px; } .favorites-symbol { display: none; } .favorite-search { flex: 1; width: auto; } }
@media (max-width: 800px) { .favorite-table-footer { padding-inline: 16px; } .favorite-folder-list, .favorite-context-row { padding-inline: 16px; } .favorite-test-job { margin-inline: 16px; } .favorites-table th, .favorite-data-row>td { padding-inline: 16px; } .favorite-data-row>td:nth-child(4), .favorite-data-row>td:nth-child(5) { padding-inline: 10px; } .favorite-data-row>td:last-child { padding-inline: 4px; } .favorite-row-toggle { gap: 5px; font-size: 12px; } .favorite-row-actions { gap: 0; } .favorite-row-actions>.icon-button { width: 24px; height: 26px; } .favorite-inline-detail { padding-inline: 16px; } .favorite-heading-hint { display: none; } .favorite-table-location { gap: 4px; font-size: 11px; } }
@media (max-width: 650px) { .favorites-identity { flex: 1; } .favorite-search { order: 3; flex: 1 1 100%; } .favorite-folder-hint { display: none; } .favorite-context-row { gap: 7px; } .favorite-table-live-note { max-width: 180px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; } }
@media (max-width: 440px) { .favorite-detail-grid { grid-template-columns: minmax(0,1fr); } .favorite-folder-actions>.text-button { font-size: 10px; } }
</style>
