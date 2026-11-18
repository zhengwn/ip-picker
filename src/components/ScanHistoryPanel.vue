<script setup lang="ts">
import { computed, nextTick, ref, shallowRef, watch } from 'vue'
import { invoke } from '@tauri-apps/api/core'
import { ArrowLeft, ChevronDown, ChevronLeft, ChevronRight, History, Info, LoaderCircle, RotateCcw, Search } from '@lucide/vue'
import { validManualIpv4 } from '../custom-ips'
import { comparisonContextDifferences, favoriteDate, favoriteError, favoriteNumber, indirectRouteWarning } from '../favorites'
import { estimateScan } from '../scan-estimate'
import type { TcpDisplayMetric } from '../result-preferences'
import { tcpLatencyChartScale } from '../result-row-display'
import type { IpResult, NetworkPathSnapshot, ScanConfig, ScanHistory, ScanHistoryKind, ScanHistoryMatch, ScanRun } from '../types'
import { candidateExclusionReason, candidateMeasurementStatus, hasPrecisionMeasurement, initialTcpFailureReason, observedFailurePercent, stabilityMetricLabel, targetHost, validateConfig } from '../ui-state'
import { scanModeLabel, singleIpConfig } from '../scan-experience'
import { useDismissiblePopover } from '../popovers'
import SelectControl from './SelectControl.vue'
import TcpDiagnostics from './TcpDiagnostics.vue'
import TcpLatencyCell from './TcpLatencyCell.vue'
import TcpLatencyHeader from './TcpLatencyHeader.vue'

const props = defineProps<{ desktop: boolean; active: boolean; revision: number; retestDisabled?: boolean }>()
const emit = defineEmits<{ retest: [ip: string, config: ScanConfig] }>()
const tcpDisplayMetric = defineModel<TcpDisplayMetric>('tcpDisplayMetric', { default: 'runMetric' })

type View = { mode: 'recent' } | { mode: 'compare'; ip: string } | { mode: 'run'; id: string; kind: ScanHistoryKind }
type Content = { mode: 'recent'; history: ScanHistory } | { mode: 'compare'; matches: ScanHistoryMatch[] } | { mode: 'run'; run: ScanRun | null }
const comparedIp = ref('')
const selectedRun = ref<{ id: string; kind: ScanHistoryKind } | null>(null)
const view = computed<View>(() => selectedRun.value ? { mode: 'run', ...selectedRun.value }
  : comparedIp.value ? { mode: 'compare', ip: comparedIp.value } : { mode: 'recent' })
const content = shallowRef<Content | null>(null)
const loading = ref(false)
const loadError = ref('')
const retry = ref(0)
const ipInput = ref('')
const inputError = ref('')
const baselineError = ref('')
const savingBaseline = ref(false)
const baselineAction = ref<HTMLButtonElement | null>(null)
const searchField = ref<HTMLInputElement | null>(null)
const backButton = ref<HTMLButtonElement | null>(null)
const table = ref<HTMLElement | null>(null)
const infoOpen = ref(false)
const infoTrigger = ref<HTMLButtonElement | null>(null)
const infoPanel = ref<HTMLElement | null>(null)
useDismissiblePopover(infoOpen, infoPanel, infoTrigger)
const page = ref(1)
const pageSize = 50
const expandedResult = ref<string | null>(null)
const runScope = ref<'precision' | 'all'>('precision')
const runIpQuery = ref('')
const runPop = ref('')
const runStatus = ref('')

async function fetchView(view: View): Promise<Content> {
  if (view.mode === 'run') return { mode: 'run', run: await invoke<ScanRun | null>('get_scan_history_run', { runId: view.id }) }
  if (view.mode === 'compare') return { mode: 'compare', matches: await invoke<ScanHistoryMatch[]>('compare_scan_history_ip', { ip: view.ip }) }
  return { mode: 'recent', history: await invoke<ScanHistory>('get_scan_history') }
}

watch([() => props.active, () => props.revision, view, retry], async ([active, , current], _previous, onCleanup) => {
  if (!props.desktop || !active) return
  let currentRequest = true
  onCleanup(() => { currentRequest = false })
  loading.value = true
  loadError.value = ''
  content.value = null
  try {
    const loaded = await fetchView(current)
    if (currentRequest) content.value = loaded
  } catch (error) {
    if (currentRequest) loadError.value = error instanceof Error ? error.message : String(error)
  } finally {
    if (currentRequest) loading.value = false
  }
}, { immediate: true })

watch(view, () => {
  infoOpen.value = false
  baselineError.value = ''
  runScope.value = 'precision'
  clearRunFilters()
  page.value = 1
  expandedResult.value = null
})
watch(() => props.active, active => { if (!active) infoOpen.value = false })
watch(ipInput, () => { inputError.value = '' }, { flush: 'sync' })

function compareIp(value = ipInput.value, focusComparison = !!selectedRun.value) {
  const ip = value.trim()
  if (!validManualIpv4(ip)) {
    inputError.value = '请输入完整的 IPv4 地址。'
    searchField.value?.focus()
    return
  }
  ipInput.value = ip
  inputError.value = ''
  comparedIp.value = ip
  selectedRun.value = null
  if (focusComparison) void focusBack()
}

function openComparison(ip: string) { compareIp(ip, true) }
defineExpose({ openComparison })

async function focusBack() {
  await nextTick()
  backButton.value?.focus()
}

function openRun(id: string, kind: ScanHistoryKind) {
  selectedRun.value = { id, kind }
  void focusBack()
}

async function goBack() {
  if (selectedRun.value) selectedRun.value = null
  else comparedIp.value = ''
  await nextTick()
  searchField.value?.focus()
}

const run = computed(() => content.value?.mode === 'run' ? content.value.run : null)
const history = computed(() => content.value?.mode === 'recent' ? content.value.history : null)
const matches = computed(() => content.value?.mode === 'compare' ? content.value.matches : [])
const baseline = computed(() => matches.value.find(match => match.baseline))
const baselineConfig = computed(() => {
  const config = baseline.value?.run.config
  if (!config?.stabilityFirst || config.tcpLatencyScreening) return null
  const snapshot = singleIpConfig(config, comparedIp.value)
  return validateConfig(snapshot) ? null : snapshot
})
const baselineEstimate = computed(() => baselineConfig.value ? estimateScan(baselineConfig.value) : null)

async function setBaseline(runId: string | null) {
  if (!props.desktop || savingBaseline.value || view.value.mode !== 'compare') return
  const ip = comparedIp.value
  savingBaseline.value = true
  baselineError.value = ''
  try {
    const matches = await invoke<ScanHistoryMatch[]>('set_scan_history_baseline', { ip, runId })
    if (view.value.mode !== 'compare' || comparedIp.value !== ip) return
    if (loading.value) retry.value++
    else content.value = { mode: 'compare', matches }
    await nextTick()
    if (view.value.mode === 'compare' && comparedIp.value === ip) {
      (baselineAction.value ?? backButton.value)?.focus({ preventScroll: true })
    }
  } catch (error) {
    if (view.value.mode === 'compare' && comparedIp.value === ip) baselineError.value = favoriteError(error)
  } finally { savingBaseline.value = false }
}

function retestBaseline() {
  if (baselineConfig.value && !props.retestDisabled) emit('retest', comparedIp.value, baselineConfig.value)
}
const runRows = computed(() => {
  const current = run.value
  if (!current) return []
  const results = runScope.value === 'precision' ? current.results.filter(hasPrecisionMeasurement) : current.results
  return results.map(result => ({
    result,
    pop: result.colo || result.observedColos[0] || '',
    status: candidateMeasurementStatus(result, current),
  }))
})
const runPopOptions = computed(() => [...new Set(runRows.value.map(row => row.pop).filter(Boolean))].sort())
const runStatusOptions = computed(() => [...new Set(runRows.value.map(row => row.status))].sort((a, b) => a.localeCompare(b, 'zh-CN')))
const filteredRunRows = computed(() => {
  const ip = runIpQuery.value.trim()
  return runRows.value.filter(row => (!ip || row.result.ip.includes(ip))
    && (!runPop.value || row.pop === runPop.value)
    && (!runStatus.value || row.status === runStatus.value))
})
const runHasFilters = computed(() => !!(runIpQuery.value.trim() || runPop.value || runStatus.value))
const pageCount = computed(() => Math.max(1, Math.ceil(filteredRunRows.value.length / pageSize)))
const currentPage = computed(() => Math.min(page.value, pageCount.value))
const pageStart = computed(() => (currentPage.value - 1) * pageSize)
const tcpScale = computed(() => tcpLatencyChartScale(run.value?.results ?? matches.value.flatMap(match => match.run.results)))

function clearRunFilters() {
  runIpQuery.value = ''
  runPop.value = ''
  runStatus.value = ''
}

function resultToggleId(key: string) { return `history-result-${encodeURIComponent(key)}` }
async function toggleResult(key: string) {
  const closing = expandedResult.value === key
  expandedResult.value = closing ? null : key
  await nextTick()
  if (closing) document.getElementById(resultToggleId(key))?.focus({ preventScroll: true })
  else {
    const detail = document.getElementById('history-tcp-detail')
    detail?.scrollIntoView({ block: 'nearest' })
    detail?.focus({ preventScroll: true })
  }
}

watch([runPopOptions, runStatusOptions], ([pops, statuses]) => {
  if (!run.value) return
  if (runPop.value && !pops.includes(runPop.value)) runPop.value = ''
  if (runStatus.value && !statuses.includes(runStatus.value)) runStatus.value = ''
})
watch([runScope, runIpQuery, runPop, runStatus], () => { page.value = 1 })
watch([runScope, runIpQuery, runPop, runStatus, currentPage], async () => {
  expandedResult.value = null
  await nextTick()
  if (table.value) table.value.scrollTop = 0
})

function comparisonSettings(config: ScanConfig) {
  const initialScreening = config.tcpLatencyScreening
    ? config.maxTcpMs == null ? '开启 · 无延迟上限' : `开启 · 单次延迟 ≤ ${config.maxTcpMs} ms`
    : '关闭 · 仅检查连通性'
  const probes = config.stabilityAttempts == null ? '无稳定性计划' : `${config.stabilityAttempts} 次 · 间隔 ${config.stabilityIntervalMs} ms`
  const threshold = config.maxTcpMs == null ? '无延迟上限' : `${stabilityMetricLabel(config.stabilityLatencyMetric)} ≤ ${config.maxTcpMs} ms`
  const budget = config.downloadBudgetEnabled === false ? '不限流量' : `${config.downloadBudgetMiB} MiB`
  return [
    { label: '测速目标', value: config.targetUrl },
    { label: 'TCP 初筛', value: initialScreening },
    { label: 'TCP 探测', value: `${probes} · 超时 ${config.connectTimeoutMs} ms` },
    { label: 'TCP 判定', value: config.stabilityAttempts == null ? '无稳定性判定' : `${threshold} · 失败率 ≤ ${config.maxStabilityFailurePercent}%` },
    { label: 'HTTPS 验证', value: `${config.verificationAttempts} 次 · 超时 ${config.requestTimeoutMs} ms` },
    { label: '下载计划', value: `${config.downloadRounds} 轮 × ${config.downloadSeconds} 秒 · ${budget}` },
  ]
}

const settingsByRun = computed(() => new Map(matches.value.map(({ run }) => [run.id, comparisonSettings(run.config)])))
const runSettings = computed(() => run.value ? comparisonSettings(run.value.config) : [])
const changedSettings = computed(() => {
  const settings = [...settingsByRun.value.values()]
  return new Set(settings[0]?.filter((field, index) => settings.some(fields => fields[index].value !== field.value)).map(field => field.label))
})

const comparisonWarnings = computed(() => {
  const first = (baseline.value ?? matches.value[0])?.run
  if (!first) return []
  const warnings = matches.value.filter(match => match.run.id !== first.id).flatMap(({ run }) => comparisonContextDifferences(
    { config: first.config, networkPath: pathFor(first, first.results[0]), colo: first.results[0]?.colo ?? '' },
    { config: run.config, networkPath: pathFor(run, run.results[0]), colo: run.results[0]?.colo ?? '' },
    comparedIp.value,
  ))
  if (matches.value.some(({ run }) => {
    const path = pathFor(run, run.results[0])
    return path && path.sampleIp !== comparedIp.value
  })) warnings.push(indirectRouteWarning)
  return [...new Set(warnings)]
})
const infoLabel = computed(() => view.value.mode === 'run' ? '本轮详情'
  : changedSettings.value.size || comparisonWarnings.value.some(warning => ['测速参数不同', '网络接口不同', '接入机房不同'].includes(warning)) ? '对比条件有差异'
  : comparisonWarnings.value.length ? '对比条件待确认' : '对比说明')
const rows = computed(() => {
  const current = run.value
  const source = current
    ? filteredRunRows.value.slice(pageStart.value, pageStart.value + pageSize).map(({ result, status }) => ({ run: current, result, kind: null, status, baseline: false, baselineOnly: false }))
    : matches.value.flatMap(match => match.run.results.map(result => ({ ...match, result, status: candidateMeasurementStatus(result, match.run) })))
  return source.map(({ run, result, kind, status, baseline, baselineOnly }) => {
    const samples = result.downloadSamplesMbps
    const speed = samples.length > 0 ? favoriteNumber(result.downloadMbps) : '—'
    return {
      key: `${run.id}-${result.ip}`,
      run,
      result,
      kind,
      baseline,
      baselineOnly,
      canPin: !!(initialTcpFailureReason(result) || result.tcpMs > 0 || result.attempts > 0 || result.tcpStability?.probesMs?.length || result.downloadSamplesMbps.length),
      networkPath: pathFor(run, result),
      pop: result.colo || result.observedColos[0] || '—',
      failureRate: observedFailurePercent(result.tcpStability),
      initial: result.tcpMs > 0 ? favoriteNumber(result.tcpMs) : '—',
      speed,
      downloadPartial: speed !== '—' && samples.length < run.config.downloadRounds,
      reason: candidateExclusionReason(result) || result.tcpStability?.earlyRejection,
      status,
      settings: settingsByRun.value.get(run.id) ?? runSettings.value,
    }
  })
})

const statusText = { running: '测速中', completed: '已完成', cancelled: '已停止', failed: '失败' } as const
const statusClass = { running: 'active', completed: 'good', cancelled: 'quiet', failed: 'error' } as const
const kindText = { scan: '地区优选', favorite: '收藏复测' } as const
const interfaceText = { wired: '有线', wifi: 'Wi‑Fi', tunnel: '隧道', other: '' } as const

function pathFor(run: ScanRun, result?: IpResult) {
  return result?.networkPath ?? run.progress.networkPath
}

function interfaceLabel(path?: NetworkPathSnapshot | null) {
  if (!path) return '未记录'
  const kind = path.interfaceKind ? interfaceText[path.interfaceKind] : path.tunnelInterface ? '隧道' : ''
  return kind ? `${kind} · ${path.interfaceName}` : path.interfaceName
}

function interfaceDetail(path?: NetworkPathSnapshot | null) {
  return path ? `本轮开始时的路由采样 ${path.sampleIp} · ${favoriteDate(path.capturedAt)}` : '本轮未取得网络接口'
}
const failureRateFormat = new Intl.NumberFormat('zh-CN', { maximumFractionDigits: 1 })
function failurePercentLabel(value: number | null) { return value == null ? '未记录' : `${failureRateFormat.format(value)}%` }
</script>

<template>
  <section class="history-panel" aria-labelledby="scan-history-title">
    <header class="history-toolbar">
      <div class="history-heading">
        <button v-if="view.mode !== 'recent'" ref="backButton" type="button" class="text-button history-back" @click="goBack"><ArrowLeft :size="13" aria-hidden="true" />{{ view.mode === 'run' && comparedIp ? '返回对比' : '返回历史' }}</button>
        <h2 id="scan-history-title">{{ view.mode === 'compare' ? 'IP 对比' : view.mode === 'run' ? '本轮结果' : '测速历史' }}</h2>
        <span v-if="history" class="history-count">最近 {{ history.limit }} 轮</span>
        <template v-if="view.mode === 'compare'"><strong class="mono history-current-ip">{{ view.ip }}</strong><span v-if="!loading && !loadError" class="history-count">{{ matches.length }} 轮</span></template>
        <template v-if="run"><time class="history-current-time" :datetime="run.startedAt" :title="favoriteDate(run.startedAt)">{{ favoriteDate(run.startedAt) }}</time><span :class="['status-label', statusClass[run.status]]">{{ statusText[run.status] }}</span></template>
      </div>
      <form class="history-search" role="search" aria-label="按 IP 对比测速历史" @submit.prevent="compareIp()">
        <label class="history-search-field"><span class="visually-hidden">输入完整 IPv4 地址</span><Search :size="14" aria-hidden="true" /><input id="history-ip-search" ref="searchField" v-model="ipInput" type="search" inputmode="decimal" autocomplete="off" spellcheck="false" placeholder="输入 IP 对比各轮结果" :disabled="!desktop" :aria-invalid="!!inputError" :aria-describedby="inputError ? 'history-ip-error' : undefined" /></label>
        <button type="submit" class="button primary" :disabled="!desktop || !ipInput.trim()">对比 IP</button>
      </form>
      <button v-if="run || matches.length" ref="infoTrigger" class="button history-info-trigger" :class="{ active: infoOpen, warning: view.mode === 'compare' && comparisonWarnings.length }" :aria-expanded="infoOpen" aria-controls="history-info-panel" @click="infoOpen = !infoOpen"><Info :size="13" aria-hidden="true" />{{ infoLabel }}<ChevronDown :size="12" aria-hidden="true" /></button>
      <section v-if="infoOpen && (run || matches.length)" id="history-info-panel" ref="infoPanel" class="history-info-panel" aria-labelledby="history-info-title" tabindex="0">
        <template v-if="run">
          <h3 id="history-info-title">本轮详情</h3>
          <p>{{ view.mode === 'run' && view.kind === 'favorite' ? '收藏复测' : scanModeLabel(run) }} · {{ favoriteDate(run.startedAt) }} · {{ statusText[run.status] }}</p>
          <p v-if="run.error" class="history-detail-error">{{ run.error }}</p>
          <dl class="history-settings-list"><div v-for="setting in runSettings" :key="setting.label"><dt>{{ setting.label }}</dt><dd>{{ setting.value }}</dd></div></dl>
          <p>{{ interfaceLabel(run.progress.networkPath) }} · {{ interfaceDetail(run.progress.networkPath) }}</p>
        </template>
        <template v-else>
          <h3 id="history-info-title">对比说明</h3>
          <p class="mono">{{ comparedIp }} · {{ matches.length }} 轮记录</p>
          <p v-if="comparisonWarnings.length" class="history-comparison-warning">{{ comparisonWarnings.join(' · ') }}；数值差异不能直接归因于网络质量。</p>
          <p v-else>接口相同也不保证网络环境相同，单次观测的变化不代表长期趋势。</p>
          <p>“采样”标记表示接口来自其他 IP 的路由观测。每行详情保留完整参数、采样地址和 TCP 明细。</p>
          <div v-for="field in changedSettings" :key="field" class="history-difference-group">
            <h4>{{ field }}</h4>
            <dl><div v-for="match in matches" :key="match.run.id"><dt>{{ favoriteDate(match.run.startedAt) }}</dt><dd>{{ settingsByRun.get(match.run.id)?.find(setting => setting.label === field)?.value }}</dd></div></dl>
          </div>
          <p v-if="!changedSettings.size">已记录的测速参数一致。</p>
        </template>
      </section>
    </header>
    <p v-if="inputError" id="history-ip-error" class="history-input-error" role="alert">{{ inputError }}</p>
    <p v-if="baselineError" class="history-input-error" role="alert">{{ baselineError }}</p>
    <div v-if="baseline" class="history-baseline-bar">
      <strong>对比基线</strong><time :datetime="baseline.run.startedAt">{{ favoriteDate(baseline.run.startedAt) }}</time>
      <template v-if="baselineEstimate">
        <span id="baseline-retest-cost" :title="baselineEstimate.detail">{{ baselineEstimate.duration }} · 下载正文 {{ baselineEstimate.download }}</span>
        <button type="button" class="button" :disabled="retestDisabled" aria-describedby="baseline-retest-cost" @click="retestBaseline"><RotateCcw :size="12" aria-hidden="true" />按基线参数复测</button>
      </template>
      <span v-else>该轮参数与当前测速规则不兼容</span>
      <button ref="baselineAction" type="button" class="text-button" :aria-disabled="savingBaseline" @click="setBaseline(null)">取消固定</button>
    </div>
    <div v-if="run?.results.length" class="history-filters" role="group" aria-label="筛选本轮测速记录">
      <SelectControl compact><select v-model="runScope" aria-label="本轮结果范围"><option value="precision">精测记录</option><option value="all">全部 IP</option></select></SelectControl>
      <label class="history-search-field history-run-search"><span class="visually-hidden">搜索本轮 IP 地址</span><Search :size="14" aria-hidden="true" /><input v-model="runIpQuery" type="search" autocomplete="off" spellcheck="false" placeholder="搜索本轮 IP" /></label>
      <SelectControl compact><select v-model="runPop" aria-label="本轮机房（POP）"><option value="">全部 POP</option><option v-for="pop in runPopOptions" :key="pop" :value="pop">{{ pop }}</option></select></SelectControl>
      <SelectControl compact><select v-model="runStatus" aria-label="本轮测速状态"><option value="">全部状态</option><option v-for="status in runStatusOptions" :key="status" :value="status">{{ status }}</option></select></SelectControl>
    </div>

    <div v-if="loading" class="history-empty" role="status"><LoaderCircle :size="20" class="spinning" aria-hidden="true" /><p>正在读取测速历史…</p></div>
    <div v-else-if="loadError" class="history-empty"><div role="alert"><h3>读取测速历史失败</h3><p>{{ loadError }}</p></div><button type="button" class="button" @click="retry++">重试</button></div>
    <div v-else-if="history?.entries.length" class="history-table-wrap" role="region" aria-label="最近测速记录" tabindex="0">
      <table class="history-table run-list">
        <thead><tr><th scope="col">开始时间</th><th scope="col">测速类型</th><th scope="col" title="本轮开始时采集的路由接口">网络接口</th><th scope="col">状态</th><th scope="col">记录 IP 数</th><th scope="col">目标</th><th scope="col"><span class="visually-hidden">操作</span></th></tr></thead>
        <tbody><tr v-for="entry in history.entries" :key="entry.id">
          <td class="history-time"><time :datetime="entry.startedAt">{{ favoriteDate(entry.startedAt) }}</time></td>
          <td>{{ entry.kind === 'scan' ? scanModeLabel(entry) : kindText[entry.kind] }}</td>
          <td><span class="history-network" :title="interfaceDetail(entry.networkPath)">{{ interfaceLabel(entry.networkPath) }}</span></td>
          <td><span :class="['status-label', statusClass[entry.status]]">{{ statusText[entry.status] }}</span></td>
          <td class="history-number">{{ entry.resultCount.toLocaleString('zh-CN') }}</td>
          <td class="history-host" :title="entry.targetUrl">{{ targetHost(entry.targetUrl) }}</td>
          <td><button type="button" class="text-button history-row-action" :aria-label="`查看 ${favoriteDate(entry.startedAt)} 的${kindText[entry.kind]}结果`" @click="openRun(entry.id, entry.kind)">查看<ChevronRight :size="12" aria-hidden="true" /></button></td>
        </tr></tbody>
      </table>
    </div>
    <div v-else-if="rows.length" ref="table" class="history-table-wrap" role="region" :aria-label="view.mode === 'compare' ? '同一 IP 的各轮测速结果' : '本轮 IP 测速记录'" tabindex="0">
      <table class="history-table result-list" :class="{ 'comparison-list': view.mode === 'compare' }">
        <thead><tr>
          <th scope="col">{{ view.mode === 'compare' ? '开始时间 / 目标' : 'IP 地址' }}</th>
          <th scope="col">{{ view.mode === 'compare' ? '网络 / POP' : 'POP' }}</th>
          <th scope="col" title="已执行 / 计划探测次数，失败率按已执行探测计算">TCP 探测</th>
          <th scope="col"><TcpLatencyHeader v-model="tcpDisplayMetric" run-metric="p95" /></th>
          <th scope="col">下载 <span class="history-unit">Mbps</span></th>
          <th scope="col">结果 / 详情</th>
        </tr></thead>
        <tbody>
          <template v-for="row in rows" :key="row.key">
            <tr class="history-data-row">
              <td v-if="row.kind" class="history-run-cell">
                <time v-if="row.baselineOnly" class="history-time" :datetime="row.run.startedAt">{{ favoriteDate(row.run.startedAt) }}</time>
                <button v-else type="button" class="text-button history-row-action history-time" :title="`查看本轮${kindText[row.kind]}结果`" @click="openRun(row.run.id, row.kind)"><time :datetime="row.run.startedAt">{{ favoriteDate(row.run.startedAt) }}</time><ChevronRight :size="12" aria-hidden="true" /></button>
                <span class="history-secondary history-host" :title="row.run.config.targetUrl">{{ row.baseline ? '基线 · ' : '' }}{{ targetHost(row.run.config.targetUrl) }}</span>
              </td>
              <td v-else><button type="button" class="text-button history-row-action mono" :aria-label="`对比 IP ${row.result.ip} 的历史结果`" @click="compareIp(row.result.ip)">{{ row.result.ip }}<Search :size="12" aria-hidden="true" /></button></td>
              <td>
                <span v-if="row.kind" class="history-network" :title="interfaceDetail(row.networkPath)">{{ interfaceLabel(row.networkPath) }}</span>
                <span class="history-location"><span class="mono" :title="row.result.observedColos.join(' / ')">{{ row.pop }}</span><span v-if="row.kind && row.networkPath && row.networkPath.sampleIp !== row.result.ip" class="history-route-tag" :title="`接口来自 ${row.networkPath.sampleIp} 的路由样本`">采样</span></span>
              </td>
              <td class="history-number">
                <template v-if="row.result.tcpStability?.attempted">
                  {{ row.result.tcpStability.attempted }} / {{ row.result.tcpStability.requested }}
                  <small class="history-secondary history-failure-rate" :class="{ 'history-failures': row.result.tcpStability.failed }" :title="`失败 ${row.result.tcpStability.failed} 次（${failurePercentLabel(row.failureRate)}）`">失败 {{ row.result.tcpStability.failed }} · {{ failurePercentLabel(row.failureRate) }}</small>
                </template><span v-else class="muted">—</span>
              </td>
              <td class="history-number history-latency"><TcpLatencyCell :result="row.result" :display-metric="tcpDisplayMetric" :scale-max-ms="tcpScale" /></td>
              <td class="history-number">{{ row.speed }}<small v-if="row.downloadPartial" class="history-secondary">{{ row.result.downloadSamplesMbps.length }} / {{ row.run.config.downloadRounds }} 轮</small></td>
              <td class="history-result"><button :id="resultToggleId(row.key)" type="button" class="text-button history-result-toggle" :title="row.reason || '查看参数、网络采样和 TCP 明细'" :aria-label="`${expandedResult === row.key ? '收起' : '查看'} ${row.result.ip} 在 ${favoriteDate(row.run.startedAt)} 的参数与测速详情`" :aria-expanded="expandedResult === row.key" :aria-controls="expandedResult === row.key ? 'history-tcp-detail' : undefined" @click="toggleResult(row.key)"><span>{{ row.status }}</span><ChevronDown :size="12" aria-hidden="true" /></button></td>
            </tr>
            <tr v-if="expandedResult === row.key" class="history-detail-row"><td colspan="6">
              <section id="history-tcp-detail" tabindex="-1" :aria-label="`${row.result.ip} 的参数与测速详情`">
                <div class="history-detail-heading"><strong class="mono">{{ row.result.ip }}</strong><span>{{ favoriteDate(row.run.startedAt) }}</span><button v-if="row.kind && row.canPin && !row.baseline" type="button" class="button history-pin-button" :disabled="savingBaseline" @click="setBaseline(row.run.id)">{{ baseline ? '换作基线' : '固定为基线' }}</button><button type="button" class="text-button" @click="toggleResult(row.key)">收起详情<ChevronDown :size="12" class="history-collapse" aria-hidden="true" /></button></div>
                <p v-if="row.reason" class="history-detail-reason">{{ row.reason }}</p>
                <div class="history-evidence-meta">
                  <section class="history-detail-settings" aria-label="本轮实际参数"><h4>测试参数</h4><dl class="history-settings-list"><div v-for="setting in row.settings" :key="setting.label" :class="{ 'setting-diff': view.mode === 'compare' && changedSettings.has(setting.label) }"><dt>{{ setting.label }}</dt><dd>{{ setting.value }}</dd></div></dl></section>
                  <section aria-label="本轮网络采样"><h4>网络与初筛</h4><dl class="history-settings-list"><div><dt>初筛延迟</dt><dd>{{ row.initial }} ms</dd></div><div><dt>路由接口</dt><dd>{{ interfaceLabel(row.networkPath) }}</dd></div><template v-if="row.networkPath"><div><dt>采样 IP</dt><dd class="mono">{{ row.networkPath.sampleIp }}{{ row.networkPath.sampleIp === row.result.ip ? '（本 IP）' : '（其他 IP 的路径样本）' }}</dd></div><div><dt>采样时间</dt><dd>{{ favoriteDate(row.networkPath.capturedAt) }}</dd></div></template><div><dt>观测 POP</dt><dd>{{ row.result.observedColos.join(' / ') || '未记录' }}</dd></div></dl></section>
                </div>
                <TcpDiagnostics :result="row.result" :config="row.run.config" />
              </section>
            </td></tr>
          </template>
        </tbody>
      </table>
    </div>
    <div v-else class="history-empty">
      <History :size="25" aria-hidden="true" />
      <h3 v-if="!desktop">请在桌面应用查看测速历史</h3><h3 v-else-if="view.mode === 'compare'">历史中未找到这个 IP</h3>
      <template v-else-if="run?.results.length"><template v-if="runScope === 'precision' && !runRows.length"><h3>本轮没有精测记录</h3><button type="button" class="button" @click="runScope = 'all'">查看全部 IP</button></template><h3 v-else>没有符合筛选条件的 IP</h3></template>
      <h3 v-else-if="view.mode === 'run'">{{ run ? '本轮没有 IP 记录' : '找不到这轮测速' }}</h3><h3 v-else>暂无测速历史</h3>
    </div>
    <footer v-if="run?.results.length" class="history-footer">
      <span class="small muted" aria-live="polite">匹配 {{ filteredRunRows.length.toLocaleString('zh-CN') }} / {{ run.results.length.toLocaleString('zh-CN') }} 个 IP</span>
      <button v-if="runHasFilters" type="button" class="text-button history-clear-filters" @click="clearRunFilters">清除筛选</button>
      <nav v-if="pageCount > 1" class="history-pages" aria-label="历史结果分页"><span class="small muted">{{ currentPage }} / {{ pageCount }} 页</span><button type="button" class="icon-button" :disabled="currentPage <= 1" aria-label="上一页" @click="page = currentPage - 1"><ChevronLeft :size="15" aria-hidden="true" /></button><button type="button" class="icon-button" :disabled="currentPage >= pageCount" aria-label="下一页" @click="page = currentPage + 1"><ChevronRight :size="15" aria-hidden="true" /></button></nav>
    </footer>
  </section>
</template>

<style scoped>
.history-baseline-bar { display: flex; flex-wrap: wrap; align-items: center; gap: 4px 12px; padding: 4px 16px; border-bottom: 1px solid var(--line); background: var(--surface-sunken); color: var(--muted); font-size: 11px; }
.history-baseline-bar strong { color: var(--text); }
.history-baseline-bar .button, .history-pin-button { min-height: 24px; height: 24px; font-size: 11px; }
.history-panel { display: flex; flex: 1; flex-direction: column; min-width: 0; min-height: 0; overflow: hidden; background: var(--panel); }
.history-toolbar { display: flex; position: relative; z-index: 3; flex: none; align-items: center; gap: 12px; min-height: 48px; padding: 8px 20px; border-bottom: 1px solid var(--line); }
.history-heading, .history-search, .history-pages { display: flex; align-items: center; gap: 8px; min-width: 0; }
.history-heading { flex: 0 1 auto; }
.history-heading h2 { flex: none; font-size: 14px; white-space: nowrap; }
.history-count { flex: none; color: var(--muted); font-size: 11px; white-space: nowrap; }
.history-current-ip { flex: none; font-size: 12px; font-weight: 500; }
.history-current-time { min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; color: var(--muted); font-size: 11px; }
.history-heading .status-label { flex: none; font-size: 10px; }
.history-back { flex: none; min-height: 28px; font-size: 11px; }
.history-search { flex: 0 1 320px; min-width: 180px; margin-left: auto; }
.history-search-field { display: block; position: relative; flex: 1; min-width: 0; }
.history-search-field>svg { position: absolute; top: 50%; left: 9px; color: var(--muted); transform: translateY(-50%); pointer-events: none; }
.history-search-field input[type=search] { height: 30px; min-height: 30px; padding-left: 30px; font-size: 12px; }
.history-search>.button { flex: none; height: 30px; min-height: 30px; font-size: 12px; }
.history-input-error { flex: none; margin: 0; padding: 5px 20px; color: var(--error); background: var(--error-bg); font-size: 11px; }
.history-info-trigger { flex: none; min-height: 30px; height: 30px; padding: 0 8px; gap: 5px; font-size: 11px; }
.history-info-trigger.active { background: var(--soft); border-color: var(--selected-border); color: var(--brand); }
.history-info-trigger.warning { color: var(--warn); }
.history-info-panel { position: absolute; top: calc(100% - 1px); right: 20px; z-index: 8; width: min(520px,calc(100% - 40px)); max-height: min(65vh,520px); padding: 14px; overflow: auto; overscroll-behavior: contain; background: var(--surface-raised); border: 1px solid var(--line); border-radius: 8px; box-shadow: 0 10px 28px #2328491c; font-size: 12px; line-height: 20px; overflow-wrap: anywhere; }
.history-info-panel h3 { margin: 0; font-size: 13px; }
.history-info-panel h4 { margin: 0 0 6px; font-size: 12px; }
.history-info-panel p { margin: 6px 0; color: var(--muted); }
.history-info-panel .history-comparison-warning { color: var(--warn); }
.history-info-panel .history-detail-error { color: var(--error); }
.history-difference-group { margin-top: 12px; padding-top: 10px; border-top: 1px solid var(--line); }
.history-difference-group dl { margin: 0; }
.history-difference-group dl>div { display: grid; grid-template-columns: 126px minmax(0,1fr); gap: 10px; margin-top: 5px; }
.history-difference-group dt { color: var(--muted); font-size: 11px; }
.history-difference-group dd { margin: 0; overflow-wrap: anywhere; }
.history-filters { display: grid; flex: none; grid-template-columns: minmax(100px,.8fr) minmax(130px,1.5fr) minmax(84px,.8fr) minmax(160px,1.2fr); align-items: center; gap: 8px; padding: 0 20px 8px; }
.history-filters :deep(select) { height: 30px; min-height: 30px; font-size: 12px; }
.history-table-wrap { flex: 1; min-height: 0; min-width: 0; overflow: auto; background: var(--panel); }
.history-table { width: 100%; min-width: 720px; border-spacing: 0; table-layout: fixed; background: var(--panel); font-size: 13px; }
.history-table th { position: sticky; top: 0; z-index: 1; height: 32px; background: var(--bg); color: var(--muted); font-size: 11px; font-weight: 500; }
.history-table th, .history-table td { padding: 3px 8px; border-bottom: 1px solid var(--line-subtle); text-align: left; vertical-align: middle; }
.history-table th:first-child, .history-table td:first-child { padding-left: 20px; }
.history-table th:last-child, .history-table td:last-child { padding-right: 20px; }
.history-table tbody tr:not(.history-detail-row)>td { height: 40px; }
.history-table tbody tr:nth-child(even) { background: var(--surface-sunken); }
.history-table tbody tr:hover { background: color-mix(in srgb,var(--soft) 52%,var(--panel)); }
.run-list th:first-child { width: 22%; }
.run-list th:nth-child(2), .run-list th:nth-child(4), .run-list th:nth-child(5) { width: 10%; }
.run-list th:nth-child(3) { width: 16%; }
.run-list th:last-child { width: 72px; }
.result-list th:first-child { width: 24%; }
.result-list th:nth-child(2) { width: 12%; }
.result-list th:nth-child(3) { width: 12%; }
.result-list th:nth-child(4) { width: 23%; }
.result-list th:nth-child(5) { width: 11%; }
.result-list th:last-child { width: 18%; }
.history-time, .history-number { font-variant-numeric: tabular-nums; white-space: nowrap; }
.history-unit { color: var(--muted); font-size: 10px; }
.history-secondary { display: block; color: var(--muted); font-size: 11px; line-height: 14px; }
.history-failure-rate { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.history-failures { color: var(--warn); }
.history-network { display: block; color: var(--muted); font-size: 12px; line-height: 16px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.history-location { display: flex; align-items: center; gap: 5px; min-width: 0; line-height: 16px; }
.history-route-tag { padding: 0 3px; border: 1px solid var(--line); border-radius: 3px; color: var(--muted); font-size: 10px; line-height: 14px; }
.history-host { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.history-row-action { max-width: 100%; min-height: 20px; height: 20px; padding: 0; font-size: 12px; white-space: nowrap; }
.history-row-action>time { min-width: 0; overflow: hidden; text-overflow: ellipsis; }
.history-row-action>svg { flex: none; }
.history-latency { white-space: normal; }
.history-latency :deep(.tcp-latency-value) { flex: none; }
.history-result-toggle { display: flex; justify-content: space-between; width: 100%; min-width: 0; min-height: 28px; padding: 0; gap: 5px; color: var(--text); font-size: 12px; }
.history-result-toggle>span { min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.history-result-toggle>svg { flex: none; color: var(--muted); }
.history-result-toggle[aria-expanded="true"] svg, .history-collapse { transform: rotate(180deg); }
#history-tcp-detail { scroll-margin-top: 36px; }
.history-detail-row>td { padding: 10px 20px; background: var(--surface-sunken); }
.history-detail-heading { display: flex; align-items: center; gap: 12px; color: var(--muted); font-size: 11px; }
.history-detail-heading>strong { color: var(--text); font-size: 13px; }
.history-detail-heading>.text-button { margin-left: auto; }
.history-detail-reason { margin: 6px 0; color: var(--muted); font-size: 12px; overflow-wrap: anywhere; }
.history-evidence-meta { display: grid; grid-template-columns: minmax(0,1.25fr) minmax(0,1fr); gap: 20px; margin: 10px 0 14px; padding-bottom: 10px; border-bottom: 1px solid var(--line); font-size: 12px; }
.history-evidence-meta h4 { margin: 0 0 6px; font-size: 12px; }
.history-settings-list { margin: 0; }
.history-settings-list>div { display: grid; grid-template-columns: 65px minmax(0,1fr); gap: 7px; margin-top: 4px; }
.history-settings-list dt { color: var(--muted); font-size: 11px; }
.history-settings-list dd { margin: 0; overflow-wrap: anywhere; }
.setting-diff dt { color: var(--brand); font-weight: 600; }
.history-footer { display: flex; flex: none; justify-content: space-between; align-items: center; min-height: 30px; gap: 12px; padding: 0 20px; border-top: 1px solid var(--line); }
.history-clear-filters { margin-left: auto; }
.history-empty { display: flex; flex: 1; align-items: center; justify-content: center; flex-direction: column; gap: 8px; min-height: 0; padding: 16px; text-align: center; color: var(--muted); }
.history-empty h3 { color: var(--text); }
.history-empty p { max-width: 520px; overflow-wrap: anywhere; }
@media (max-width: 940px) { .history-toolbar { gap: 8px; } .history-info-trigger>svg:first-child { display: none; } .history-search { flex-basis: 260px; } }
@media (max-width: 800px) {
  .history-toolbar, .history-input-error, .history-filters, .history-footer { padding-inline: 16px; }
  .history-table th:first-child, .history-table td:first-child { padding-left: 16px; }
  .history-table th:last-child, .history-table td:last-child { padding-right: 16px; }
  .history-heading { gap: 6px; }
  .history-current-time { max-width: 96px; }
  .history-search { flex-basis: 220px; }
  .history-info-panel { right: 16px; width: min(520px,calc(100% - 32px)); }
  .history-evidence-meta { gap: 12px; }
}
@media (max-width: 640px) { .history-toolbar { flex-wrap: wrap; } .history-search { flex: 1 1 220px; } .history-heading { flex-basis: 100%; } .history-evidence-meta { grid-template-columns: minmax(0,1fr); } }
</style>
