import { defaultConfig, selectableRegions, type IpResult, type ScanConfig, type ScanRun, type TcpStability } from './types'

export interface ValidationIssue { field: keyof ScanConfig; message: string }

export function validateConfig(config: ScanConfig): ValidationIssue | null {
  if (!config.regions.length) return { field: 'regions', message: '至少选择一个国家／地区。' }
  if (!['adaptive', 'limited'].includes(config.precisionMode)) return { field: 'precisionMode', message: '请选择精测方式。' }
  if (config.stabilityFirst !== true) return { field: 'stabilityFirst', message: '新测试须先完成稳定性筛选，请恢复默认参数后再开始。' }
  if (config.tcpLatencyScreening !== false) return { field: 'tcpLatencyScreening', message: '新测试不支持旧的单次延迟初筛规则，请恢复默认参数后再开始。' }
  if (!['average', 'p95', 'max'].includes(config.stabilityLatencyMetric)) return { field: 'stabilityLatencyMetric', message: '请选择 TCP 稳定性延迟判定指标。' }
  if (config.source === 'custom' && !config.customIps.trim()) return { field: 'customIps', message: '请填写要测试的 IPv4 或 CIDR，每行一个。' }
  try {
    const url = new URL(config.targetUrl)
    if (url.protocol !== 'https:') return { field: 'targetUrl', message: '测速地址必须使用 HTTPS。' }
    if (url.username || url.password || url.hash) return { field: 'targetUrl', message: '测速地址不能包含用户名、密码或 URL 片段。' }
    if (/^[\d.]+$/.test(url.hostname) || url.hostname.includes(':') || url.port === '0') return { field: 'targetUrl', message: '测速地址须使用域名和有效端口，以正确验证 TLS 证书。' }
  } catch { return { field: 'targetUrl', message: '请填写有效的 HTTPS 测速地址。' } }
  const limits: [keyof ScanConfig, number, number, string][] = [
    ['ipsPerRegion', 1, 10, '每区结果数'], ['maxTcpMs', 1, 30000, '稳定性延迟上限'], ['minDownloadMbps', 0, 10000, '日常优选最低速率'],
    ['stabilityAttempts', 10, 1000, 'TCP 稳定性测试次数'], ['stabilityIntervalMs', 50, 5000, 'TCP 稳定性探测间隔'],
    ['maxStabilityFailurePercent', 0, 100, '允许 TCP 连接失败率'],
    ['maxCandidates', 1, 10000, '抽样候选上限'], ['tcpConcurrency', 1, 256, '连接并发'],
    ['connectTimeoutMs', 100, 30000, '连接超时'], ['requestTimeoutMs', 100, 60000, '请求超时'],
    ['regionalCandidateLimit', 1, 10000, '每区入围精测上限'],
    ['verificationAttempts', 2, 10, '验证次数'],
    ['downloadRounds', 2, 5, '下载轮数'], ['downloadSeconds', 1, 15, '每轮下载时长'],
  ]
  if (config.downloadBudgetEnabled !== false) limits.push(['downloadBudgetMiB', 1, 4096, '下载数据预算'])
  if (config.precisionMode === 'limited') limits.push(['candidatesPerRegion', 1, 10000, '每区精测候选上限'])
  for (const [field, min, max, label] of limits) {
    const value = config[field]
    if (typeof value !== 'number' || !Number.isFinite(value)) return { field, message: `请填写有效的${label}。` }
    if (value < min || value > max) return { field, message: `${label}应在 ${min}–${max} 之间。` }
    if (field !== 'minDownloadMbps' && field !== 'maxTcpMs' && field !== 'maxStabilityFailurePercent' && !Number.isInteger(value)) return { field, message: `${label}必须为整数。` }
  }
  if (config.precisionMode === 'limited' && config.candidatesPerRegion < config.ipsPerRegion) return { field: 'candidatesPerRegion', message: '每区精测候选上限不能小于每区保留结果数。' }
  if ((config.regionalCandidateLimit ?? 0) < config.ipsPerRegion) return { field: 'regionalCandidateLimit', message: '每区入围精测上限不能小于每区保留结果数。' }
  return null
}

export interface ExclusionCategory {
  key: string
  label: string
  kind: 'failure' | 'filter' | 'incomplete'
  field: keyof ScanConfig
  count: number
}

function isRegionalAdmissionExclusion(reason: string): boolean {
  return /按初筛\s*TCP\s*延迟排序|未进入本地区前\s*\d+\s*名精测候选/.test(reason)
}

function isDownloadShortlistExclusion(reason: string): boolean {
  return /未进入本地区前\s*\d+\s*名下载候选/.test(reason)
}

function exclusionCategory(reason: string): Omit<ExclusionCategory, 'count'> {
  if (isDownloadShortlistExclusion(reason)) return { key: 'download-shortlist', label: '稳定性已完成，未进入本区前 10 名下载候选', kind: 'filter', field: 'stabilityLatencyMetric' }
  if (/累计\s*2\s*次连接失败|累计\s*\d+\s*次连接失败，达到\s*2\s*次|连接失败累计达到\s*2\s*次/.test(reason)) return { key: 'tcp-stability-two-failures', label: 'TCP 稳定性累计 2 次连接失败', kind: 'filter', field: 'connectTimeoutMs' }
  if (/TCP 稳定性提前淘汰/.test(reason)) return /连接失败|失败率/.test(reason)
    ? { key: 'tcp-stability-failure', label: 'TCP 稳定性连接失败率超过允许值', kind: 'filter', field: 'maxStabilityFailurePercent' }
    : { key: 'tcp-limit', label: 'TCP 延迟超过本次上限', kind: 'filter', field: 'maxTcpMs' }
  if (/稳定性.*(?:连接失败率|失败率).*(?:超过|超出|高于)/.test(reason)) return { key: 'tcp-stability-failure', label: 'TCP 稳定性连接失败率超过允许值', kind: 'filter', field: 'maxStabilityFailurePercent' }
  if (/稳定性.*(?:未完成|已停止|统计不完整)|尚未完成.*TCP 稳定性/.test(reason)) return { key: 'tcp-stability-incomplete', label: 'TCP 稳定性测试未完成', kind: 'incomplete', field: 'stabilityAttempts' }
  if (/TCP 稳定性探测全部连接失败/.test(reason)) return { key: 'tcp-stability-all-failed', label: 'TCP 稳定性探测全部连接失败', kind: 'failure', field: 'connectTimeoutMs' }
  if (/TCP.*(?:超过|超出|高于).*(?:上限|限制)|TCP.*(?:上限|限制).*(?:超过|超出|高于)/.test(reason)) return { key: 'tcp-limit', label: 'TCP 延迟超过本次上限', kind: 'filter', field: 'maxTcpMs' }
  if (/本地区已有足够达标结果/.test(reason)) return { key: 'region-satisfied', label: '已有足够达标结果，未继续精测', kind: 'filter', field: 'ipsPerRegion' }
  if (/实际响应地区不在/.test(reason)) return { key: 'region-filter', label: '实际响应地区不在所选范围', kind: 'filter', field: 'regions' }
  if (isRegionalAdmissionExclusion(reason)) return { key: 'regional-limit', label: '按初筛 TCP 延迟排序后未入围，未精测', kind: 'filter', field: 'regionalCandidateLimit' }
  if (/未进入本地区精测候选|达到本地区手动精测上限/.test(reason)) return { key: 'quota-filter', label: '达到本地区手动精测上限，未继续精测', kind: 'filter', field: 'precisionMode' }
  if (/下载预算(?:不足|已用尽)/.test(reason)) return { key: 'budget', label: '下载预算用尽，未完成全部精测', kind: 'incomplete', field: 'downloadBudgetMiB' }
  if (/测试已停止|未完成必要|复测未完成/.test(reason)) return { key: 'incomplete', label: '尚未完成必要复测', kind: 'incomplete', field: 'downloadBudgetMiB' }
  if (/接入地区发生变化|POP 变化|连接对端发生变化|无法确认实际连接/.test(reason)) return { key: 'drift', label: '接入地区或连接对端变化', kind: 'failure', field: 'verificationAttempts' }
  if (/低于日常优选目标/.test(reason)) return { key: 'speed', label: '验证通过，有效下载速率低于日常目标', kind: 'filter', field: 'minDownloadMbps' }
  if (/无地区映射|未知地区|有效的 Cloudflare POP/.test(reason)) return { key: 'unknown-pop', label: '未能确认 POP 地区', kind: 'failure', field: 'targetUrl' }
  if (/下载响应进行了压缩|下载目标未返回|下载响应不足|下载样本不足/.test(reason)) return { key: 'download', label: '下载目标或样本不符合测速要求', kind: 'failure', field: 'targetUrl' }
  if (/^TCP/.test(reason)) return { key: 'connection', label: 'TCP 连接失败或超时', kind: 'failure', field: 'connectTimeoutMs' }
  if (/HTTPS 请求超时/.test(reason)) return { key: 'https-timeout', label: 'HTTPS 请求超时', kind: 'failure', field: 'requestTimeoutMs' }
  if (/HTTPS|TLS|HTTP|trace|测速地址/.test(reason)) return { key: 'https', label: 'HTTPS 验证失败或目标响应不兼容', kind: 'failure', field: 'targetUrl' }
  if (!reason) return { key: 'incomplete', label: '尚未完成必要复测', kind: 'incomplete', field: 'downloadBudgetMiB' }
  return { key: 'other', label: '其他未入选原因', kind: 'failure', field: 'targetUrl' }
}

export function summarizeExclusions(results: IpResult[], _status: ScanRun['status']): ExclusionCategory[] {
  const groups = new Map<string, ExclusionCategory>()
  for (const result of results) {
    if ((result.eligible || result.measurementPassed) && !result.exclusionReason) continue
    const category = exclusionCategory(result.exclusionReason ?? '')
    const existing = groups.get(category.key)
    if (existing) existing.count += 1
    else groups.set(category.key, { ...category, count: 1 })
  }
  return [...groups.values()].sort((a, b) => b.count - a.count)
}

export function targetHost(url: string): string {
  try { return new URL(url).host } catch { return url || '未设置目标' }
}

export function normalizeActiveConfig(config: ScanConfig): ScanConfig {
  const defaults = defaultConfig()
  const allowedRegions = new Set<string>(selectableRegions.map(region => region.code))
  const selectedRegions = [...new Set(config.regions.filter(code => allowedRegions.has(code)))]
  // Recommendation settings are legacy metadata; result-table filters now
  // control selection. Keep new requests valid without imposing hidden limits.
  const ipsPerRegion = 1
  const precisionMode = config.precisionMode ?? defaults.precisionMode
  const validManualLimit = Number.isInteger(config.candidatesPerRegion) && config.candidatesPerRegion >= 1 && config.candidatesPerRegion <= 10000
  const validPoolSize = typeof config.candidatePoolSize === 'number' && Number.isInteger(config.candidatePoolSize)
    && config.candidatePoolSize >= ipsPerRegion && config.candidatePoolSize <= 1000
  const serializablePoolSize = typeof config.candidatePoolSize === 'number' && Number.isInteger(config.candidatePoolSize) && config.candidatePoolSize >= 1 && config.candidatePoolSize <= 1000
  return {
    ...config,
    regions: selectedRegions.length ? selectedRegions : [...defaults.regions],
    ipsPerRegion,
    preference: 'daily',
    minDownloadMbps: 0,
    precisionMode,
    regionalCandidateLimit: config.regionalCandidateLimit ?? defaults.regionalCandidateLimit,
    downloadBudgetEnabled: config.downloadBudgetEnabled ?? defaults.downloadBudgetEnabled,
    candidatesPerRegion: precisionMode === 'adaptive' && !validManualLimit ? defaults.candidatesPerRegion : config.candidatesPerRegion,
    candidatePoolSize: precisionMode === 'adaptive' && !validPoolSize ? Math.max(defaults.candidatePoolSize ?? 20, ipsPerRegion)
      : serializablePoolSize ? config.candidatePoolSize : null,
    maxTcpMs: config.maxTcpMs ?? defaults.maxTcpMs,
    tcpLatencyScreening: false,
    stabilityFirst: true,
    stabilityAttempts: config.stabilityAttempts ?? defaults.stabilityAttempts,
    stabilityIntervalMs: config.stabilityIntervalMs ?? defaults.stabilityIntervalMs,
    maxStabilityFailurePercent: config.maxStabilityFailurePercent ?? defaults.maxStabilityFailurePercent,
    stabilityLatencyMetric: config.stabilityLatencyMetric ?? defaults.stabilityLatencyMetric,
  }
}

/** Historical null keeps the original admission policy truthful. */
export function regionalPrecisionLimit(config: ScanConfig): number | null {
  const value = config.regionalCandidateLimit
  const admission = typeof value === 'number' && Number.isInteger(value) && value > 0 ? value : null
  return config.precisionMode === 'limited'
    ? admission == null ? config.candidatesPerRegion : Math.min(admission, config.candidatesPerRegion)
    : admission
}

export function precisionPolicyLabel(config: ScanConfig): string {
  const limit = regionalPrecisionLimit(config)
  if (config.stabilityFirst) return `先测稳定性 · 每区前 10 名下载${limit == null ? '' : ` · 入围上限 ${limit}`}`
  if (typeof config.regionalCandidateLimit === 'number' && limit != null) return `按 TCP 延迟取前 ${limit} 个／地区`
  return config.precisionMode === 'adaptive'
    ? `自动补测 · 首批候选池 ${config.candidatePoolSize ?? 20} 个／地区`
    : `每区最多精测 ${config.candidatesPerRegion} 个`
}

export function precisionPolicyDescription(config: ScanConfig): string {
  const limit = regionalPrecisionLimit(config)
  const budgetStop = config.downloadBudgetEnabled !== false ? '下载预算用尽、' : ''
  if (config.stabilityFirst) {
    const admission = limit == null ? '各区入围候选' : `各区按初筛 TCP 延迟取前 ${limit} 个入围，不足时全部入围`
    return `先完成连通与 POP 识别，${admission}，全部执行 TCP 稳定性测试；累计 2 次连接失败立即淘汰，其余测满后判定。全部稳定性测试结束后，各区从达标候选中依次按失败率、所选延迟指标、HTTPS 耗时、波动与 IP 排序，选前 10 个进行服务校验和逐个下载测速；不足 10 个则全部选择。入选名单全部测速，不从名单外补位；${budgetStop}名单测完或手动停止时结束。`
  }
  if (typeof config.regionalCandidateLimit === 'number' && limit != null) {
    return `先完成连通与 POP 识别，各区按初筛 TCP 延迟从低到高排序，仅前 ${limit} 个入围精测；不足时全部入围。每批最多精测 5 个，在入围范围内补位，达到 ${config.ipsPerRegion} 个达标结果、入围候选测完、${budgetStop}手动停止时结束。不从前 ${limit} 名之外继续补测，未入围不表示节点不可用。`
  }
  const stop = `各区找到 ${config.ipsPerRegion} 个达标结果、${budgetStop}候选测完或手动停止时结束。`
  return config.precisionMode === 'adaptive'
    ? `先完成连通与 POP 识别，每区首批收集 ${config.candidatePoolSize ?? 20} 个候选，不足则按已发现数量开始；每次安排最多 5 个精测，未达标时继续从后备补充，不设每区精测数量上限。${stop}候选池数量不代表已全部完成精测。`
    : `连通与 POP 识别后，每区最多精测 ${config.candidatesPerRegion} 个；达到手动上限，或${stop}`
}

export function regionCandidatePool(run: ScanRun, region: string): { label: string; detail: string } | null {
  const limit = regionalPrecisionLimit(run.config)
  if (run.config.stabilityFirst && limit != null) {
    const identified = Math.max(0, run.progress.discoveredByRegion[region] ?? 0)
    const admitted = Math.min(limit, identified)
    const collecting = run.status === 'running' && ['connecting', 'discovering'].includes(run.progress.stage)
    const selecting = !collecting && !['stabilizing'].includes(run.progress.stage)
    const downloadCandidates = selecting ? run.results.filter(result => result.regionCode === region && !!result.tcpStability
      && !isDownloadShortlistExclusion(result.exclusionReason ?? '') && stabilityVerdict(result, run.config) === 'passed').length : null
    const label = `${collecting ? '预计入围稳定性' : '入围稳定性'} ${admitted} / ${limit} · 未入围 ${Math.max(0, identified - admitted)}${downloadCandidates == null ? '' : ` · 下载候选 ${Math.min(10, downloadCandidates)}`}`
    const detail = collecting ? '识别结束后按初筛 TCP 延迟确定稳定性入围名单'
      : !identified ? '未识别到本区 POP，暂无可测候选'
      : ['cancelled', 'failed'].includes(run.status) ? '测试已停止；入围不代表已完成稳定性或下载测速'
      : selecting ? '各区稳定性达标前 10 名进入服务校验与下载，不补位'
      : '先完成全部入围候选的稳定性测试，再选本区前 10 名下载'
    return { label, detail }
  }
  if (typeof run.config.regionalCandidateLimit === 'number' && limit != null) {
    const identified = Math.max(0, run.progress.discoveredByRegion[region] ?? 0)
    const admitted = Math.min(limit, identified)
    const collecting = run.status === 'running' && ['connecting', 'discovering'].includes(run.progress.stage)
    const label = `${collecting ? '预计入围' : '入围'} ${admitted} / ${limit} · 未入围 ${Math.max(0, identified - admitted)}`
    const detail = collecting ? '识别结束后按 TCP 延迟从低到高确定入围名单'
      : !identified ? '未识别到本区 POP，暂无可精测候选'
      : ['cancelled', 'failed'].includes(run.status) ? '测试已停止；入围不代表已完成精测'
      : identified < limit ? `不足 ${limit} 个，按实际数量入围精测`
      : `仅精测初筛 TCP 延迟最低的 ${limit} 个，不再补充未入围候选`
    return { label, detail }
  }
  const target = run.config.candidatePoolSize
  if (run.config.precisionMode !== 'adaptive' || typeof target !== 'number' || !Number.isInteger(target) || target < 1) return null
  const identified = Math.max(0, run.progress.discoveredByRegion[region] ?? 0)
  const initial = Math.min(target, identified)
  const reserve = Math.max(identified - target, 0)
  const collecting = run.status === 'running' && ['connecting', 'discovering'].includes(run.progress.stage)
  const interrupted = ['cancelled', 'failed'].includes(run.status)
  const detail = collecting ? '正在识别地区，候选仍在收集'
    : interrupted ? '测试已停止；候选池数量不代表已完成精测'
    : !identified ? '尚未发现候选，无法开始本区精测'
    : identified < target ? `不足 ${target} 个，按已发现数量安排精测`
    : run.status === 'completed' ? '本轮精测已结束；分组数量来自地区识别阶段，完成数见测速覆盖'
    : '首批候选按批精测，未达标时继续从后备补充'
  return { label: `首批候选池 ${initial} / ${target} · 后备总数 ${reserve}`, detail }
}

export function candidateExclusionReason(result: IpResult): string {
  const reason = result.exclusionReason ?? ''
  if (isRegionalAdmissionExclusion(reason)) return reason.replace('初筛TCP', '初筛 TCP ')
  if (/本地区已有足够达标结果/.test(reason)) return '已有足够达标结果，未继续精测'
  if (/未进入本地区精测候选|达到本地区手动精测上限/.test(reason)) return '达到本地区手动精测上限，未继续精测'
  if (/下载预算(?:不足|已用尽)/.test(reason)) {
    return result.attempts <= 1 && !result.tcpStability && !result.downloadSamplesMbps.length
      ? '下载预算已用尽，未继续精测'
      : '下载预算不足，未完成必要下载复测'
  }
  return reason
}

export function completeStability(result: IpResult, config: ScanConfig): boolean {
  if (result.tcpStability?.earlyRejection) return false
  if (config.stabilityAttempts == null) return true
  const stability = result.tcpStability
  return !!stability?.completed && !stability.earlyRejection && stability.requested === config.stabilityAttempts && stability.attempted === stability.requested
}

export function completedTcpMetricValue(result: IpResult, metric: ScanConfig['stabilityLatencyMetric']): number | null {
  const stability = result.tcpStability
  if (!stability?.completed || stability.earlyRejection || stability.attempted !== stability.requested || !Array.isArray(stability.probesMs) || stability.probesMs.length !== stability.requested) return null
  const successful = stability.probesMs.filter((value): value is number => typeof value === 'number' && Number.isFinite(value) && value >= 0).sort((a, b) => a - b)
  if (!successful.length) return null
  const value = metric === 'average'
    ? successful.reduce((sum, sample) => sum + sample, 0) / successful.length
    : metric === 'max'
      ? successful[successful.length - 1]
      : successful[Math.ceil(0.95 * successful.length) - 1]
  return Number.isFinite(value) ? value : null
}

export function measuredTcpMs(result: IpResult, metric: ScanConfig['stabilityLatencyMetric'] = 'p95'): number {
  return completedTcpMetricValue(result, metric) ?? result.tcpMs
}

export function stabilityMetricLabel(metric: ScanConfig['stabilityLatencyMetric']): string {
  return metric === 'average' ? '平均' : metric === 'max' ? '最大' : 'P95'
}

export function measuredTcpFailure(result: IpResult): number {
  const stability = result.tcpStability
  return stability?.completed && stability.attempted === stability.requested && stability.attempted > 0 ? stability.failed / stability.attempted * 100 : Infinity
}

export function observedFailurePercent(stability?: TcpStability): number | null {
  return stability && stability.attempted > 0 ? stability.failed / stability.attempted * 100 : null
}

export function completeMeasurement(result: IpResult, config: ScanConfig): boolean {
  return result.measurementPassed && completeStability(result, config)
    && (config.stabilityAttempts == null || stabilityVerdict(result, config) === 'passed')
    && result.attempts >= config.verificationAttempts && result.successCount === result.attempts
    && result.downloadSamplesMbps.length >= config.downloadRounds
    && result.downloadSamplesMbps.every(sample => Number.isFinite(sample) && sample > 0)
}

/** POP discovery and a queued stability object alone are not precision measurements. */
export function hasPrecisionMeasurement(result: IpResult): boolean {
  return (result.tcpStability?.attempted ?? 0) > 0
    || result.attempts > 1
    || result.downloadSamplesMbps.length > 0
}

export function summarizeMeasurementCoverage(results: IpResult[], config: ScanConfig) {
  return {
    sampled: results.length,
    tcpConnected: results.filter(result => result.tcpMs > 0 || result.attempts > 0 || !!result.colo || !!result.tcpStability).length,
    identified: results.filter(result => !!result.colo).length,
    precisionStarted: results.filter(hasPrecisionMeasurement).length,
    stabilityStarted: results.filter(result => (result.tcpStability?.attempted ?? 0) > 0).length,
    stabilityCompleted: results.filter(result => {
      const stats = result.tcpStability
      if (!stats?.completed || stats.earlyRejection || !Number.isInteger(stats.requested) || stats.requested <= 0
        || stats.attempted !== stats.requested || stats.succeeded + stats.failed !== stats.attempted
        || !Array.isArray(stats.probesMs) || stats.probesMs.length !== stats.requested
        || (config.stabilityAttempts != null && stats.requested !== config.stabilityAttempts)) return false
      const successful = stats.probesMs.filter(probe => probe !== null)
      return successful.length === stats.succeeded
        && successful.every(probe => typeof probe === 'number' && Number.isFinite(probe) && probe >= 0)
    }).length,
    stabilityRejectedEarly: results.filter(result => !!result.tcpStability?.earlyRejection).length,
    measurementCompleted: results.filter(result => completeMeasurement(result, config)).length,
  }
}

/** Report evidence actually obtained, independently of why a candidate was excluded. */
export function candidateMeasurementStatus(result: IpResult, run: ScanRun): string {
  if (isDownloadShortlistExclusion(result.exclusionReason ?? '')) return '稳定性已完成 · 未入选下载'
  if (isRegionalAdmissionExclusion(result.exclusionReason ?? '')) return '未入围 · 未精测'
  if (completeMeasurement(result, run.config)) return '完整测速完成'
  const stability = result.tcpStability
  if (stability) {
    if (stability.earlyRejection) return '稳定性提前淘汰'
    const verdict = stabilityVerdict(result, run.config)
    if (verdict === 'failed') return '稳定性未达标'
    if (verdict === 'incomplete') {
      const active = run.status === 'running' && run.progress.stage === 'stabilizing'
        && !!run.progress.stabilityBatch?.candidateIps.includes(result.ip)
      return active ? (stability.attempted > 0 ? '稳定性测试中' : '准备稳定性测试') : '稳定性未完成'
    }
    if (run.config.stabilityFirst) {
      if (result.successCount < result.attempts) return '服务校验未通过'
      if (result.attempts < run.config.verificationAttempts) return run.status === 'running' && !result.exclusionReason ? '待服务校验' : '服务校验未完成'
    }
    if (result.downloadSamplesMbps.length < run.config.downloadRounds) {
      return run.status === 'running' && !result.exclusionReason && !result.downloadSamplesMbps.length ? '待下载复测' : '下载未完成'
    }
    return '完整测速未通过'
  }
  if (result.attempts > 1) {
    if (result.successCount < result.attempts) return '服务校验未通过'
    return result.attempts < run.config.verificationAttempts ? '服务校验未完成' : '待稳定性测试'
  }
  if (!result.tcpMs && !result.attempts && /^TCP/.test(result.exclusionReason ?? '')) return 'TCP 未连通'
  return '未精测'
}

export interface StabilityBatchCandidate {
  ip: string
  result?: IpResult
  stability?: TcpStability
  state: 'waiting' | 'active' | 'completed' | 'rejected'
  attempted: number
  requested: number
  percent: number
}

export interface StabilityBatchSummary {
  number: number
  requestedPerIp: number
  candidates: StabilityBatchCandidate[]
  active: StabilityBatchCandidate[]
  completed: StabilityBatchCandidate[]
  rejected: StabilityBatchCandidate[]
  settled: StabilityBatchCandidate[]
  waiting: StabilityBatchCandidate[]
  completedProbes: number
  totalProbes: number
  percent: number
}

/** The explicit batch is authoritative; prior batches and other run results stay out. */
export function summarizeStabilityBatch(run: ScanRun): StabilityBatchSummary | null {
  if (run.status !== 'running') return null
  const batch = run.progress.stage === 'stabilizing' ? run.progress.stabilityBatch : undefined
  const requested = run.config.stabilityAttempts
  if (!batch || !batch.candidateIps.length || typeof requested !== 'number' || !Number.isInteger(requested) || requested <= 0) return null
  const results = new Map(run.results.map(result => [result.ip, result]))
  const candidates = [...new Set(batch.candidateIps)].map((ip): StabilityBatchCandidate => {
    const result = results.get(ip)
    const snapshot = result?.tcpStability
    const stability = snapshot?.requested === requested ? snapshot : undefined
    const attempted = stability ? Math.max(0, Math.min(requested, stability.attempted)) : 0
    const completed = !!stability?.completed && stability.attempted === requested
    return { ip, result, stability, state: stability?.earlyRejection ? 'rejected' : completed ? 'completed' : stability ? 'active' : 'waiting', attempted, requested, percent: Math.round(attempted / requested * 100) }
  })
  const completedProbes = candidates.reduce((sum, candidate) => sum + candidate.attempted, 0)
  const totalProbes = candidates.length * requested
  const settled = candidates.filter(candidate => candidate.state === 'completed' || candidate.state === 'rejected')
  return {
    number: batch.number, requestedPerIp: requested, candidates,
    active: candidates.filter(candidate => candidate.state === 'active'),
    completed: candidates.filter(candidate => candidate.state === 'completed'),
    rejected: candidates.filter(candidate => candidate.state === 'rejected'),
    settled,
    waiting: candidates.filter(candidate => candidate.state === 'waiting'),
    completedProbes, totalProbes, percent: Math.round(settled.length / candidates.length * 100),
  }
}

export function stabilityVerdict(result: IpResult, config: ScanConfig): 'passed' | 'failed' | 'incomplete' {
  const stats = result.tcpStability
  if (stats?.earlyRejection || config.stabilityFirst && (stats?.failed ?? 0) >= 2) return 'failed'
  if (!stats || !completeStability(result, config) || !Array.isArray(stats.probesMs) || stats.probesMs.length !== stats.requested || stats.succeeded + stats.failed !== stats.attempted) return 'incomplete'
  const success = stats.probesMs.filter(probe => probe !== null)
  if (success.length !== stats.succeeded || success.some(probe => typeof probe !== 'number' || !Number.isFinite(probe) || probe < 0)) return 'incomplete'
  const latency = completedTcpMetricValue(result, config.stabilityLatencyMetric ?? 'max')
  const failures = observedFailurePercent(stats)
  if (stats.succeeded === 0) return 'failed'
  if (latency == null || failures == null || config.maxTcpMs == null) return 'incomplete'
  return latency <= config.maxTcpMs && failures <= config.maxStabilityFailurePercent ? 'passed' : 'failed'
}

/** A settled candidate has a result, not merely a finished stability sub-stage. */
export function precisionCandidateSettled(result: IpResult, config: ScanConfig): boolean {
  if (completeMeasurement(result, config) || stabilityVerdict(result, config) === 'failed') return true
  if (config.stabilityFirst && isDownloadShortlistExclusion(result.exclusionReason ?? '') && completeStability(result, config)) return true
  const started = result.attempts > 1 || !!result.tcpStability || result.downloadSamplesMbps.length > 0
  if (!started || !result.exclusionReason) return false
  // Stopping, budget limits and queue limits are not measured rejection evidence.
  return !/未完成|测试已停止|未进入|手动精测上限|足够达标结果|下载预算/.test(result.exclusionReason)
}

export function summarizePrecisionQueue(run: ScanRun) {
  const config = run.config
  const selected = new Set(config.regions)
  const candidates = run.results.filter(result => !!result.colo && selected.has(result.regionCode))
  const inBatch = run.status === 'running'
    ? new Set(run.progress.precisionBatch?.candidateIps ?? run.progress.stabilityBatch?.candidateIps ?? [])
    : new Set<string>()
  // Precision admission happened before any later POP drift. Its work remains
  // accounted for even when the latest observed region is no longer selected.
  const precisionJobs = run.results.filter(result => !isRegionalAdmissionExclusion(result.exclusionReason ?? '') && (inBatch.has(result.ip)
    || !!result.tcpStability || result.downloadSamplesMbps.length > 0
    || result.attempts > 1 && (selected.has(result.regionCode) || /接入地区发生变化|POP 变化/.test(result.exclusionReason ?? ''))))
  const identifiedAvailable = config.regions.reduce((total, code) => {
    const recorded = candidates.filter(result => result.regionCode === code).length
    const count = Math.max(recorded, Math.max(0, run.progress.discoveredByRegion[code] ?? 0))
    const limit = regionalPrecisionLimit(config)
    return total + (limit == null ? count : Math.min(limit, count))
  }, 0)
  const available = Math.max(identifiedAvailable, precisionJobs.length)
  const measured = precisionJobs.filter(result => completeMeasurement(result, config))
  const qualified = measured.filter(result => selected.has(result.regionCode)
    && (config.preference === 'download' || result.downloadMbps >= config.minDownloadMbps))
  const processed = precisionJobs.filter(result => precisionCandidateSettled(result, config)).length
  const processing = precisionJobs.filter(result => inBatch.has(result.ip) && !precisionCandidateSettled(result, config)).length
  const remaining = run.status === 'running' ? Math.max(0, available - processed) : 0
  return {
    available, processed, remaining,
    processing: Math.min(processing, remaining),
    queued: Math.max(0, remaining - processing),
    measurementPassed: measured.length,
    qualified: qualified.length,
    qualifiedRegions: config.regions.filter(code => qualified.filter(result => result.regionCode === code).length >= config.ipsPerRegion).length,
    earlyRejected: precisionJobs.filter(result => !!result.tcpStability?.earlyRejection).length,
  }
}

export function stageProgressText(run: ScanRun): { title: string; caption: string; explanation: string; percent: number | null } {
  if (run.status === 'cancelled') return { title: '测试已停止', caption: '已保留完成的观测', explanation: '未完成的探测不会计为连接失败。', percent: null }
  if (run.status === 'failed') return { title: '本次测试未完成', caption: '已保留取得的观测', explanation: '请查看结果中的具体原因。', percent: null }
  if (run.status === 'completed') return { title: '测速已结束', caption: '本轮结果已整理完成', explanation: '请查看各地区结果与具体判定。', percent: null }
  const progress = run.progress
  const completed = Math.max(0, progress.completed)
  const total = Math.max(0, progress.total)
  const percent = total > 0 ? Math.min(100, Math.round(completed / total * 100)) : null
  const count = `${completed.toLocaleString('zh-CN')} / ${total.toLocaleString('zh-CN')}`
  switch (progress.stage) {
    case 'connecting': return { title: '正在检查 TCP 连通性', caption: total ? `已检查 ${count} 个 IP（${percent}%）` : '正在准备待检查的 IP', explanation: '连接成功后先识别实际接入地区；此进度只表示当前连通性检查。', percent }
    case 'discovering': return { title: '正在识别实际响应地区', caption: total ? `已检查 ${count} 个 IP 的 POP 响应（${percent}%）` : '正在准备地区识别', explanation: '按当前网络实际响应的 POP 分组；识别不到的归属保留为未知。', percent }
    case 'verifying': return { title: '正在验证候选可用性', caption: total ? `已复核 ${count} 个候选（${percent}%）` : '正在准备候选可用性验证', explanation: run.config.stabilityFirst ? `全部稳定性测试已结束；仅对各区前 10 名下载候选完成 ${run.config.verificationAttempts} 次服务可用性校验。` : `每个候选完成 ${run.config.verificationAttempts} 次服务可用性校验，再进入 TCP 稳定性复测。`, percent }
    case 'downloading': return { title: '正在进行下载复测', caption: total ? `已处理 ${count} 个计划下载轮次（${percent}%）` : '正在准备下载复测', explanation: '进度包含失败或跳过的计划轮次；成功取得的有效下载样本用于最终推荐。', percent }
    case 'completed': return { title: '正在汇总本轮结果', caption: '测速阶段已结束，正在整理结果', explanation: '将根据本次设置分别生成各地区推荐。', percent: null }
    case 'cancelled': return { title: '测试已停止', caption: '已保留完成的观测', explanation: '未完成的探测不会计为连接失败。', percent: null }
    case 'failed': return { title: '本次测试未完成', caption: '已保留取得的观测', explanation: '请查看结果中的具体原因。', percent: null }
    default: return { title: '正在检查 TCP 稳定性', caption: '正在准备本批候选', explanation: run.config.stabilityFirst ? '先完成所有入围候选的稳定性测试；累计 2 次连接失败立即淘汰，其余测满后再按地区选前 10 名下载。' : '已确定无法达标的 IP 会提前淘汰；仍有机会达标的继续探测。', percent: null }
  }
}
