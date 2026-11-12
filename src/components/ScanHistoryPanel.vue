<script setup lang="ts">
import { computed, nextTick, ref, shallowRef, watch } from 'vue'
import { invoke } from '@tauri-apps/api/core'
import { ArrowLeft, ChevronDown, ChevronLeft, ChevronRight, History, LoaderCircle, Search } from '@lucide/vue'
import { validManualIpv4 } from '../custom-ips'
import { favoriteDate, favoriteNumber } from '../favorites'
import type { TcpDisplayMetric } from '../result-preferences'
import { tcpLatencyChartScale } from '../result-row-display'
import type { NetworkPathSnapshot, ScanConfig, ScanHistory, ScanHistoryKind, ScanHistoryMatch, ScanRun } from '../types'
import { candidateExclusionReason, candidateMeasurementStatus, hasPrecisionMeasurement, observedFailurePercent, stabilityMetricLabel, targetHost } from '../ui-state'
import SelectControl from './SelectControl.vue'
import TcpDiagnostics from './TcpDiagnostics.vue'
import TcpLatencyCell from './TcpLatencyCell.vue'
import TcpLatencyHeader from './TcpLatencyHeader.vue'

const props = defineProps<{ desktop: boolean; active: boolean; revision: number }>()
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
const searchField = ref<HTMLInputElement | null>(null)
const backButton = ref<HTMLButtonElement | null>(null)
const table = ref<HTMLElement | null>(null)
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
  runScope.value = 'precision'
  clearRunFilters()
  page.value = 1
  expandedResult.value = null
})
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
const changedSettings = computed(() => {
  const settings = [...settingsByRun.value.values()]
  return new Set(settings[0]?.filter((field, index) => settings.some(fields => fields[index].value !== field.value)).map(field => field.label))
})

const rows = computed(() => {
  const current = run.value
  const source = current
    ? filteredRunRows.value.slice(pageStart.value, pageStart.value + pageSize).map(({ result, status }) => ({ run: current, result, kind: null, status }))
    : matches.value.flatMap(match => match.run.results.map(result => ({ ...match, result, status: candidateMeasurementStatus(result, match.run) })))
  return source.map(({ run, result, kind, status }) => {
    const samples = result.downloadSamplesMbps
    const speed = samples.length > 0 ? favoriteNumber(result.downloadMbps) : '—'
    return {
      key: `${run.id}-${result.ip}`,
      run,
      result,
      kind,
      pop: result.colo || result.observedColos[0] || '—',
      failureRate: observedFailurePercent(result.tcpStability),
      initial: result.tcpMs > 0 ? favoriteNumber(result.tcpMs) : '—',
      speed,
      downloadPartial: speed !== '—' && samples.length < run.config.downloadRounds,
      reason: candidateExclusionReason(result) || result.tcpStability?.earlyRejection,
      status,
      settings: settingsByRun.value.get(run.id),
    }
  })
})

const statusText = { running: '测速中', completed: '已完成', cancelled: '已停止', failed: '失败' } as const
const statusClass = { running: 'active', completed: 'good', cancelled: 'quiet', failed: 'error' } as const
const kindText = { scan: '地区优选', favorite: '收藏复测' } as const
const interfaceText = { wired: '有线', wifi: 'Wi‑Fi', tunnel: '隧道', other: '' } as const

function interfaceLabel(path?: NetworkPathSnapshot | null) {
  if (!path) return '未记录'
  const kind = path.interfaceKind ? interfaceText[path.interfaceKind] : path.tunnelInterface ? '隧道' : ''
  return kind ? `${kind} · ${path.interfaceName}` : path.interfaceName
}

function interfaceDetail(path?: NetworkPathSnapshot | null) {
  return path ? `本轮开始时的路由采样 ${path.sampleIp} · ${favoriteDate(path.capturedAt)}` : '本轮未取得网络接口'
}
</script>

<template>
  <section class="history-panel" aria-labelledby="scan-history-title">
    <header class="history-toolbar">
      <div class="history-heading">
        <h2 id="scan-history-title">测速历史</h2>
        <span v-if="history" class="small muted">最近 {{ history.limit }} 轮</span>
      </div>
      <form class="history-search" role="search" aria-label="按 IP 对比测速历史" @submit.prevent="compareIp()">
        <label class="history-search-field">
          <span class="visually-hidden">输入完整 IPv4 地址</span>
          <Search :size="14" aria-hidden="true" />
          <input id="history-ip-search" ref="searchField" v-model="ipInput" type="search" inputmode="decimal" autocomplete="off" spellcheck="false" placeholder="输入 IP 对比各轮结果" :disabled="!desktop" :aria-invalid="!!inputError" :aria-describedby="inputError ? 'history-ip-error' : undefined" />
        </label>
        <button type="submit" class="button primary" :disabled="!desktop || !ipInput.trim()">对比 IP</button>
      </form>
      <p v-if="inputError" id="history-ip-error" class="history-input-error" role="alert">{{ inputError }}</p>
    </header>

    <div v-if="view.mode !== 'recent'" class="history-context">
      <button ref="backButton" type="button" class="button history-back" @click="goBack"><ArrowLeft :size="13" aria-hidden="true" />{{ view.mode === 'run' && comparedIp ? '返回对比' : '返回历史' }}</button>
      <template v-if="view.mode === 'compare'">
        <strong class="mono">{{ view.ip }}</strong>
        <span v-if="!loading && !loadError" class="small muted">匹配 {{ matches.length }} 轮</span>
        <span v-if="changedSettings.size" class="small history-changes">各轮差异 · {{ [...changedSettings].join('、') }}</span>
      </template>
      <template v-else-if="run">
        <strong>{{ kindText[view.kind] }}</strong>
        <time :datetime="run.startedAt">{{ favoriteDate(run.startedAt) }}</time>
        <span :class="['status-label', statusClass[run.status]]">{{ statusText[run.status] }}</span>
        <span class="history-network" :title="interfaceDetail(run.progress.networkPath)">{{ interfaceLabel(run.progress.networkPath) }}</span>
      </template>
    </div>
    <div v-if="run" class="history-run-meta small muted">
      <span :title="run.config.targetUrl">{{ targetHost(run.config.targetUrl) }}</span>
      <span>连接超时 {{ run.config.connectTimeoutMs }} ms</span>
      <span v-if="run.config.maxTcpMs != null">TCP {{ stabilityMetricLabel(run.config.stabilityLatencyMetric) }} ≤ {{ run.config.maxTcpMs }} ms</span>
      <span v-if="run.config.stabilityAttempts != null">稳定性探测 {{ run.config.stabilityAttempts }} 次</span>
    </div>
    <p v-if="run?.error" class="history-run-error small" role="alert">{{ run.error }}</p>
    <div v-if="run?.results.length" class="history-filters" role="group" aria-label="筛选本轮测速记录">
      <SelectControl compact>
        <select v-model="runScope" aria-label="本轮结果范围">
          <option value="precision">精测记录</option>
          <option value="all">全部 IP</option>
        </select>
      </SelectControl>
      <label class="history-search-field history-run-search">
        <span class="visually-hidden">搜索本轮 IP 地址</span>
        <Search :size="14" aria-hidden="true" />
        <input v-model="runIpQuery" type="search" autocomplete="off" spellcheck="false" placeholder="搜索本轮 IP" />
      </label>
      <SelectControl compact>
        <select v-model="runPop" aria-label="本轮机房（POP）">
          <option value="">全部 POP</option>
          <option v-for="pop in runPopOptions" :key="pop" :value="pop">{{ pop }}</option>
        </select>
      </SelectControl>
      <SelectControl compact class="history-status-filter">
        <select v-model="runStatus" aria-label="本轮测速状态">
          <option value="">全部状态</option>
          <option v-for="status in runStatusOptions" :key="status" :value="status">{{ status }}</option>
        </select>
      </SelectControl>
      <button v-if="runHasFilters" type="button" class="text-button" @click="clearRunFilters">清除筛选</button>
    </div>

    <div v-if="loading" class="history-empty" role="status"><LoaderCircle :size="20" class="spinning" aria-hidden="true" /><p>正在读取测速历史…</p></div>
    <div v-else-if="loadError" class="history-empty">
      <div role="alert"><h3>读取测速历史失败</h3><p>{{ loadError }}</p></div>
      <button type="button" class="button" @click="retry++">重试</button>
    </div>
    <div v-else-if="history?.entries.length" class="history-table-wrap" role="region" aria-label="最近测速记录" tabindex="0">
      <table class="history-table run-list">
        <thead><tr>
          <th scope="col">开始时间</th>
          <th scope="col">测速类型</th>
          <th scope="col" title="本轮开始时采集的路由接口">网络接口</th>
          <th scope="col">状态</th>
          <th scope="col">记录 IP 数</th>
          <th scope="col">目标</th>
          <th scope="col"><span class="visually-hidden">操作</span></th>
        </tr></thead>
        <tbody>
          <tr v-for="entry in history.entries" :key="entry.id">
            <td class="history-time"><time :datetime="entry.startedAt">{{ favoriteDate(entry.startedAt) }}</time></td>
            <td>{{ kindText[entry.kind] }}</td>
            <td><span class="history-network" :title="interfaceDetail(entry.networkPath)">{{ interfaceLabel(entry.networkPath) }}</span></td>
            <td><span :class="['status-label', statusClass[entry.status]]">{{ statusText[entry.status] }}</span></td>
            <td class="history-number">{{ entry.resultCount.toLocaleString('zh-CN') }}</td>
            <td class="history-host" :title="entry.targetUrl">{{ targetHost(entry.targetUrl) }}</td>
            <td><button type="button" class="text-button history-row-action" :aria-label="`查看 ${favoriteDate(entry.startedAt)} 的${kindText[entry.kind]}结果`" @click="openRun(entry.id, entry.kind)">查看<ChevronRight :size="12" aria-hidden="true" /></button></td>
          </tr>
        </tbody>
      </table>
    </div>
    <div v-else-if="rows.length" ref="table" class="history-table-wrap" role="region" :aria-label="view.mode === 'compare' ? '同一 IP 的各轮测速结果' : '本轮 IP 测速记录'" tabindex="0">
      <table class="history-table result-list" :class="{ 'comparison-list': view.mode === 'compare' }">
        <thead><tr>
          <th v-if="view.mode === 'compare'" scope="col" title="接口来自本轮开始时的路由采样">开始时间 / 网络接口</th>
          <th v-else scope="col">IP 地址</th>
          <th v-if="view.mode === 'compare'" scope="col">目标 / 关键参数</th>
          <th scope="col">POP</th>
          <th scope="col">初筛 <span class="history-unit">ms</span></th>
          <th scope="col" title="已执行 / 计划探测次数。失败率按已执行的探测计算。">TCP 探测</th>
          <th scope="col"><TcpLatencyHeader v-model="tcpDisplayMetric" run-metric="p95" /></th>
          <th scope="col">下载 <span class="history-unit">Mbps</span></th>
          <th scope="col">结果</th>
        </tr></thead>
        <tbody>
          <template v-for="row in rows" :key="row.key">
          <tr>
            <td v-if="row.kind" class="history-run-cell">
              <button type="button" class="text-button history-row-action history-time" :title="`查看本轮${kindText[row.kind]}结果`" @click="openRun(row.run.id, row.kind)"><time :datetime="row.run.startedAt">{{ favoriteDate(row.run.startedAt) }}</time><ChevronRight :size="12" aria-hidden="true" /></button>
              <span class="history-network">{{ interfaceLabel(row.run.progress.networkPath) }}</span>
              <span v-if="row.run.progress.networkPath" class="history-secondary">
                采样 IP <span class="mono">{{ row.run.progress.networkPath.sampleIp }}</span>
                <span v-if="row.run.progress.networkPath.sampleIp === row.result.ip">（本 IP）</span>
              </span>
            </td>
            <td v-else>
              <button type="button" class="text-button history-row-action mono" :aria-label="`对比 IP ${row.result.ip} 的历史结果`" @click="compareIp(row.result.ip)">{{ row.result.ip }}<Search :size="12" aria-hidden="true" /></button>
            </td>
            <td v-if="row.settings" class="history-comparison-settings">
              <dl>
                <div v-for="setting in row.settings" :key="setting.label" :class="{ 'setting-diff': changedSettings.has(setting.label) }">
                  <dt>{{ setting.label }}</dt><dd>{{ setting.value }}</dd>
                </div>
              </dl>
            </td>
            <td class="mono" :title="row.result.observedColos.join(' / ')">{{ row.pop }}</td>
            <td class="history-number">{{ row.initial }}</td>
            <td class="history-number">
              <template v-if="row.result.tcpStability?.attempted">
                {{ row.result.tcpStability.attempted }} / {{ row.result.tcpStability.requested }}
                <small class="history-secondary history-failure-rate" :class="{ 'history-failures': row.result.tcpStability.failed }">失败 {{ row.result.tcpStability.failed }} 次<span v-if="row.failureRate != null">（{{ favoriteNumber(row.failureRate) }}%）</span></small>
              </template>
              <span v-else class="muted">—</span>
            </td>
            <td class="history-number history-latency">
              <TcpLatencyCell :result="row.result" :display-metric="tcpDisplayMetric" :scale-max-ms="tcpScale" />
            </td>
            <td class="history-number">{{ row.speed }}<small v-if="row.downloadPartial" class="history-secondary">{{ row.result.downloadSamplesMbps.length }} / {{ row.run.config.downloadRounds }} 轮</small></td>
            <td class="history-result">
              <div>{{ row.status }}</div>
              <button type="button" class="text-button history-result-toggle" :aria-expanded="expandedResult === row.key" :aria-controls="expandedResult === row.key ? 'history-tcp-detail' : undefined" @click="expandedResult = expandedResult === row.key ? null : row.key">
                {{ expandedResult === row.key ? '收起明细' : 'TCP 明细' }}<ChevronDown :size="12" aria-hidden="true" />
              </button>
            </td>
          </tr>
          <tr v-if="expandedResult === row.key" class="history-detail-row"><td :colspan="view.mode === 'compare' ? 8 : 7">
            <div id="history-tcp-detail">
              <p v-if="row.reason" class="small muted history-detail-reason">{{ row.reason }}</p>
              <TcpDiagnostics :result="row.result" :config="row.run.config" />
            </div>
          </td></tr>
          </template>
        </tbody>
      </table>
    </div>
    <div v-else class="history-empty">
      <History :size="25" aria-hidden="true" />
      <h3 v-if="!desktop">请在桌面应用查看测速历史</h3>
      <h3 v-else-if="view.mode === 'compare'">历史中未找到这个 IP</h3>
      <template v-else-if="run?.results.length">
        <template v-if="runScope === 'precision' && !runRows.length">
          <h3>本轮没有精测记录</h3>
          <button type="button" class="button" @click="runScope = 'all'">查看全部 IP</button>
        </template>
        <h3 v-else>没有符合筛选条件的 IP</h3>
      </template>
      <h3 v-else-if="view.mode === 'run'">{{ run ? '本轮没有 IP 记录' : '找不到这轮测速' }}</h3>
      <h3 v-else>暂无测速历史</h3>
    </div>

    <footer v-if="run?.results.length" class="history-footer">
      <span class="small muted" aria-live="polite">匹配 {{ filteredRunRows.length.toLocaleString('zh-CN') }} / {{ run.results.length.toLocaleString('zh-CN') }} 个 IP</span>
      <nav v-if="pageCount > 1" class="history-pages" aria-label="历史结果分页">
        <span class="small muted">{{ currentPage }} / {{ pageCount }} 页</span>
        <button type="button" class="icon-button" :disabled="currentPage <= 1" aria-label="上一页" @click="page = currentPage - 1"><ChevronLeft :size="15" aria-hidden="true" /></button>
        <button type="button" class="icon-button" :disabled="currentPage >= pageCount" aria-label="下一页" @click="page = currentPage + 1"><ChevronRight :size="15" aria-hidden="true" /></button>
      </nav>
    </footer>
  </section>
</template>

<style scoped>
.history-panel { display: flex; flex: 1; flex-direction: column; min-width: 0; min-height: 0; overflow: hidden; background: var(--panel); }
.history-toolbar { display: flex; align-items: center; flex-wrap: wrap; justify-content: space-between; gap: 8px 16px; padding: 12px var(--page-padding); border-bottom: 1px solid var(--line); }
.history-heading, .history-search, .history-context, .history-run-meta, .history-pages { display: flex; align-items: center; gap: 8px; }
.history-heading h2 { display: flex; align-items: center; gap: 8px; }
.history-heading h2::before { content: ""; width: 3px; height: 13px; border-radius: 2px; background: var(--brand); }
.history-search { flex: 0 1 320px; min-width: 0; }
.history-search-field { position: relative; flex: 1; min-width: 0; }
.history-search-field svg { position: absolute; top: 50%; left: 9px; color: var(--muted); transform: translateY(-50%); pointer-events: none; }
.history-search-field input[type=search] { padding-left: 30px; font-size: var(--font-control); }
.history-input-error { flex: 0 0 100%; color: var(--error); text-align: right; }
.history-context { flex-wrap: wrap; padding: 9px var(--page-padding); border-bottom: 1px solid var(--line-subtle); }
.history-back { min-height: var(--compact-height); }
.history-run-meta { flex-wrap: wrap; gap: 4px 14px; padding: 6px var(--page-padding); }
.history-run-error { padding: 6px var(--page-padding); color: var(--error); overflow-wrap: anywhere; }
.history-filters { display: flex; flex-wrap: wrap; align-items: center; gap: 6px 8px; padding: 5px var(--page-padding) 9px; }
.history-filters>.select-control { width: auto; }
.history-run-search { flex: 0 1 180px; }
.history-run-search input[type=search] { height: var(--compact-height); min-height: var(--compact-height); }
.history-status-filter { max-width: 205px; }
.history-changes { color: var(--brand); }
.history-table-wrap { flex: 1; min-height: 0; overflow: auto; background: var(--surface-sunken); }
.history-table { width: 100%; min-width: 650px; border-spacing: 0; table-layout: fixed; background: var(--panel); font-size: var(--font-control); }
.history-table th { position: sticky; top: 0; z-index: 1; background: var(--bg); color: var(--muted); font-size: var(--font-caption); font-weight: 600; }
.history-table th, .history-table td { padding: 8px 7px; border-bottom: 1px solid var(--line-subtle); text-align: left; vertical-align: middle; }
.history-table th:first-child, .history-table td:first-child { padding-left: var(--page-padding); }
.history-table th:last-child, .history-table td:last-child { padding-right: var(--page-padding); }
.history-table tbody tr:nth-child(even) { background: var(--surface-sunken); }
.history-table tbody tr:hover { background: color-mix(in srgb, var(--soft) 52%, var(--panel)); }
.run-list th:first-child { width: 22%; }
.run-list th:nth-child(2), .run-list th:nth-child(4), .run-list th:nth-child(5) { width: 10%; }
.run-list th:nth-child(3) { width: 16%; }
.run-list th:last-child { width: 73px; }
.result-list th:first-child { width: 24%; }
.result-list th:nth-child(2) { width: 7%; }
.result-list th:nth-child(3) { width: 8%; }
.result-list th:nth-child(4) { width: 13%; }
.result-list th:nth-child(5) { width: 18%; }
.result-list th:nth-child(6) { width: 11%; }
.comparison-list { min-width: 740px; }
.comparison-list th:first-child { width: 20%; }
.comparison-list th:nth-child(2) { width: 24%; }
.comparison-list th:nth-child(3) { width: 6%; }
.comparison-list th:nth-child(4) { width: 7%; }
.comparison-list th:nth-child(5) { width: 10%; }
.comparison-list th:nth-child(6) { width: 16%; }
.comparison-list th:nth-child(7) { width: 8%; }
.comparison-list th:last-child, .comparison-list td:last-child { padding-right: 8px; }
.history-comparison-settings dl { margin: 0; color: var(--muted); font-size: var(--font-caption); }
.history-comparison-settings dl>div+div { margin-top: 3px; }
.history-comparison-settings dt, .history-comparison-settings dd { display: inline; margin: 0; overflow-wrap: anywhere; }
.history-comparison-settings dt { margin-right: 5px; }
.history-comparison-settings .setting-diff { color: var(--text); }
.setting-diff dt { color: var(--brand); font-weight: 600; }
.history-time, .history-number { font-variant-numeric: tabular-nums; white-space: nowrap; }
.history-unit, .history-secondary { color: var(--muted); font-size: var(--font-caption); }
.history-secondary { display: block; }
.history-latency, .history-failure-rate { white-space: normal; }
.history-failure-rate span { white-space: nowrap; }
.history-failures { color: var(--warn); }
.history-network { color: var(--muted); font-size: var(--font-control); overflow-wrap: anywhere; }
.history-run-cell .history-network { display: block; }
.history-host { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.history-row-action { min-height: 23px; padding: 0; font-size: var(--font-control); white-space: nowrap; }
.history-result-toggle { min-height: 23px; padding: 0; font-size: var(--font-caption); }
.history-result-toggle[aria-expanded="true"] svg { transform: rotate(180deg); }
.history-detail-row { background: var(--surface-sunken); }
.history-detail-reason { margin: 0 0 8px; overflow-wrap: anywhere; }
.history-footer { display: flex; justify-content: space-between; align-items: center; min-height: 38px; padding: 0 var(--page-padding); border-top: 1px solid var(--line); }
.history-empty { display: flex; flex: 1; align-items: center; justify-content: center; flex-direction: column; gap: 8px; min-height: 0; padding: 16px; text-align: center; color: var(--muted); background: var(--surface-sunken); }
.history-empty h3 { color: var(--text); }
.history-empty p { max-width: 520px; overflow-wrap: anywhere; }
@media (max-width: 570px) {
  .history-search { flex-basis: 100%; }
  .history-input-error { text-align: left; }
}
</style>
