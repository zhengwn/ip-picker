<script setup lang="ts">
import { computed, nextTick, ref, watch } from 'vue'
import { invoke, isTauri } from '@tauri-apps/api/core'
import { Activity, BarChart3, ChevronDown, ChevronLeft, ChevronRight, Copy, Search, SearchX, SlidersHorizontal } from '@lucide/vue'
import { regionName, type IpResult, type ScanRun } from '../types'
import { candidateExclusionReason, candidateMeasurementStatus, completeMeasurement, completedTcpMetricValue, hasCompleteDownload, hasPrecisionMeasurement, initialTcpFailureReason, isDownloadShortlistExclusion, observedFailurePercent, observedTcpMs, quickRetestAdvice, regionalPrecisionLimit, stabilityMetricLabel, stabilityVerdict, summarizeExclusions } from '../ui-state'
import type { ResultSortBy, TcpDisplayMetric } from '../result-preferences'
import { displayForLatencySort, latencySortForDisplay } from '../result-preferences'
import { describeResultRow, tcpLatencyChartScale } from '../result-row-display'
import { summarizeScanResults } from '../scan-result-summary'
import { scanModeLabel } from '../scan-experience'
import { useDismissiblePopover } from '../popovers'
import FavoriteButton from './FavoriteButton.vue'
import SelectControl from './SelectControl.vue'
import TcpDiagnostics from './TcpDiagnostics.vue'
import TcpLatencyCell from './TcpLatencyCell.vue'
import TcpLatencyHeader from './TcpLatencyHeader.vue'

type ResultAction = 'regions' | 'budget' | 'stability' | 'target' | 'expand'
const props = defineProps<{ run: ScanRun; savedIps: Set<string>; favoriteDisabled: boolean; settingsDiffer?: boolean; settingsDisabled?: boolean; retestDisabled?: boolean }>()
const emit = defineEmits<{ favorite: [result: IpResult, event: MouseEvent]; notify: [text: string, error?: boolean]; action: [action: ResultAction]; 'reuse-config': []; retest: [ip: string] }>()
const desktop = isTauri()
const copyingIp = ref<string | null>(null)

async function copyIp(ip: string) {
  if (!desktop || copyingIp.value) return
  copyingIp.value = ip
  try { await invoke('copy_text', { text: ip }); emit('notify', `已复制 ${ip}`) }
  catch (error) { emit('notify', `复制失败：${error instanceof Error ? error.message : String(error)}`, true) }
  finally { copyingIp.value = null }
}

type ResultStatus = 'qualified' | 'measured' | 'pendingSummary' | 'retest' | 'rejected' | 'notDownloaded' | 'stability' | 'unfinished'
const query = ref('')
const regionFilter = ref('all')
const popFilter = ref('all')
const statusFilter = ref('all')
const sortBy = defineModel<ResultSortBy>('sortBy', { default: 'tcp' })
const completedDownloadOnly = defineModel<boolean>('completedDownloadOnly', { default: true })
const tcpDisplayMetric = defineModel<TcpDisplayMetric>('tcpDisplayMetric', { default: 'runMetric' })
// A latency-ordered table must be ordered by the latency actually on display.
watch(tcpDisplayMetric, display => {
  if (displayForLatencySort[sortBy.value]) sortBy.value = latencySortForDisplay[display]
}, { flush: 'sync' })
watch(sortBy, order => {
  const display = displayForLatencySort[order]
  if (display) tcpDisplayMetric.value = display
}, { flush: 'sync' })
const observationsOpen = ref(false)
const initialIssuesOpen = ref(false)
const initialIssueQuery = ref('')
const initialIssueSearchInput = ref<HTMLInputElement | null>(null)
const minDownloadMbps = ref<number | ''>('')
const maxTcpMs = ref<number | ''>('')
const moreFiltersOpen = ref(false)
const moreFiltersPanel = ref<HTMLElement | null>(null)
const moreFiltersTrigger = ref<HTMLButtonElement | null>(null)
const statusSelect = ref<HTMLSelectElement | null>(null)
useDismissiblePopover(moreFiltersOpen, moreFiltersPanel, moreFiltersTrigger)
const summaryOpen = ref(false)
const summaryPanel = ref<HTMLElement | null>(null)
const summaryTrigger = ref<HTMLButtonElement | null>(null)
useDismissiblePopover(summaryOpen, summaryPanel, summaryTrigger)
const resultsTable = ref<HTMLElement | null>(null)
const expandedIp = ref<string | null>(null)
const page = ref(1)
const pageSize = 50
const ipOrder = new Intl.Collator(undefined, { numeric: true })
const failureRateFormat = new Intl.NumberFormat('zh-CN', { maximumFractionDigits: 1 })
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
  { value: 'pendingSummary', label: '测速完成 · 待汇总' },
  { value: 'retest', label: '建议完整复测' },
  { value: 'rejected', label: '稳定性未达标／提前淘汰' },
  { value: 'notDownloaded', label: '稳定性已完成 · 未入选下载' },
  { value: 'stability', label: '稳定性达标 · 完整测速未通过' },
  { value: 'unfinished', label: '精测未完成' },
]
const visibleStatusOptions = computed(() => statusOptions.filter(option =>
  (option.value !== 'pendingSummary' || props.run.status === 'running') &&
  (option.value !== 'retest' || retestCount.value > 0)))

function status(result: IpResult): ResultStatus {
  if (completeMeasurement(result, props.run.config)) {
    // The engine assigns eligibility when it ranks the finished run.
    if (props.run.status === 'running') return 'pendingSummary'
    return result.eligible ? 'qualified' : 'measured'
  }
  if (isDownloadShortlistExclusion(result)) return 'notDownloaded'
  if (quickRetestAdvice(result, props.run)) return 'retest'
  const verdict = stabilityVerdict(result, props.run.config)
  if (verdict === 'failed') return 'rejected'
  if (verdict === 'passed') return 'stability'
  return 'unfinished'
}
function location(code: string) { return /^[A-Z]{2}$/.test(code) ? regionName(code) : '未知地区' }
function firstPop(result: IpResult) { return result.colo || result.observedColos[0] || '' }
function fixed(value: number | null) { return value != null && Number.isFinite(value) ? value.toFixed(1) : '—' }
function downloadValue(result: IpResult): number | null {
  return result.downloadSamplesMbps.length && Number.isFinite(result.downloadMbps) ? result.downloadMbps : null
}
function completeDownloadValue(result: IpResult): number | null {
  return hasCompleteDownload(result, props.run.config) ? result.downloadMbps : null
}
function initialTcp(result: IpResult): number | null {
  return Number.isFinite(result.tcpMs) && (result.tcpMs > 0 || result.attempts > 0 || !!result.colo) ? result.tcpMs : null
}
function hasInitialTcpIssue(result: IpResult) {
  return !!initialTcpFailureReason(result) || result.exclusionCode === 'tcpLatency'
    || (!result.exclusionCode && /^TCP 延迟 .*超过上限/.test(result.exclusionReason ?? ''))
}
function compareNullable(a: number | null, b: number | null, descending = false) {
  if (a == null) return b == null ? 0 : 1
  if (b == null) return -1
  return descending ? b - a : a - b
}

function record(result: IpResult) {
  const tcp = completedTcpMetricValue(result, metric.value)
  const resultStatus = status(result)
  return {
    result,
    status: resultStatus,
    download: completeDownloadValue(result),
    tcp,
    tcpFailurePercent: observedFailurePercent(result.tcpStability),
    reason: candidateExclusionReason(result) || result.tcpStability?.earlyRejection,
    display: describeResultRow(result, props.run, tcp, initialTcp(result), resultStatus),
  }
}
function sortValue(entry: ReturnType<typeof record>, order: ResultSortBy): number | null {
  switch (order) {
    case 'tcp': return entry.tcp
    case 'medianTcp': return observedTcpMs(entry.result.tcpStability, 'medianMs')
    case 'averageTcp': return observedTcpMs(entry.result.tcpStability, 'averageMs')
    case 'peakTcp': return observedTcpMs(entry.result.tcpStability, 'maxMs')
    case 'tcpFailure': return entry.tcpFailurePercent
    case 'initialTcp': return initialTcp(entry.result)
    case 'download': return downloadValue(entry.result)
    case 'ip': return null
  }
}
const precisionRecords = computed(() => props.run.results.filter(hasPrecisionMeasurement).map(record))
const initialIssueResults = computed(() => props.run.results.filter(result => !hasPrecisionMeasurement(result) && hasInitialTcpIssue(result)))
const records = computed(() => initialIssuesOpen.value ? initialIssueResults.value.map(record) : precisionRecords.value)
const tcpChartScaleMs = computed(() => tcpLatencyChartScale(props.run.results))
const regionalStats = computed(() => {
  const run = props.run
  const limit = regionalPrecisionLimit(run.config)
  // A stopped scan may still be discovering POPs. Precision work confirms
  // admission took place in the stability-first flow.
  const admissionKnown = run.config.stabilityFirst && (run.status === 'completed' || run.results.some(hasPrecisionMeasurement))
  return run.config.regions.map(code => {
    const candidates = run.results.filter(result => result.regionCode === code)
    const identified = Math.max(run.progress.discoveredByRegion[code] ?? 0, candidates.filter(result => result.colo).length)
    return {
      code,
      identified,
      admitted: admissionKnown && limit != null ? Math.min(identified, limit) : null,
      stabilityPassed: candidates.filter(result => stabilityVerdict(result, run.config) === 'passed').length,
      downloaded: candidates.filter(result => completeDownloadValue(result) != null).length,
    }
  })
})
const exclusions = computed(() => summarizeExclusions(
  props.run.results.filter(result => result.exclusionCode || result.exclusionReason),
  props.run.status,
))
const maxDownloadMbps = computed(() => records.value.reduce((maximum, record) => Math.max(maximum, downloadValue(record.result) ?? 0), 0))
function relativeDownloadWidth(result: IpResult) { return `${maxDownloadMbps.value > 0 ? Math.max(0, (downloadValue(result) ?? 0) / maxDownloadMbps.value * 100) : 0}%` }
function downloadDescription(result: IpResult) {
  const value = downloadValue(result)
  if (value == null) return '尚未取得下载样本'
  const completion = completeDownloadValue(result) != null ? '已完成下载测速' : `尚未取得完整有效的下载结果：已记录 ${result.downloadSamplesMbps.length} / ${props.run.config.downloadRounds} 轮样本`
  return `${completion}，${fixed(value)} Mbps；横条相对于本轮最高已测速率 ${fixed(maxDownloadMbps.value)} Mbps`
}
const downloadedCount = computed(() => precisionRecords.value.filter(record => record.download != null).length)
const roundSummary = computed(() => props.run.status === 'running' ? null : summarizeScanResults(props.run, downloadedCount.value))
const qualifiedCount = computed(() => precisionRecords.value.filter(record => record.status === 'qualified').length)
const retestCount = computed(() => precisionRecords.value.filter(record => record.status === 'retest').length)
const hasBudgetExclusions = computed(() => props.run.results.some(result => result.exclusionCode === 'downloadBudget'))
const identifiedRegions = computed(() => [...new Set([
  ...Object.entries(props.run.progress.discoveredByRegion).filter(([, count]) => count > 0).map(([code]) => code),
  ...props.run.results.filter(result => result.colo && /^[A-Z]{2}$/.test(result.regionCode)).map(result => result.regionCode),
])].map(location).join('、'))
const canFilterDownloads = computed(() => props.run.status === 'running' || downloadedCount.value > 0)
const downloadFilterActive = computed(() => !observationsOpen.value && !initialIssuesOpen.value && canFilterDownloads.value && completedDownloadOnly.value)
function count(value: number) { return value.toLocaleString('zh-CN') }
const regionOptions = computed(() => [...new Set(precisionRecords.value.map(record => record.result.regionCode))]
  .sort((a, b) => location(a).localeCompare(location(b), 'zh-CN')))
const popOptions = computed(() => [...new Set(precisionRecords.value
  .filter(record => regionFilter.value === 'all' || record.result.regionCode === regionFilter.value)
  .map(record => firstPop(record.result))
  .filter(Boolean))].sort())
const matchingRecords = computed(() => {
  const search = query.value.trim().toLowerCase()
  return precisionRecords.value.filter(({ result, download, tcp }) => {
    return (regionFilter.value === 'all' || result.regionCode === regionFilter.value)
      && (popFilter.value === 'all' || firstPop(result) === popFilter.value)
      && (minDownloadMbps.value === '' || (download != null && download >= minDownloadMbps.value))
      && (maxTcpMs.value === '' || (tcp != null && tcp <= maxTcpMs.value))
      && (!search || [result.ip, result.colo, result.city, result.regionCode, location(result.regionCode), ...result.observedColos].some(value => value.toLowerCase().includes(search)))
  })
})
const statusCandidates = computed(() => downloadFilterActive.value ? matchingRecords.value.filter(record => record.download != null) : matchingRecords.value)
const statusCounts = computed(() => Object.fromEntries(statusOptions.map(option => [option.value, statusCandidates.value.filter(record => record.status === option.value).length])))
const statusDownloadConflict = computed(() => downloadFilterActive.value
  && statusFilter.value !== 'all'
  && statusCounts.value[statusFilter.value] === 0
  && matchingRecords.value.some(record => record.status === statusFilter.value))
const filtered = computed(() => {
  if (initialIssuesOpen.value) {
    const search = initialIssueQuery.value.trim()
    return records.value.filter(({ result }) => result.ip.includes(search))
      .sort((a, b) => ipOrder.compare(a.result.ip, b.result.ip))
  }
  return statusCandidates.value
    .filter(record => statusFilter.value === 'all' || record.status === statusFilter.value)
    .sort((a, b) => {
      const order = sortBy.value
      const difference = compareNullable(sortValue(a, order), sortValue(b, order), order === 'download')
      return difference || ipOrder.compare(a.result.ip, b.result.ip)
    })
})
const pageCount = computed(() => Math.max(1, Math.ceil(filtered.value.length / pageSize)))
const currentPage = computed(() => Math.min(page.value, pageCount.value))
const firstRow = computed(() => (currentPage.value - 1) * pageSize)
const visibleRows = computed(() => filtered.value.slice(firstRow.value, firstRow.value + pageSize))
const moreFilterCount = computed(() => [statusFilter.value !== 'all', minDownloadMbps.value !== '', maxTcpMs.value !== ''].filter(Boolean).length)
const transientFilterCount = computed(() => initialIssuesOpen.value ? Number(!!initialIssueQuery.value.trim())
  : [!!query.value.trim(), regionFilter.value !== 'all', popFilter.value !== 'all'].filter(Boolean).length + moreFilterCount.value)
const hasFilters = computed(() => transientFilterCount.value > 0 || downloadFilterActive.value)
const emptyTitle = computed(() => {
  if (initialIssuesOpen.value) return records.value.length ? '找不到这个 IP 的初筛记录' : '本轮暂无初筛问题'
  if (!records.value.length) return props.run.status === 'running' ? '正在寻找可精测的 IP' : '本轮没有精测记录'
  return downloadFilterActive.value && !downloadedCount.value && !transientFilterCount.value ? '还没有完成下载的 IP' : '没有符合条件的精测结果'
})
const emptyDescription = computed(() => {
  if (initialIssuesOpen.value) return records.value.length ? '清除搜索可查看本轮全部初筛问题。' : ''
  if (statusDownloadConflict.value) return '「仅完成下载」隐藏了符合所选状态的记录。'
  if (records.value.length) return downloadFilterActive.value ? '可取消下载筛选，查看已取得的 TCP 精测记录。' : ''
  return props.run.status === 'running' ? '完成初筛和地区识别后，精测记录会显示在这里。' : ''
})
function resetTransientFilters() {
  if (initialIssuesOpen.value) {
    initialIssueQuery.value = ''
    return
  }
  query.value = ''
  regionFilter.value = 'all'
  popFilter.value = 'all'
  statusFilter.value = 'all'
  minDownloadMbps.value = ''
  maxTcpMs.value = ''
}
async function setInitialIssuesOpen(open: boolean) {
  moreFiltersOpen.value = false
  summaryOpen.value = false
  initialIssuesOpen.value = open
  await nextTick()
  const target = initialIssuesOpen.value ? initialIssueSearchInput.value : summaryTrigger.value
  target?.focus({ preventScroll: true })
}
async function includeIncompleteDownloads() {
  completedDownloadOnly.value = false
  await nextTick()
  statusSelect.value?.focus({ preventScroll: true })
}
function showObservations() {
  closeSummaryForResults()
  initialIssuesOpen.value = false
  resetTransientFilters()
  observationsOpen.value = true
}
function chooseDownloadFilter(event: Event) {
  observationsOpen.value = false
  completedDownloadOnly.value = (event.target as HTMLInputElement).checked
}
function showQualified(order?: ResultSortBy) {
  closeSummaryForResults()
  initialIssuesOpen.value = false
  resetTransientFilters()
  statusFilter.value = 'qualified'
  if (order) sortBy.value = order
}
function showStabilityFailures() {
  showObservations()
  statusFilter.value = 'rejected'
}
function showRetestCandidates() {
  showObservations()
  statusFilter.value = 'retest'
}
function closeSummaryForResults() {
  const wasOpen = summaryOpen.value
  summaryOpen.value = false
  if (wasOpen) void nextTick(() => (resultsTable.value ?? summaryTrigger.value)?.focus({ preventScroll: true }))
}
function requestAction(action: ResultAction) {
  summaryOpen.value = false
  emit('action', action)
}
function reuseSettings() {
  if (props.settingsDisabled) return
  summaryOpen.value = false
  emit('reuse-config')
  void nextTick(() => summaryTrigger.value?.focus({ preventScroll: true }))
}
defineExpose({ showObservations })

watch(popOptions, options => {
  if (popFilter.value !== 'all' && !options.includes(popFilter.value)) popFilter.value = 'all'
})
watch(() => props.run.status, status => {
  if (status !== 'running' && statusFilter.value === 'pendingSummary') statusFilter.value = 'all'
})
async function resetTableScroll() { await nextTick(); if (resultsTable.value) resultsTable.value.scrollTop = 0 }
watch([query, regionFilter, popFilter, statusFilter, sortBy, completedDownloadOnly, observationsOpen, minDownloadMbps, maxTcpMs, initialIssuesOpen, initialIssueQuery], () => { page.value = 1; expandedIp.value = null; void resetTableScroll() })
watch(currentPage, () => { expandedIp.value = null; void resetTableScroll() })
watch(() => props.run.id, () => { observationsOpen.value = false; initialIssuesOpen.value = false; initialIssueQuery.value = ''; resetTransientFilters(); page.value = 1; expandedIp.value = null; summaryOpen.value = false })
</script>

<template>
  <section class="all-results-panel" aria-labelledby="all-results-title">
    <div class="all-results-controls">
    <div class="all-results-heading">
      <h2 id="all-results-title">测试结果</h2>
      <span class="status-label" :class="{ active: run.status === 'running', good: run.status === 'completed' && downloadedCount > 0, quiet: run.status === 'cancelled' || run.status === 'completed' && !downloadedCount, error: run.status === 'failed' }" role="status" aria-live="polite">{{ runStatus }}</span>
      <span v-if="run.quickValidation || run.retestIp" class="run-mode-label">{{ scanModeLabel(run) }}</span>
      <time v-if="testTime" class="results-test-time muted" :datetime="run.startedAt" title="本轮开始时间">{{ testTime }}</time>
      <p v-if="roundSummary" class="results-heading-summary" :title="[roundSummary.headline, roundSummary.detail].filter(Boolean).join('：')">{{ roundSummary.headline }}</p>
      <button v-if="retestCount" type="button" class="text-button result-retest-link" @click="showRetestCandidates">{{ retestCount }} 个建议复测</button>
      <button ref="summaryTrigger" type="button" class="button more-filters-button run-summary-button" :class="{ 'is-active': summaryOpen }" :aria-expanded="summaryOpen" aria-controls="run-summary" :title="settingsDiffer ? '查看本轮结果、实际参数与下次设置的差异' : '查看本轮参数、地区统计和处理建议'" @click="summaryOpen = !summaryOpen">
        <BarChart3 :size="13" aria-hidden="true" />本轮详情<span v-if="settingsDiffer" class="settings-differ-indicator" aria-label="下次设置与本轮不同" /><ChevronDown :size="12" :class="{ 'is-open': summaryOpen }" aria-hidden="true" />
      </button>
      <section v-if="summaryOpen" id="run-summary" ref="summaryPanel" class="run-summary-panel" aria-labelledby="run-summary-title" tabindex="0">
        <h3 id="run-summary-title">本轮详情</h3>
        <p>{{ scanModeLabel(run) }} · {{ runStatus }} · {{ testTime }}</p>
        <dl class="run-parameters" aria-label="本轮实际测速参数">
          <div class="run-target"><dt>测速目标</dt><dd>{{ run.config.targetUrl }}</dd></div>
          <div v-if="run.retestIp"><dt>复测 IP</dt><dd class="mono">{{ run.retestIp }}</dd></div>
          <div v-else><dt>抽样上限</dt><dd>{{ count(run.config.maxCandidates) }} 个</dd></div>
          <div><dt>TCP 探测</dt><dd>{{ run.config.stabilityAttempts == null ? '未记录稳定性计划' : `${run.config.stabilityAttempts} 次` }} · {{ run.config.maxTcpMs == null ? '未设延迟上限' : `${metricName} ≤ ${run.config.maxTcpMs} ms` }}</dd></div>
          <div><dt>下载计划</dt><dd>{{ run.config.downloadRounds }} 轮 × {{ run.config.downloadSeconds }} 秒</dd></div>
          <div><dt>下载预算</dt><dd>{{ run.config.downloadBudgetEnabled !== false ? `${run.config.downloadBudgetMiB} MiB` : '不限下载流量' }}</dd></div>
        </dl>
        <div v-if="settingsDiffer" class="run-settings-difference"><span>下次测速设置与本轮不同，已有结果仍按本轮参数解释。</span><button class="text-button" :disabled="settingsDisabled" @click="reuseSettings">沿用本轮参数</button></div>
        <div v-if="roundSummary" class="run-overview">
          <div class="results-overview-outcome">
            <p class="results-overview-title">{{ roundSummary.headline }}</p>
            <p v-if="roundSummary.detail" class="results-overview-detail">{{ roundSummary.detail }}</p>
            <p v-if="downloadedCount" class="results-overview-detail">其中 {{ qualifiedCount }} 个完整测速达标；下载完成不等同于所有条件达标。</p>
            <p v-if="!roundSummary.precisionCount" class="results-overview-detail">{{ identifiedRegions ? `本轮识别地区：${identifiedRegions}` : '本轮尚未识别到接入地区，请检查当前网络与测速目标。' }}</p>
            <p v-if="run.quickValidation && !qualifiedCount" class="results-overview-detail">快速验证抽样较少，未找到结果不代表所选地区没有可用 IP。</p>
            <div class="result-next-actions" aria-label="本轮结果的下一步操作">
              <template v-if="qualifiedCount"><button class="text-button" @click="showQualified()">查看达标 IP（{{ qualifiedCount }}）</button><button class="text-button" @click="showQualified('tcp')">优先低延迟</button><button class="text-button" @click="showQualified('download')">优先下载速度</button></template>
              <button v-if="hasBudgetExclusions" class="text-button" @click="requestAction('budget')">调整预算后重测</button>
              <button v-if="retestCount" class="text-button" @click="showRetestCandidates">查看建议复测的 IP</button>
              <button v-if="roundSummary.stabilityFailures > retestCount" class="text-button" @click="showStabilityFailures">查看稳定性失败</button>
              <button v-if="roundSummary.stabilityFailures" class="text-button" @click="requestAction('stability')">检查判定条件</button>
              <template v-if="!roundSummary.precisionCount"><button v-if="!run.retestIp" class="text-button" @click="requestAction('regions')">调整目标地区</button><button class="text-button" @click="requestAction('target')">检查测速目标</button></template>
              <button v-if="run.quickValidation && !qualifiedCount" class="text-button" @click="requestAction('expand')">扩大抽样做完整优选</button>
              <button v-if="!qualifiedCount && roundSummary.precisionCount" class="text-button" @click="showObservations">查看已有精测记录</button>
            </div>
          </div>
          <dl class="results-overview-counts" aria-label="本轮各阶段的 IP 数量">
            <div>
              <dt>已做初筛</dt>
              <dd>
                {{ count(roundSummary.checkedCandidates) }}<span v-if="roundSummary.checkedCandidates < roundSummary.totalCandidates" title="已初筛 / 抽样数"> / {{ count(roundSummary.totalCandidates) }}</span>
                <button v-if="initialIssueResults.length && !initialIssuesOpen" type="button" class="text-button initial-issues-link" @click="setInitialIssuesOpen(true)">查看初筛异常（{{ count(initialIssueResults.length) }}）<ChevronRight :size="11" aria-hidden="true" /></button>
              </dd>
            </div>
            <div><dt>有精测记录</dt><dd>{{ count(roundSummary.precisionCount) }}</dd></div>
            <div><dt>完成下载</dt><dd>{{ count(roundSummary.downloadedCount) }}</dd></div>
          </dl>
        </div>

        <button v-if="!roundSummary && initialIssueResults.length && !initialIssuesOpen" type="button" class="text-button" @click="setInitialIssuesOpen(true)">查看初筛异常（{{ count(initialIssueResults.length) }}）<ChevronRight :size="11" /></button>
        <p v-if="run.error">{{ run.error }}</p>
        <h4 v-if="!run.retestIp">各地区统计</h4>
        <table v-if="!run.retestIp">
          <thead><tr><th scope="col">目标地区</th><th scope="col">识别到的 IP</th><th scope="col">入围复测</th><th scope="col">稳定性通过</th><th scope="col">完成下载</th></tr></thead>
          <tbody>
            <tr v-for="region in regionalStats" :key="region.code">
              <th scope="row">{{ location(region.code) }}</th>
              <td>{{ region.identified }}</td>
              <td>{{ region.admitted ?? '—' }}</td>
              <td>{{ region.stabilityPassed }}</td>
              <td>{{ region.downloaded }}</td>
            </tr>
          </tbody>
        </table>
        <p v-if="!run.retestIp">统计涵盖本轮所有观测。<template v-if="regionalStats.some(region => region.admitted == null)"> — 表示入围数无法确认。</template></p>
        <h4>本轮排除或未完成原因</h4>
        <ul v-if="exclusions.length">
          <li v-for="item in exclusions" :key="item.key"><span>{{ item.label }}</span><strong>{{ item.count }}</strong></li>
        </ul>
        <p v-else>暂无已记录的排除或未完成原因。</p>
      </section>
    </div>

    <div v-if="initialIssuesOpen" class="initial-issues-toolbar">
      <button type="button" class="text-button initial-issues-back" @click="setInitialIssuesOpen(false)"><ChevronLeft :size="14" aria-hidden="true" />返回精测结果</button>
      <h3>初筛异常</h3>
      <label class="field initial-issues-search">
        <span class="visually-hidden">搜索初筛异常 IP</span>
        <span class="results-search-control"><Search :size="14" aria-hidden="true" /><input ref="initialIssueSearchInput" v-model="initialIssueQuery" type="search" placeholder="搜索 IP 地址" aria-label="搜索初筛异常 IP" /></span>
      </label>
      <button v-if="transientFilterCount" class="text-button" @click="resetTransientFilters">清除搜索</button>
    </div>
    <div v-if="!initialIssuesOpen" class="all-results-filters" :class="{ 'has-download-toggle': canFilterDownloads }">
      <label class="field results-search"><span class="visually-hidden">搜索 IP、地区或机房</span><span class="results-search-control"><Search :size="14" aria-hidden="true" /><input v-model="query" type="search" placeholder="搜索 IP、地区或机房" aria-label="搜索 IP、地区或机房" /></span></label>
      <label class="field"><span class="visually-hidden">地区</span><SelectControl compact><select v-model="regionFilter" aria-label="地区"><option value="all">全部地区</option><option v-if="regionFilter !== 'all' && !regionOptions.includes(regionFilter)" :value="regionFilter">{{ location(regionFilter) }}</option><option v-for="code in regionOptions" :key="code" :value="code">{{ location(code) }}{{ /^[A-Z]{2}$/.test(code) ? ` · ${code}` : '' }}</option></select></SelectControl></label>
      <label class="field"><span class="visually-hidden">机房（POP）</span><SelectControl compact><select v-model="popFilter" aria-label="机房（POP）"><option value="all">全部机房</option><option v-if="popFilter !== 'all' && !popOptions.includes(popFilter)" :value="popFilter">{{ popFilter }}</option><option v-for="code in popOptions" :key="code" :value="code">{{ code }}</option></select></SelectControl></label>
      <label class="field results-sort"><span class="visually-hidden">排序</span><SelectControl compact>
        <select v-model="sortBy" aria-label="排序">
          <option value="tcp">复测延迟（{{ metricName }}）升序</option>
          <option value="medianTcp">中位延迟从低到高</option>
          <option value="averageTcp">样本平均延迟从低到高</option>
          <option value="peakTcp">样本峰值延迟从低到高</option>
          <option value="tcpFailure">连接失败率从低到高</option>
          <option value="download">下载速度从高到低</option>
          <option value="initialTcp">初筛延迟从低到高</option>
          <option value="ip">IP 地址排序</option>
        </select>
      </SelectControl></label>
      <button ref="moreFiltersTrigger" class="button more-filters-button" :class="{ 'is-active': moreFiltersOpen || moreFilterCount }" :aria-expanded="moreFiltersOpen" aria-controls="additional-result-filters" aria-label="更多筛选" title="更多筛选" @click="moreFiltersOpen = !moreFiltersOpen"><SlidersHorizontal :size="13" /><span class="filter-button-label">更多筛选</span><span v-if="moreFilterCount" class="filter-count">{{ moreFilterCount }}</span><ChevronDown :size="12" :class="{ 'is-open': moreFiltersOpen }" /></button>
      <label v-if="canFilterDownloads" class="check-field completed-download-toggle" :title="observationsOpen ? '本轮临时显示全部记录，默认筛选偏好保持不变' : '只显示取得全部有效下载样本的 IP'">
        <input :checked="downloadFilterActive" type="checkbox" aria-describedby="completed-download-help" @change="chooseDownloadFilter" /><span>仅完成下载</span>
      </label>
      <span v-if="canFilterDownloads" id="completed-download-help" class="visually-hidden">{{ observationsOpen ? '本轮临时显示全部记录，默认筛选偏好保持不变。' : '此筛选选择会自动保存。' }}</span>
    </div>
    <div v-if="moreFiltersOpen && !initialIssuesOpen" id="additional-result-filters" ref="moreFiltersPanel" class="additional-result-filters">
      <div class="all-results-thresholds">
        <label class="field"><span>测试状态</span><SelectControl compact><select ref="statusSelect" v-model="statusFilter" aria-label="测试状态"><option value="all">全部状态 · {{ statusCandidates.length }}</option><option v-for="option in visibleStatusOptions" :key="option.value" :value="option.value">{{ option.label }} · {{ statusCounts[option.value] }}</option></select></SelectControl></label>
        <label class="field"><span>最低下载速度（Mbps）</span><input v-model.number="minDownloadMbps" type="number" min="0" step="any" placeholder="不限" aria-describedby="results-threshold-help" /></label>
        <label class="field"><span>最高 TCP {{ metricName }}（ms）</span><input v-model.number="maxTcpMs" type="number" min="0" step="any" placeholder="不限" aria-describedby="results-threshold-help" /></label>
      </div>
      <p id="results-threshold-help" class="muted small">留空表示不限；指标筛选仅匹配完整的下载或 TCP 稳定性测量。</p>
      <p v-if="statusDownloadConflict" class="filter-conflict">「仅完成下载」隐藏了符合所选状态的记录。<button type="button" class="text-button" @click="includeIncompleteDownloads">关闭仅完成下载</button></p>
    </div>
    </div>

    <div v-if="visibleRows.length" ref="resultsTable" class="all-results-table" role="region" :aria-label="initialIssuesOpen ? '初筛连接问题表格，可滚动' : '全部精测结果表格，可滚动'" tabindex="0">
      <table :class="{ 'initial-issues-table': initialIssuesOpen }">
        <colgroup>
          <col class="ip-column" />
          <col v-if="!initialIssuesOpen" class="location-column" />
          <col class="outcome-column" />
          <col class="tcp-column" />
          <col v-if="!initialIssuesOpen" class="download-column" />
          <col class="favorite-column" />
        </colgroup>
        <thead><tr>
          <th scope="col">IP 地址</th>
          <th v-if="!initialIssuesOpen" scope="col">地区 / 机房</th>
          <th scope="col">{{ initialIssuesOpen ? '初筛结论' : '测试结论' }}</th>
          <th scope="col" class="result-measurement-cell">
            <template v-if="initialIssuesOpen">连接耗时 <span class="table-unit">ms</span></template>
            <TcpLatencyHeader v-else v-model="tcpDisplayMetric" :run-metric="metric" />
          </th>
          <th v-if="!initialIssuesOpen" scope="col" class="result-measurement-cell">下载 <span class="table-unit">Mbps</span></th>
          <th scope="col">收藏</th>
        </tr></thead>
        <tbody>
          <template v-for="(record, index) in visibleRows" :key="record.result.ip">
            <tr :class="{ striped: index % 2 === 1 }">
              <td><span class="result-ip-cell">
                <button v-if="initialIssuesOpen || record.reason || record.result.tcpStability?.attempted || (run.status !== 'running' && record.status !== 'qualified')" type="button" class="icon-button result-reason-trigger" :aria-label="`查看 ${record.result.ip} 的测速详情`" title="查看测速详情" :aria-expanded="expandedIp === record.result.ip" :aria-controls="expandedIp === record.result.ip ? `result-reason-${record.result.ip}` : undefined" @click="expandedIp = expandedIp === record.result.ip ? null : record.result.ip">
                  <ChevronRight :size="13" aria-hidden="true" />
                </button>
                <span class="mono result-ip">{{ record.result.ip }}</span><button class="icon-button result-copy" :disabled="!desktop || copyingIp !== null" :aria-label="`复制 IP ${record.result.ip}`" :title="desktop ? '复制 IP 地址' : '请在桌面应用中复制'" @click="copyIp(record.result.ip)"><Copy :size="13" aria-hidden="true" /></button></span></td>
              <td v-if="!initialIssuesOpen" :title="record.result.observedColos.length > 1 ? `本次观测到的机房 ${record.result.observedColos.join(' / ')}。表格显示首次观测。` : undefined"><span class="location-cell"><span class="location-name">{{ location(record.result.regionCode) }}</span><span v-if="firstPop(record.result)" class="result-pop">{{ firstPop(record.result) }}</span></span></td>
              <td>
                <button v-if="record.status === 'retest' && desktop" type="button" class="text-button result-retest-link" :disabled="retestDisabled" :title="quickRetestAdvice(record.result, run) ?? ''" :aria-label="`完整复测 ${record.result.ip}`" @click="emit('retest', record.result.ip)">建议完整复测</button>
                <span v-else class="result-outcome" :data-tone="record.display.outcome.tone" :title="record.reason || record.display.outcome.label">{{ record.display.outcome.label }}</span>
                <small v-if="!initialIssuesOpen && record.result.tcpStability && record.tcpFailurePercent != null" class="result-tcp-failures" title="TCP 连接失败次数 / 已探测次数">
                  失败 {{ record.result.tcpStability.failed }}/{{ record.result.tcpStability.attempted }} · {{ failureRateFormat.format(record.tcpFailurePercent) }}%
                </small>
              </td>
              <td class="result-measurement-cell">
                <template v-if="initialIssuesOpen">
                  <span v-if="record.result.tcpConnectFailure?.elapsedMs != null && Number.isFinite(record.result.tcpConnectFailure.elapsedMs) && record.result.tcpConnectFailure.elapsedMs >= 0" class="result-tcp-value"><small>失败前等待</small><span class="result-number">{{ fixed(record.result.tcpConnectFailure.elapsedMs) }}</span></span>
                  <span v-else-if="record.display.tcp" class="result-tcp-value" :title="record.display.tcp.title"><small>{{ record.display.tcp.label }}</small><span class="result-number">{{ fixed(record.display.tcp.value) }}</span></span>
                  <span v-else class="result-value-missing">未测得</span>
                </template>
                <TcpLatencyCell v-else :result="record.result" :run-metric="metric" :completed-value="record.tcp"
                  :display-metric="tcpDisplayMetric" :scale-max-ms="tcpChartScaleMs" />
              </td>
              <td v-if="!initialIssuesOpen" class="result-measurement-cell">
                <span v-if="downloadValue(record.result) == null" class="result-value-missing" :title="record.reason || downloadDescription(record.result)">{{ record.display.missingDownload }}</span>
                <template v-else>
                  <span class="speed-cell" :class="{ 'is-partial': record.download == null }" :title="downloadDescription(record.result)"><span v-if="maxDownloadMbps > 0" class="speed-meter" aria-hidden="true"><span :style="{ width: relativeDownloadWidth(record.result) }" /></span><span class="result-number" :class="{ 'result-speed': record.download != null }">{{ fixed(downloadValue(record.result)) }}</span></span>
                  <span v-if="record.download == null" class="result-value-missing">部分样本</span>
                </template>
              </td>
              <td><FavoriteButton :ip="record.result.ip" :saved="savedIps.has(record.result.ip)" :disabled="favoriteDisabled" compact @click="$emit('favorite', record.result, $event)" /></td>
            </tr>
            <tr v-if="expandedIp === record.result.ip" :id="`result-reason-${record.result.ip}`" class="result-reason-row">
              <td :colspan="initialIssuesOpen ? 4 : 6">
                <strong>{{ candidateMeasurementStatus(record.result, run) }}</strong>
                <p v-if="record.reason">{{ record.reason }}</p>
                <p v-if="record.status === 'retest'">{{ quickRetestAdvice(record.result, run) }}</p>
                <TcpDiagnostics :result="record.result" :config="run.config" />
              </td>
            </tr>
          </template>
        </tbody>
      </table>
    </div>
    <div v-else class="all-results-empty"><span class="results-empty-icon"><Activity v-if="run.status === 'running' && !records.length" :size="24" /><SearchX v-else :size="24" /></span><h3>{{ emptyTitle }}</h3><p v-if="emptyDescription">{{ emptyDescription }}</p><div v-if="!initialIssuesOpen && records.length && (transientFilterCount || downloadFilterActive)" class="results-empty-actions"><button v-if="transientFilterCount" class="button" @click="resetTransientFilters">清除筛选</button><button v-if="downloadFilterActive" class="text-button" @click="showObservations">查看已取得的记录<ChevronRight :size="14" /></button></div></div>
    <div class="all-results-footer">
      <span class="muted small" role="status" aria-live="polite">{{ hasFilters ? `显示 ${count(filtered.length)} / ${count(records.length)} 个 IP` : `共 ${count(records.length)} 个 IP` }}{{ filtered.length > pageSize ? ` · 当前 ${count(firstRow + 1)}–${count(firstRow + visibleRows.length)}` : '' }}</span>
      <button v-if="!initialIssuesOpen && transientFilterCount" class="text-button clear-result-filters" @click="resetTransientFilters">清除筛选</button>
      <nav v-if="pageCount > 1" class="all-results-pagination" :aria-label="initialIssuesOpen ? '初筛异常分页' : '精测结果分页'"><span class="muted small">{{ currentPage }} / {{ pageCount }} 页</span><button class="icon-button" :disabled="currentPage <= 1" aria-label="上一页" @click="page = currentPage - 1"><ChevronLeft :size="15" aria-hidden="true" /></button><button class="icon-button" :disabled="currentPage >= pageCount" aria-label="下一页" @click="page = currentPage + 1"><ChevronRight :size="15" aria-hidden="true" /></button></nav>
    </div>
  </section>
</template>

<style scoped>
.result-next-actions { display: flex; flex-wrap: wrap; gap: 2px 12px; margin-top: 4px; }
.result-next-actions .text-button { font-size: 11px; min-height: 24px; padding: 0; }
.result-retest-link { flex: none; min-height: 22px; padding: 0; color: var(--warn); font-size: 11px; white-space: nowrap; }
.visually-hidden { position: absolute; width: 1px; height: 1px; overflow: hidden; clip: rect(0,0,0,0); white-space: nowrap; }
.all-results-panel { display: flex; flex: 1 1 0; flex-direction: column; min-width: 0; min-height: 0; margin: 0; padding: 0; overflow: hidden; border: 0; border-radius: 0; box-shadow: none; background: var(--panel); }
.all-results-controls { position: relative; z-index: 3; display: flex; flex-direction: column; flex: 0 0 auto; padding-bottom: 8px; overflow: visible; }
.all-results-heading { display: flex; flex: 0 0 42px; align-items: center; gap: 10px; height: 42px; min-height: 0; padding: 0 20px; }
.all-results-heading h2 { flex: none; display: inline-flex; align-items: center; gap: 8px; font-size: 14px; line-height: 20px; font-weight: 600; letter-spacing: -.15px; white-space: nowrap; }
.all-results-heading h2::before { content: ""; width: 3px; height: 13px; border-radius: 2px; background: var(--brand); opacity: .8; }
.all-results-heading .status-label { flex: none; min-height: 20px; padding: 0 7px; font-size: 11px; line-height: 20px; border-radius: 5px; border: 1px solid color-mix(in srgb, currentColor 12%, transparent); }
.results-test-time { flex: none; font-size: 11px; font-variant-numeric: tabular-nums; white-space: nowrap; }
.run-summary-button { flex: none; margin-left: auto; }
.results-heading-summary { flex: 1; min-width: 0; margin: 0; color: var(--muted); font-size: 12px; font-weight: 400; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.run-mode-label { flex: none; padding: 0 5px; color: var(--brand); background: var(--soft); border-radius: 4px; font-size: 10px; line-height: 20px; white-space: nowrap; }
.settings-differ-indicator { flex: none; width: 5px; height: 5px; border-radius: 50%; background: var(--warn); }
.run-parameters { display: grid; grid-template-columns: repeat(2,minmax(0,1fr)); gap: 8px 16px; margin: 10px 0; }
.run-parameters>div { min-width: 0; }
.run-parameters dt { color: var(--muted); font-size: 11px; }
.run-parameters dd { margin: 2px 0 0; overflow-wrap: anywhere; font-variant-numeric: tabular-nums; }
.run-parameters .run-target { grid-column: 1/-1; }
.run-settings-difference { display: flex; align-items: center; flex-wrap: wrap; gap: 4px 12px; padding: 8px 0; border-top: 1px solid var(--line); font-size: 11px; color: var(--muted); }
.run-overview { margin: 8px 0 12px; padding: 10px 0; border-block: 1px solid var(--line); }
.results-overview-outcome { min-width: 0; }
.results-overview-title { margin: 0; font-size: 13px; font-weight: 650; line-height: 19px; }
.results-overview-detail { margin: 3px 0 0; color: var(--muted); font-size: 11px; line-height: 16px; overflow-wrap: anywhere; }
.results-overview-counts { display: grid; grid-template-columns: repeat(3,minmax(70px,auto)); flex: 0 0 auto; margin: 10px 0 0; text-align: center; font-variant-numeric: tabular-nums; }
.results-overview-counts>div { padding: 0 12px; border-left: 1px solid var(--line); }
.results-overview-counts dt { color: var(--muted); font-size: 10px; line-height: 14px; white-space: nowrap; }
.results-overview-counts dd { margin: 2px 0 0; font-size: 16px; font-weight: 650; line-height: 20px; }
.results-overview-counts dd>span { color: var(--muted); font-size: 11px; font-weight: 400; }
.initial-issues-link { display: flex; min-height: 18px; gap: 2px; margin: 0 auto; padding: 0; color: var(--brand); font-size: 11px; line-height: 16px; white-space: nowrap; }
.all-results-filters { display: grid; flex: 0 0 auto; grid-template-columns: minmax(130px,1.5fr) minmax(92px,.8fr) minmax(84px,.7fr) minmax(155px,1fr) auto; gap: 8px; padding: 0 20px; }
.all-results-filters.has-download-toggle { grid-template-columns: minmax(130px,1.5fr) minmax(92px,.8fr) minmax(84px,.7fr) minmax(155px,1fr) auto auto; }
.completed-download-toggle { align-self: center; white-space: nowrap; font-size: 11px; gap: 5px; }
.completed-download-toggle input { width: 14px; height: 14px; }
.all-results-filters .field { gap: 0; }
.all-results-filters :deep(select), .all-results-filters input[type=search], .more-filters-button { border-color: color-mix(in srgb, var(--line) 88%, var(--muted)); background: linear-gradient(var(--panel), color-mix(in srgb, var(--bg) 38%, var(--panel))); box-shadow: 0 1px 1px color-mix(in srgb, var(--text) 3%, transparent); transition: border-color .15s, box-shadow .15s, background .15s; }
.all-results-filters :deep(select:hover:not(:disabled)), .all-results-filters input[type=search]:hover { border-color: var(--line-hover); }
.all-results-filters :deep(select:focus-visible), .all-results-filters input[type=search]:focus-visible, .more-filters-button:focus-visible { outline: 2px solid color-mix(in srgb, var(--brand) 55%, transparent); outline-offset: 2px; border-color: var(--brand); box-shadow: 0 0 0 1px var(--panel); }
.all-results-filters :deep(.select-control>select), .additional-result-filters :deep(.select-control>select), .results-search-control>input[type=search], .additional-result-filters input[type=number] { height: 30px; min-height: 30px; font-size: 12px; border-radius: 5px; }
.results-search-control { position: relative; display: block; }
.results-search-control>svg { position: absolute; left: 9px; top: 50%; transform: translateY(-50%); color: var(--muted); pointer-events: none; }
.results-search-control>input[type=search] { padding-left: 29px; }
.results-search-control:focus-within>svg { color: var(--brand); }
.initial-issues-toolbar { display: flex; align-items: center; flex-wrap: wrap; gap: 6px 12px; min-height: 34px; padding: 0 20px; }
.initial-issues-toolbar h3 { font-size: 13px; font-weight: 600; }
.initial-issues-back { padding: 0; white-space: nowrap; }
.initial-issues-search { flex: 0 1 260px; min-width: 140px; margin-left: auto; }
.clear-result-filters { margin-left: auto; white-space: nowrap; }
.more-filters-button { min-height: 30px; gap: 5px; padding-inline: 8px; font-size: 12px; border-radius: 5px; }
.more-filters-button.is-active { background: var(--soft); border-color: var(--selected-border); color: var(--brand); box-shadow: inset 0 1px 0 color-mix(in srgb, var(--panel) 70%, transparent); }
.more-filters-button>svg:last-child { transition: transform .15s; }
.more-filters-button>svg.is-open { transform: rotate(180deg); }
.filter-count { display: inline-grid; place-items: center; min-width: 16px; height: 16px; padding-inline: 3px; border-radius: 4px; background: var(--soft); color: var(--brand); font-size: 10px; font-variant-numeric: tabular-nums; }
.additional-result-filters, .run-summary-panel { position: absolute; top: calc(100% - 1px); right: 20px; z-index: 10; width: min(600px,calc(100% - 40px)); max-height: 40vh; margin: 0; padding: 12px; overflow: auto; overscroll-behavior: contain; border: 1px solid var(--line); border-radius: 8px; background: var(--surface-raised); box-shadow: 0 10px 28px #2328491c, 0 2px 6px #2328490a; }
.all-results-thresholds { display: grid; grid-template-columns: minmax(160px,1.3fr) repeat(2,minmax(110px,1fr)); gap: 12px; }
.all-results-thresholds .field { gap: 4px; }
.all-results-thresholds .field>span { font-size: 11px; line-height: 16px; }
.additional-result-filters>p { margin-top: 7px; font-size: 11px; }
.filter-conflict { color: var(--text); }
.filter-conflict .text-button { min-height: 24px; margin-left: 5px; }
.run-summary-panel { top: 41px; width: min(540px,calc(100% - 40px)); max-height: min(64vh,520px); font-size: 12px; }
.run-summary-panel h3, .run-summary-panel h4 { font-size: 12px; }
.run-summary-panel p { margin: 4px 0 8px; color: var(--muted); overflow-wrap: anywhere; }
.run-summary-panel table { width: 100%; margin: 6px 0; border-collapse: collapse; font-variant-numeric: tabular-nums; text-align: right; }
.run-summary-panel th, .run-summary-panel td { padding: 4px 8px; border-bottom: 1px solid var(--line); font-weight: 400; }
.run-summary-panel thead { background: var(--bg); color: var(--muted); white-space: nowrap; }
.run-summary-panel th:first-child { text-align: left; }
.run-summary-panel ul { display: grid; grid-template-columns: repeat(2,minmax(0,1fr)); gap: 4px 16px; list-style: none; margin: 4px 0 0; padding: 0; color: var(--muted); }
.run-summary-panel li { display: flex; justify-content: space-between; gap: 6px; }
.run-summary-panel li strong { flex: none; color: var(--text); font-variant-numeric: tabular-nums; }
.all-results-table { flex: 1 1 0; min-width: 0; min-height: 0; max-width: 100%; overflow: auto; overscroll-behavior: contain; border-top: 1px solid var(--line); border-bottom: 1px solid var(--line); scrollbar-gutter: stable; }
.all-results-table table { width: 100%; min-width: 690px; table-layout: fixed; border-collapse: separate; border-spacing: 0; font-size: 13px; text-align: left; }
.ip-column { width: 28%; }
.location-column { width: 16%; }
.outcome-column { width: 19%; }
.tcp-column { width: 20%; }
.download-column { width: 11%; }
.favorite-column { width: 6%; }
.all-results-table .initial-issues-table { min-width: 560px; }
.initial-issues-table .ip-column { width: 36%; }
.initial-issues-table .outcome-column { width: 29%; }
.initial-issues-table .tcp-column { width: 27%; }
.initial-issues-table .favorite-column { width: 8%; }
.all-results-table th { position: sticky; top: 0; z-index: 1; height: 32px; padding: 5px 20px; border-bottom: 1px solid var(--line); background: linear-gradient(var(--bg), color-mix(in srgb, var(--bg) 65%, var(--panel))); color: var(--muted); font-size: 11px; line-height: 21px; font-weight: 500; white-space: nowrap; box-shadow: 0 1px 0 color-mix(in srgb, var(--panel) 70%, transparent); }
.all-results-table td { height: 38px; padding: 3px 20px; border: 0; border-bottom: 1px solid var(--line); line-height: 20px; vertical-align: middle; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.all-results-table tbody tr:last-child td { border-bottom: 0; }
.all-results-table tbody tr.striped { background: color-mix(in srgb,var(--bg) 45%,var(--panel)); }
.all-results-table tbody tr { transition: background .12s; }
.all-results-table tbody tr:hover, .all-results-table tbody tr:focus-within { background: color-mix(in srgb, var(--brand) 5%, var(--panel)); }
.all-results-table tbody tr:hover td:first-child, .all-results-table tbody tr:focus-within td:first-child { box-shadow: inset 2px 0 0 color-mix(in srgb, var(--brand) 55%, var(--line)); }
.all-results-table .result-measurement-cell { padding-inline: 12px; text-align: right; }
.all-results-table th:last-child, .all-results-table td:last-child { min-width: 0; max-width: none; padding-inline: 4px; text-align: center; white-space: nowrap; }
.table-unit { display: inline-block; height: 15px; padding: 0 4px; margin-left: 3px; border: 1px solid color-mix(in srgb, var(--line) 85%, transparent); border-radius: 4px; background: color-mix(in srgb, var(--panel) 70%, transparent); font-weight: 400; font-size: 9px; line-height: 13px; vertical-align: 1px; }
.result-ip { font-size: 13px; font-weight: 500; }
.result-ip-cell { position: relative; display: inline-flex; align-items: center; gap: 8px; max-width: 100%; padding-left: 24px; }
.result-reason-trigger { position: absolute; left: 0; top: 50%; transform: translateY(-50%); width: 20px; height: 24px; min-height: 24px; padding: 2px; color: var(--muted); }
.result-reason-trigger:hover, .result-reason-trigger[aria-expanded="true"] { color: var(--brand); background: var(--soft); }
.result-reason-trigger[aria-expanded="true"]>svg { transform: rotate(90deg); }
.all-results-table .result-reason-row td { height: auto; padding: 8px 20px 8px 44px; background: var(--bg); font-size: 12px; text-align: left; white-space: normal; overflow-wrap: anywhere; }
.result-reason-row p { margin: 0; color: var(--muted); }
.result-copy { width: 24px; height: 24px; min-height: 24px; padding: 5px; color: var(--muted); opacity: 0; transition: opacity .12s, background .12s; }
.result-copy:disabled { opacity: 0; }
.result-copy>svg { width: 13px; height: 13px; }
.all-results-table tbody tr:hover .result-copy, .all-results-table tbody tr:focus-within .result-copy, .result-copy:focus-visible { opacity: 1; }
.result-copy:hover:not(:disabled) { color: var(--brand); background: color-mix(in srgb, var(--brand) 9%, transparent); }
.result-copy:focus-visible { outline: 2px solid color-mix(in srgb, var(--brand) 65%, transparent); outline-offset: 1px; }
.location-cell { display: inline-flex; align-items: center; gap: 7px; max-width: 100%; vertical-align: middle; }
.location-name { min-width: 0; overflow: hidden; text-overflow: ellipsis; }
.result-pop { display: inline-block; flex: 0 0 auto; height: 17px; padding: 0 4px; border: 1px solid color-mix(in srgb, var(--line) 75%, transparent); border-radius: 4px; color: var(--muted); background: color-mix(in srgb, var(--bg) 70%, var(--panel)); font-size: 10px; font-weight: 500; line-height: 15px; letter-spacing: .2px; }
.result-number { font-variant-numeric: tabular-nums; }
.result-outcome { display: inline-block; max-width: 100%; padding: 1px 6px; overflow: hidden; border-radius: 4px; background: var(--bg); color: var(--muted); font-size: 11px; font-weight: 500; line-height: 17px; text-overflow: ellipsis; vertical-align: middle; }
.result-outcome[data-tone="warning"] { background: var(--warn-bg); color: color-mix(in srgb, var(--warn) 72%, var(--text)); }
.result-outcome[data-tone="good"] { background: var(--good-bg); color: color-mix(in srgb, var(--good) 78%, var(--text)); }
.result-tcp-failures { display: block; margin-top: 2px; color: var(--muted); font-size: 11px; line-height: 14px; font-variant-numeric: tabular-nums; }
.result-tcp-value { display: inline-flex; align-items: baseline; gap: 6px; }
.result-tcp-value small, .result-value-missing { color: var(--muted); font-size: 11px; }
.result-value-missing { display: block; line-height: 16px; }
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
@media (max-width: 940px) {
  .all-results-filters>.more-filters-button .filter-button-label, .all-results-filters>.more-filters-button>svg:last-child { display: none; }
  .all-results-filters>.more-filters-button { padding-inline: 8px; }
}
@media (max-width: 800px) {
  .all-results-heading { gap: 8px; }
  .all-results-heading, .all-results-filters, .initial-issues-toolbar, .all-results-footer { padding-inline: 16px; }
  .additional-result-filters { right: 16px; width: min(600px,calc(100% - 32px)); }
  .run-summary-panel { right: 16px; width: min(540px,calc(100% - 32px)); }
  .all-results-filters { gap: 6px; }
  .all-results-filters :deep(.select-control>select), .all-results-filters input[type=search] { font-size: 11px; }
  .more-filters-button { gap: 4px; font-size: 11px; padding-inline: 6px; }
  .run-summary-button>svg:first-child { display: none; }
  .all-results-table th, .all-results-table td { padding-inline: 16px; }
  .location-cell { gap: 5px; }
  .result-pop { padding-inline: 3px; }
  .speed-cell { gap: 6px; }
  .all-results-table .result-measurement-cell { padding-inline: 10px; }
}
@media (max-width: 640px) {
  .all-results-filters, .all-results-filters.has-download-toggle { grid-template-columns: minmax(130px,1.4fr) minmax(85px,1fr) minmax(80px,.8fr); }
  .results-sort { grid-column: span 2; }
  .results-overview-counts { border-top: 1px solid var(--line); padding-top: 6px; }
  .results-overview-counts>div:first-child { border-left: 0; }
  .all-results-thresholds { grid-template-columns: minmax(160px,1.3fr) repeat(2,minmax(110px,1fr)); min-width: 440px; }
}
</style>
