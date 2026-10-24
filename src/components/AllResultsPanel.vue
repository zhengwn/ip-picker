<script setup lang="ts">
import { computed, nextTick, ref, watch } from 'vue'
import { invoke, isTauri } from '@tauri-apps/api/core'
import { Activity, ChevronDown, ChevronLeft, ChevronRight, Copy, Search, SearchX, SlidersHorizontal } from '@lucide/vue'
import { regionName, type IpResult, type ScanRun } from '../types'
import { completeMeasurement, completedTcpMetricValue, hasPrecisionMeasurement, stabilityMetricLabel, stabilityVerdict } from '../ui-state'
import { readResultPreferences, saveResultPreferences, type ResultSortBy } from '../result-preferences'
import { useDismissiblePopover } from '../popovers'
import FavoriteButton from './FavoriteButton.vue'
import SelectControl from './SelectControl.vue'

const props = defineProps<{ run: ScanRun; savedIps: Set<string>; favoriteDisabled: boolean }>()
const emit = defineEmits<{ favorite: [result: IpResult, event: MouseEvent]; notify: [text: string, error?: boolean] }>()
const desktop = isTauri()
const copyingIp = ref<string | null>(null)

async function copyIp(ip: string) {
  if (!desktop || copyingIp.value) return
  copyingIp.value = ip
  try { await invoke('copy_text', { text: ip }); emit('notify', `已复制 ${ip}`) }
  catch (error) { emit('notify', `复制失败：${error instanceof Error ? error.message : String(error)}`, true) }
  finally { copyingIp.value = null }
}

type ResultStatus = 'qualified' | 'measured' | 'rejected' | 'notDownloaded' | 'stability' | 'unfinished'
const query = ref('')
const regionFilter = ref('all')
const popFilter = ref('all')
const statusFilter = ref('all')
const preferences = readResultPreferences()
const sortBy = ref<ResultSortBy>(preferences.sortBy)
const completedDownloadOnly = ref(preferences.completedDownloadOnly)
const minDownloadMbps = ref<number | ''>('')
const maxTcpMs = ref<number | ''>('')
const moreFiltersOpen = ref(false)
const moreFiltersPanel = ref<HTMLElement | null>(null)
const moreFiltersTrigger = ref<HTMLButtonElement | null>(null)
useDismissiblePopover(moreFiltersOpen, moreFiltersPanel, moreFiltersTrigger)
const resultsTable = ref<HTMLElement | null>(null)
const page = ref(1)
const pageSize = 50
const metric = computed(() => props.run.config.stabilityLatencyMetric ?? 'max')
const metricName = computed(() => stabilityMetricLabel(metric.value))
const runStatus = computed(() => ({ running: '测试中', completed: '已完成', cancelled: '已停止', failed: '测试失败' })[props.run.status])
const testTime = computed(() => {
  const date = new Date(props.run.startedAt)
  return Number.isNaN(date.getTime()) ? '' : date.toLocaleString('zh-CN', { month: 'numeric', day: 'numeric', hour: '2-digit', minute: '2-digit', hour12: false })
})
const statusOptions: { value: ResultStatus; label: string }[] = [
  { value: 'qualified', label: '完整测速达标' },
  { value: 'measured', label: '完整测速未达标' },
  { value: 'rejected', label: '稳定性未达标／提前淘汰' },
  { value: 'notDownloaded', label: '稳定性已完成 · 未入选下载' },
  { value: 'stability', label: '稳定性达标 · 完整测速未通过' },
  { value: 'unfinished', label: '精测未完成' },
]

function status(result: IpResult): ResultStatus {
  if (completeMeasurement(result, props.run.config)) return result.eligible ? 'qualified' : 'measured'
  if (/未进入本地区前\s*\d+\s*名下载候选/.test(result.exclusionReason ?? '')) return 'notDownloaded'
  const verdict = stabilityVerdict(result, props.run.config)
  if (verdict === 'failed') return 'rejected'
  if (verdict === 'passed') return 'stability'
  return 'unfinished'
}
function location(code: string) { return /^[A-Z]{2}$/.test(code) ? regionName(code) : '未知地区' }
function firstPop(result: IpResult) { return result.colo || result.observedColos[0] || '' }
function locationCode(code: string) { return /^[A-Z]{2}$/.test(code) ? code : '?' }
function fixed(value: number | null) { return value != null && Number.isFinite(value) ? value.toFixed(1) : '—' }
function downloadValue(result: IpResult): number | null {
  return result.downloadSamplesMbps.length && Number.isFinite(result.downloadMbps) ? result.downloadMbps : null
}
function completeDownloadValue(result: IpResult): number | null {
  const value = downloadValue(result)
  return value != null && value > 0
    && result.downloadSamplesMbps.length >= props.run.config.downloadRounds
    && result.downloadSamplesMbps.every(sample => Number.isFinite(sample) && sample > 0)
    ? value : null
}
function initialTcp(result: IpResult): number | null {
  return Number.isFinite(result.tcpMs) && (result.tcpMs > 0 || result.attempts > 0 || !!result.colo) ? result.tcpMs : null
}
function compareNullable(a: number | null, b: number | null, descending = false) {
  if (a == null) return b == null ? 0 : 1
  if (b == null) return -1
  return descending ? b - a : a - b
}
function compareIps(a: string, b: string) { return a.localeCompare(b, undefined, { numeric: true }) }

const records = computed(() => props.run.results.filter(hasPrecisionMeasurement).map(result => ({ result, status: status(result) })))
const maxDownloadMbps = computed(() => records.value.reduce((maximum, record) => Math.max(maximum, downloadValue(record.result) ?? 0), 0))
function relativeDownloadWidth(result: IpResult) { return `${maxDownloadMbps.value > 0 ? Math.max(0, (downloadValue(result) ?? 0) / maxDownloadMbps.value * 100) : 0}%` }
function downloadDescription(result: IpResult) {
  const value = downloadValue(result)
  if (value == null) return '尚未取得下载样本'
  const completion = completeDownloadValue(result) != null ? '已完成下载测速' : `尚未取得完整有效的下载结果：已记录 ${result.downloadSamplesMbps.length} / ${props.run.config.downloadRounds} 轮样本`
  return `${completion}，${fixed(value)} Mbps；横条相对于本轮最高已测速率 ${fixed(maxDownloadMbps.value)} Mbps`
}
const completeCount = computed(() => records.value.filter(record => ['qualified', 'measured'].includes(record.status)).length)
const downloadedCount = computed(() => records.value.filter(record => completeDownloadValue(record.result) != null).length)
const statusCounts = computed(() => Object.fromEntries(statusOptions.map(option => [option.value, records.value.filter(record => record.status === option.value).length])))
const regionOptions = computed(() => [...new Set(records.value.map(record => record.result.regionCode))]
  .sort((a, b) => location(a).localeCompare(location(b), 'zh-CN')))
const popOptions = computed(() => [...new Set(records.value
  .filter(record => regionFilter.value === 'all' || record.result.regionCode === regionFilter.value)
  .map(record => firstPop(record.result))
  .filter(Boolean))].sort())
const filtered = computed(() => {
  const search = query.value.trim().toLowerCase()
  return records.value.filter(record => {
    const result = record.result
    const download = completeDownloadValue(result)
    const tcp = completedTcpMetricValue(result, metric.value)
    return (regionFilter.value === 'all' || result.regionCode === regionFilter.value)
      && (popFilter.value === 'all' || firstPop(result) === popFilter.value)
      && (statusFilter.value === 'all' || record.status === statusFilter.value)
      && (!completedDownloadOnly.value || download != null)
      && (minDownloadMbps.value === '' || (download != null && download >= minDownloadMbps.value))
      && (maxTcpMs.value === '' || (tcp != null && tcp <= maxTcpMs.value))
      && (!search || [result.ip, result.colo, result.city, result.regionCode, location(result.regionCode), ...result.observedColos].some(value => value.toLowerCase().includes(search)))
  }).sort((a, b) => {
    const first = a.result
    const second = b.result
    const difference = sortBy.value === 'tcp'
      ? compareNullable(completedTcpMetricValue(first, metric.value), completedTcpMetricValue(second, metric.value))
      : sortBy.value === 'initialTcp'
        ? compareNullable(initialTcp(first), initialTcp(second))
        : sortBy.value === 'download'
          ? compareNullable(downloadValue(first), downloadValue(second), true)
          : 0
    return difference || compareIps(first.ip, second.ip)
  })
})
const pageCount = computed(() => Math.max(1, Math.ceil(filtered.value.length / pageSize)))
const currentPage = computed(() => Math.min(page.value, pageCount.value))
const firstRow = computed(() => (currentPage.value - 1) * pageSize)
const visibleRows = computed(() => filtered.value.slice(firstRow.value, firstRow.value + pageSize))
const transientFilterCount = computed(() => [!!query.value.trim(), regionFilter.value !== 'all', popFilter.value !== 'all', statusFilter.value !== 'all', minDownloadMbps.value !== '', maxTcpMs.value !== ''].filter(Boolean).length)
const moreFilterCount = computed(() => [statusFilter.value !== 'all', minDownloadMbps.value !== '', maxTcpMs.value !== ''].filter(Boolean).length)
const hasFilters = computed(() => transientFilterCount.value > 0 || completedDownloadOnly.value)
const emptyTitle = computed(() => {
  if (records.value.length) return completedDownloadOnly.value && !downloadedCount.value && !transientFilterCount.value ? (props.run.status === 'running' ? '下载结果正在路上' : '本轮没有完成下载测速的 IP') : '没有符合条件的结果'
  return props.run.status === 'running' ? '正在寻找可精测的 IP' : '本轮尚未取得精测结果'
})
const emptyDescription = computed(() => {
  if (records.value.length) return completedDownloadOnly.value && !downloadedCount.value && !transientFilterCount.value ? '已取得的稳定性记录仍然保留，可关闭下载筛选查看。' : '试试放宽地区、POP 或指标限制，查看其他已测结果。'
  return props.run.status === 'running' ? '完成连通性检查与地区识别后，精测数据会在这里实时出现。' : props.run.status === 'cancelled' ? '测试在精测开始前已停止，可以调整设置后重新测试。' : '可以调整目标地区、测试地址或连接超时后重新测试。'
})
function resetTransientFilters() { query.value = ''; regionFilter.value = 'all'; popFilter.value = 'all'; statusFilter.value = 'all'; minDownloadMbps.value = ''; maxTcpMs.value = '' }

watch([completedDownloadOnly, sortBy], ([completedDownloadOnly, sortBy]) => {
  saveResultPreferences({ completedDownloadOnly, sortBy })
}, { immediate: true, flush: 'sync' })
watch(popOptions, options => {
  if (popFilter.value !== 'all' && !options.includes(popFilter.value)) popFilter.value = 'all'
})
async function resetTableScroll() { await nextTick(); if (resultsTable.value) resultsTable.value.scrollTop = 0 }
watch([query, regionFilter, popFilter, statusFilter, sortBy, completedDownloadOnly, minDownloadMbps, maxTcpMs], () => { page.value = 1; void resetTableScroll() })
watch(currentPage, () => { void resetTableScroll() })
watch(() => props.run.id, () => { resetTransientFilters(); page.value = 1 })
</script>

<template>
  <section class="all-results-panel" aria-labelledby="all-results-title">
    <div class="all-results-controls">
    <div class="all-results-heading">
      <h2 id="all-results-title">测试结果</h2>
      <span class="status-label" :class="{ active: run.status === 'running', good: run.status === 'completed', quiet: run.status === 'cancelled', error: run.status === 'failed' }" role="status" aria-live="polite">{{ runStatus }}</span>
      <span class="results-summary muted small" :title="`完整测速通过 ${completeCount} 个 IP`"><span>精测 <strong>{{ records.length }}</strong></span><span class="summary-divider" aria-hidden="true" /><span>完成下载 <strong>{{ downloadedCount }}</strong></span></span>
      <time v-if="testTime" class="results-test-time muted small" :datetime="run.startedAt">{{ testTime }}</time>
    </div>

    <div class="all-results-filters">
      <label class="field results-search"><span class="visually-hidden">搜索 IP / POP</span><span class="results-search-control"><Search :size="14" aria-hidden="true" /><input v-model="query" type="search" placeholder="搜索 IP、POP 或地区" aria-label="搜索 IP / POP" /></span></label>
      <label class="field"><span class="visually-hidden">地区</span><SelectControl compact><select v-model="regionFilter" aria-label="地区"><option value="all">全部地区</option><option v-for="code in regionOptions" :key="code" :value="code">{{ location(code) }}{{ /^[A-Z]{2}$/.test(code) ? ` · ${code}` : '' }}</option></select></SelectControl></label>
      <label class="field"><span class="visually-hidden">POP</span><SelectControl compact><select v-model="popFilter" aria-label="POP"><option value="all">全部 POP</option><option v-for="code in popOptions" :key="code" :value="code">{{ code }}</option></select></SelectControl></label>
      <label class="field results-sort"><span class="visually-hidden">排序</span><SelectControl compact><select v-model="sortBy" aria-label="排序"><option value="tcp">TCP {{ metricName }} 从低到高</option><option value="download">下载速度从高到低</option><option value="initialTcp">TCP 初筛从低到高</option><option value="ip">IP 地址排序</option></select></SelectControl></label>
      <button ref="moreFiltersTrigger" class="button more-filters-button" :class="{ 'is-active': moreFiltersOpen || moreFilterCount }" :aria-expanded="moreFiltersOpen" aria-controls="additional-result-filters" @click="moreFiltersOpen = !moreFiltersOpen"><SlidersHorizontal :size="13" />更多筛选<span v-if="moreFilterCount" class="filter-count">{{ moreFilterCount }}</span><ChevronDown :size="12" :class="{ 'is-open': moreFiltersOpen }" /></button>
    </div>
    <div class="results-filter-toolbar">
      <label class="check-field"><input v-model="completedDownloadOnly" type="checkbox" aria-describedby="completed-download-help" /><span>仅完成下载</span></label>
      <span id="completed-download-help" class="visually-hidden">只显示已测满 {{ run.config.downloadRounds }} 轮下载的 IP；此选择会自动保存。</span>
      <span v-if="transientFilterCount" class="muted small">已应用 {{ transientFilterCount }} 项筛选</span>
      <button v-if="transientFilterCount" class="text-button" @click="resetTransientFilters">清除筛选</button>
    </div>
    <div v-if="moreFiltersOpen" id="additional-result-filters" ref="moreFiltersPanel" class="additional-result-filters">
      <div class="all-results-thresholds">
        <label class="field"><span>测试状态</span><SelectControl compact><select v-model="statusFilter" aria-label="测试状态"><option value="all">全部状态 · {{ records.length }}</option><option v-for="option in statusOptions" :key="option.value" :value="option.value">{{ option.label }} · {{ statusCounts[option.value] }}</option></select></SelectControl></label>
        <label class="field"><span>最低下载速度（Mbps）</span><input v-model.number="minDownloadMbps" type="number" min="0" step="any" placeholder="不限" aria-describedby="results-threshold-help" /></label>
        <label class="field"><span>最高 TCP {{ metricName }}（ms）</span><input v-model.number="maxTcpMs" type="number" min="0" step="any" placeholder="不限" aria-describedby="results-threshold-help" /></label>
      </div>
      <p id="results-threshold-help" class="muted small">留空表示不限；指标筛选仅匹配完整的下载或 TCP 稳定性测量。</p>
    </div>
    </div>

    <div v-if="visibleRows.length" ref="resultsTable" class="all-results-table" role="region" aria-label="全部精测结果表格，可滚动" tabindex="0">
      <table>
        <colgroup><col class="ip-column" /><col class="location-column" /><col class="tcp-column" /><col class="download-column" /><col class="favorite-column" /></colgroup>
        <thead><tr><th scope="col">IP 地址</th><th scope="col">地区 / POP</th><th scope="col" :title="`TCP ${metricName}延迟`">TCP {{ metricName }} <span class="table-unit">ms</span></th><th scope="col">下载 <span class="table-unit">Mbps</span></th><th scope="col">收藏</th></tr></thead>
        <tbody>
          <tr v-for="record in visibleRows" :key="record.result.ip">
            <td><span class="result-ip-cell"><span class="mono result-ip">{{ record.result.ip }}</span><button class="icon-button result-copy" :disabled="!desktop || copyingIp !== null" :aria-label="`复制 IP ${record.result.ip}`" :title="desktop ? '复制 IP 地址' : '请在桌面应用中复制'" @click="copyIp(record.result.ip)"><Copy :size="13" aria-hidden="true" /></button></span></td>
            <td :title="record.result.observedColos.length > 1 ? `本次观测 POP：${record.result.observedColos.join(' / ')}；显示首次观测` : undefined"><span class="location-cell"><span class="region-badge" :title="record.result.regionCode">{{ locationCode(record.result.regionCode) }}</span><span class="location-name">{{ location(record.result.regionCode) }}</span><span v-if="firstPop(record.result)" class="result-pop">{{ firstPop(record.result) }}</span></span></td>
            <td><span class="result-number">{{ fixed(completedTcpMetricValue(record.result, metric)) }}</span></td>
            <td><span class="speed-cell" :class="{ 'is-partial': downloadValue(record.result) != null && completeDownloadValue(record.result) == null }" :title="downloadDescription(record.result)"><span v-if="downloadValue(record.result) != null && maxDownloadMbps > 0" class="speed-meter" aria-hidden="true"><span :style="{ width: relativeDownloadWidth(record.result) }" /></span><span class="result-number" :class="{ 'result-speed': completeDownloadValue(record.result) != null }">{{ fixed(downloadValue(record.result)) }}</span></span></td>
            <td><FavoriteButton :ip="record.result.ip" :saved="savedIps.has(record.result.ip)" :disabled="favoriteDisabled" compact @click="$emit('favorite', record.result, $event)" /></td>
          </tr>
        </tbody>
      </table>
    </div>
    <div v-else class="all-results-empty"><span class="results-empty-icon"><Activity v-if="run.status === 'running' && !records.length" :size="24" /><SearchX v-else :size="24" /></span><h3>{{ emptyTitle }}</h3><p>{{ emptyDescription }}</p><div v-if="records.length" class="results-empty-actions"><button v-if="transientFilterCount" class="button" @click="resetTransientFilters">清除筛选</button><button v-if="completedDownloadOnly" class="text-button" @click="completedDownloadOnly = false">查看含未完成下载的结果<ChevronRight :size="14" /></button></div></div>
    <div class="all-results-footer">
      <span class="muted small" role="status" aria-live="polite">{{ hasFilters ? `显示 ${filtered.length} / ${records.length} 个结果` : `共 ${records.length} 个结果` }}{{ filtered.length > pageSize ? ` · 当前 ${firstRow + 1}–${firstRow + visibleRows.length}` : '' }}</span>
      <nav v-if="pageCount > 1" class="all-results-pagination" aria-label="精测结果分页"><span class="muted small">{{ currentPage }} / {{ pageCount }} 页</span><button class="icon-button" :disabled="currentPage <= 1" aria-label="上一页" @click="page = currentPage - 1"><ChevronLeft :size="15" aria-hidden="true" /></button><button class="icon-button" :disabled="currentPage >= pageCount" aria-label="下一页" @click="page = currentPage + 1"><ChevronRight :size="15" aria-hidden="true" /></button></nav>
    </div>
  </section>
</template>

<style scoped>
.visually-hidden { position: absolute; width: 1px; height: 1px; overflow: hidden; clip: rect(0,0,0,0); white-space: nowrap; }
.all-results-panel { display: flex; flex: 1 1 0; flex-direction: column; min-width: 0; min-height: 0; margin: 0; padding: 0; overflow: hidden; border: 0; border-radius: 0; box-shadow: none; background: var(--panel); }
.all-results-controls { position: relative; z-index: 3; display: flex; flex-direction: column; flex: 0 0 calc(var(--workspace-controls-height,208px) - var(--scan-controls-height,102px)); height: calc(var(--workspace-controls-height,208px) - var(--scan-controls-height,102px)); min-height: 0; overflow: visible; }
.all-results-heading { display: flex; flex: 0 0 42px; align-items: center; gap: 10px; height: 42px; min-height: 0; padding: 0 20px; }
.all-results-heading h2 { display: inline-flex; align-items: center; gap: 8px; font-size: 14px; line-height: 20px; font-weight: 600; letter-spacing: -.15px; white-space: nowrap; }
.all-results-heading h2::before { content: ""; width: 3px; height: 13px; border-radius: 2px; background: var(--brand); opacity: .8; }
.all-results-heading .status-label { min-height: 20px; padding: 0 7px; font-size: 11px; line-height: 20px; border-radius: 5px; border: 1px solid color-mix(in srgb, currentColor 12%, transparent); }
.results-summary { display: inline-flex; align-items: center; gap: 9px; font-size: 11px; font-variant-numeric: tabular-nums; white-space: nowrap; }
.results-summary strong { margin-left: 2px; color: var(--text); font-size: 11px; font-weight: 600; }
.summary-divider { width: 1px; height: 10px; background: var(--line); }
.results-test-time { margin-left: auto; padding-left: 10px; border-left: 1px solid var(--line); font-size: 11px; line-height: 12px; font-variant-numeric: tabular-nums; white-space: nowrap; }
.all-results-filters { display: grid; flex: 0 0 auto; grid-template-columns: minmax(160px,1.7fr) minmax(108px,1fr) minmax(90px,.8fr) minmax(160px,1.15fr) auto; gap: 8px; padding: 0 20px; }
.all-results-filters .field { gap: 0; }
.all-results-filters :deep(select), .all-results-filters input[type=search], .more-filters-button { border-color: color-mix(in srgb, var(--line) 88%, var(--muted)); background: linear-gradient(var(--panel), color-mix(in srgb, var(--bg) 38%, var(--panel))); box-shadow: 0 1px 1px color-mix(in srgb, var(--text) 3%, transparent); transition: border-color .15s, box-shadow .15s, background .15s; }
.all-results-filters :deep(select:hover:not(:disabled)), .all-results-filters input[type=search]:hover { border-color: var(--line-hover); }
.all-results-filters :deep(select:focus-visible), .all-results-filters input[type=search]:focus-visible, .more-filters-button:focus-visible { outline: 2px solid color-mix(in srgb, var(--brand) 55%, transparent); outline-offset: 2px; border-color: var(--brand); box-shadow: 0 0 0 1px var(--panel); }
.all-results-filters :deep(.select-control>select), .additional-result-filters :deep(.select-control>select), .all-results-filters input[type=search], .additional-result-filters input[type=number] { height: 30px; min-height: 30px; font-size: 12px; border-radius: 5px; }
.results-search-control { position: relative; display: block; }
.results-search-control>svg { position: absolute; left: 9px; top: 50%; transform: translateY(-50%); color: var(--muted); pointer-events: none; }
.results-search-control>input[type=search] { padding-left: 29px; }
.results-search-control:focus-within>svg { color: var(--brand); }
.results-filter-toolbar { display: flex; flex: 1 1 0; align-items: center; gap: 12px; min-height: 0; overflow: hidden; padding: 0 20px; }
.results-filter-toolbar .check-field { min-height: 28px; font-size: 12px; transition: color .15s; }
.results-filter-toolbar .check-field:hover { color: var(--brand); }
.results-filter-toolbar .check-field input { width: 14px; height: 14px; }
.results-filter-toolbar .text-button { min-height: 26px; padding: 0; font-size: 11px; }
.more-filters-button { min-height: 30px; gap: 5px; padding-inline: 8px; font-size: 12px; border-radius: 5px; }
.more-filters-button.is-active { background: var(--soft); border-color: var(--selected-border); color: var(--brand); box-shadow: inset 0 1px 0 color-mix(in srgb, var(--panel) 70%, transparent); }
.more-filters-button>svg:last-child { transition: transform .15s; }
.more-filters-button>svg.is-open { transform: rotate(180deg); }
.filter-count { display: inline-grid; place-items: center; min-width: 16px; height: 16px; padding-inline: 3px; border-radius: 4px; background: var(--soft); color: var(--brand); font-size: 10px; font-variant-numeric: tabular-nums; }
.additional-result-filters { position: absolute; top: calc(100% - 1px); right: 20px; z-index: 10; width: min(600px,calc(100% - 40px)); max-height: 40vh; margin: 0; padding: 12px; overflow: auto; overscroll-behavior: contain; border: 1px solid var(--line); border-radius: 8px; background: var(--surface-raised); box-shadow: 0 10px 28px #2328491c, 0 2px 6px #2328490a; }
.all-results-thresholds { display: grid; grid-template-columns: minmax(160px,1.3fr) repeat(2,minmax(110px,1fr)); gap: 12px; }
.all-results-thresholds .field { gap: 4px; }
.all-results-thresholds .field>span { font-size: 11px; line-height: 16px; }
.additional-result-filters>p { margin-top: 7px; font-size: 11px; }
.all-results-table { flex: 1 1 0; min-width: 0; min-height: 0; max-width: 100%; overflow: auto; overscroll-behavior: contain; border-top: 1px solid var(--line); border-bottom: 1px solid var(--line); scrollbar-gutter: stable; }
.all-results-table table { width: 100%; min-width: 580px; table-layout: fixed; border-collapse: separate; border-spacing: 0; font-size: 13px; text-align: left; }
.ip-column { width: 35%; }
.location-column { width: 25%; }
.tcp-column, .download-column { width: 16%; }
.favorite-column { width: 8%; }
.all-results-table th { position: sticky; top: 0; z-index: 1; height: 32px; padding: 5px 20px; border-bottom: 1px solid var(--line); background: linear-gradient(var(--bg), color-mix(in srgb, var(--bg) 65%, var(--panel))); color: var(--muted); font-size: 11px; line-height: 21px; font-weight: 500; white-space: nowrap; box-shadow: 0 1px 0 color-mix(in srgb, var(--panel) 70%, transparent); }
.all-results-table td { height: 38px; padding: 3px 20px; border: 0; border-bottom: 1px solid var(--line); line-height: 20px; vertical-align: middle; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.all-results-table tbody tr:last-child td { border-bottom: 0; }
.all-results-table tbody tr:nth-child(even) { background: color-mix(in srgb,var(--bg) 45%,var(--panel)); }
.all-results-table tbody tr { transition: background .12s; }
.all-results-table tbody tr:hover, .all-results-table tbody tr:focus-within { background: color-mix(in srgb, var(--brand) 5%, var(--panel)); }
.all-results-table tbody tr:hover td:first-child, .all-results-table tbody tr:focus-within td:first-child { box-shadow: inset 2px 0 0 color-mix(in srgb, var(--brand) 55%, var(--line)); }
.all-results-table th:nth-child(3), .all-results-table td:nth-child(3), .all-results-table th:nth-child(4), .all-results-table td:nth-child(4) { text-align: right; }
.all-results-table th:last-child, .all-results-table td:last-child { min-width: 0; max-width: none; padding-inline: 4px; text-align: center; white-space: nowrap; }
.table-unit { display: inline-block; height: 15px; padding: 0 4px; margin-left: 3px; border: 1px solid color-mix(in srgb, var(--line) 85%, transparent); border-radius: 4px; background: color-mix(in srgb, var(--panel) 70%, transparent); font-weight: 400; font-size: 9px; line-height: 13px; vertical-align: 1px; }
.result-ip { font-size: 13px; font-weight: 500; }
.result-ip-cell { display: inline-flex; align-items: center; gap: 8px; max-width: 100%; }
.result-copy { width: 24px; height: 24px; min-height: 24px; padding: 5px; color: var(--muted); opacity: 0; transition: opacity .12s, background .12s; }
.result-copy:disabled { opacity: 0; }
.result-copy>svg { width: 13px; height: 13px; }
.all-results-table tbody tr:hover .result-copy, .all-results-table tbody tr:focus-within .result-copy, .result-copy:focus-visible { opacity: 1; }
.result-copy:hover:not(:disabled) { color: var(--brand); background: color-mix(in srgb, var(--brand) 9%, transparent); }
.result-copy:focus-visible { outline: 2px solid color-mix(in srgb, var(--brand) 65%, transparent); outline-offset: 1px; }
.location-cell { display: inline-flex; align-items: center; gap: 7px; max-width: 100%; vertical-align: middle; }
.region-badge { display: inline-grid; flex: 0 0 20px; place-items: center; width: 20px; height: 20px; border: 1px solid color-mix(in srgb, var(--brand) 14%, var(--line)); border-radius: 6px; background: color-mix(in srgb, var(--brand) 5%, var(--panel)); color: color-mix(in srgb, var(--brand) 75%, var(--muted)); font-size: 10px; line-height: 1; font-weight: 650; letter-spacing: -.2px; }
.location-name { min-width: 0; overflow: hidden; text-overflow: ellipsis; }
.result-pop { display: inline-block; flex: 0 0 auto; height: 17px; padding: 0 4px; border: 1px solid color-mix(in srgb, var(--line) 75%, transparent); border-radius: 4px; color: var(--muted); background: color-mix(in srgb, var(--bg) 70%, var(--panel)); font-size: 10px; font-weight: 500; line-height: 15px; letter-spacing: .2px; }
.result-number { font-variant-numeric: tabular-nums; }
.result-speed { color: var(--brand); font-weight: 600; }
.speed-cell { display: flex; align-items: center; justify-content: flex-end; gap: 8px; min-width: 0; }
.speed-cell>.result-number { flex: 0 0 auto; min-width: 5ch; text-align: right; }
.speed-meter { display: block; flex: 0 1 42px; min-width: 14px; height: 3px; overflow: hidden; border-radius: 2px; background: color-mix(in srgb, var(--brand) 9%, var(--line)); }
.speed-meter>span { display: block; height: 100%; border-radius: inherit; background: color-mix(in srgb, var(--brand) 53%, var(--muted)); }
.speed-cell.is-partial .speed-meter>span { background: color-mix(in srgb, var(--muted) 50%, var(--line)); }
.speed-cell.is-partial .result-number { color: var(--muted); }
.all-results-table tbody tr:hover .speed-meter>span { background: color-mix(in srgb, var(--brand) 70%, var(--muted)); }
.all-results-table tbody tr:hover .speed-cell.is-partial .speed-meter>span { background: color-mix(in srgb, var(--muted) 60%, var(--line)); }
.all-results-empty { display: flex; flex: 1 1 0; flex-direction: column; align-items: center; justify-content: center; min-height: 0; padding: 24px 16px; overflow: auto; border-top: 1px solid var(--line); border-bottom: 1px solid var(--line); text-align: center; }
.results-empty-icon { color: var(--muted); margin-bottom: 12px; }
.all-results-empty h3 { font-size: 14px; }
.all-results-empty p { max-width: 360px; margin-top: 7px; color: var(--muted); font-size: 12px; line-height: 1.8; }
.results-empty-actions { display: flex; align-items: center; flex-wrap: wrap; justify-content: center; gap: 12px; margin-top: 12px; }
.results-empty-actions .button { min-height: 30px; font-size: 12px; }
.all-results-footer { display: flex; flex: 0 0 30px; align-items: center; justify-content: space-between; gap: 12px; height: 30px; min-height: 0; padding: 0 20px; }
.all-results-footer>span { font-size: 11px; }
.all-results-pagination { display: flex; align-items: center; gap: 6px; }
.all-results-pagination .icon-button { width: 26px; height: 26px; min-height: 26px; border: 1px solid transparent; border-radius: 5px; }
.all-results-pagination .icon-button:hover:not(:disabled) { color: var(--brand); border-color: var(--line); background: var(--soft); }
@media (hover: none) { .result-copy, .result-copy:disabled { opacity: .7; } }
@media (max-width: 800px) {
  .all-results-heading { gap: 8px; }
  .all-results-heading, .all-results-filters, .results-filter-toolbar, .all-results-footer { padding-inline: 16px; }
  .additional-result-filters { right: 16px; width: min(600px,calc(100% - 32px)); }
  .all-results-filters { grid-template-columns: minmax(140px,1.3fr) minmax(98px,.9fr) minmax(82px,.7fr) minmax(152px,1.2fr) auto; gap: 6px; }
  .all-results-filters :deep(.select-control>select), .all-results-filters input[type=search] { font-size: 11px; }
  .more-filters-button { gap: 4px; font-size: 11px; padding-inline: 6px; }
  .more-filters-button>svg:first-child { display: none; }
  .all-results-table th, .all-results-table td { padding-inline: 16px; }
  .location-cell { gap: 5px; }
  .result-pop { padding-inline: 3px; }
  .speed-cell { gap: 6px; }
  .ip-column { width: 33%; }
  .location-column { width: 25%; }
  .tcp-column, .download-column { width: 17%; }
}
@media (max-width: 640px) {
  .all-results-filters { grid-template-columns: minmax(130px,1.4fr) minmax(85px,1fr) minmax(80px,.8fr); }
  .results-sort { grid-column: span 2; }
  .results-test-time { display: none; }
  .all-results-thresholds { grid-template-columns: minmax(160px,1.3fr) repeat(2,minmax(110px,1fr)); min-width: 440px; }
}
</style>
