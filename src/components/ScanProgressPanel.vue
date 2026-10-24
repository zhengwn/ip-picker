<script setup lang="ts">
import { computed } from 'vue'
import { ArrowRight, Check, ChevronDown, LoaderCircle } from '@lucide/vue'
import { regionName, type ScanRun } from '../types'
import { completedTcpMetricValue, observedFailurePercent, precisionPolicyLabel, regionalPrecisionLimit, regionCandidatePool, stabilityMetricLabel, stabilityVerdict, stageProgressText, summarizePrecisionQueue, summarizeStabilityBatch, type StabilityBatchCandidate } from '../ui-state'

const props = defineProps<{ run: ScanRun; elapsed: string }>()

const stabilityFirst = computed(() => props.run.config.stabilityFirst)
const stabilizing = computed(() => props.run.status === 'running' && props.run.progress.stage === 'stabilizing')
const discovering = computed(() => ['connecting', 'discovering'].includes(props.run.progress.stage))
const precision = computed(() => !discovering.value && props.run.status === 'running')
const phase = computed(() => stageProgressText(props.run))
const batch = computed(() => summarizeStabilityBatch(props.run))
const queue = computed(() => summarizePrecisionQueue(props.run))
const regionalLimit = computed(() => typeof props.run.config.regionalCandidateLimit === 'number' ? regionalPrecisionLimit(props.run.config) : null)
const active = computed(() => batch.value?.active.slice(0, 4) ?? [])
const steps = computed(() => stabilityFirst.value
  ? [{ stage: 'discovering', label: '发现候选' }, { stage: 'stabilizing', label: '稳定性' }, { stage: 'verifying', label: '服务验证' }, { stage: 'downloading', label: '下载测速' }]
  : [{ stage: 'discovering', label: '发现候选' }, { stage: 'verifying', label: '服务验证' }, { stage: 'stabilizing', label: '稳定性' }, { stage: 'downloading', label: '下载测速' }])
const currentStep = computed(() => steps.value.findIndex(step => step.stage === (discovering.value ? 'discovering' : props.run.progress.stage)))
const downloadedMiB = computed(() => (props.run.progress.downloadedBytes / 1024 / 1024).toFixed(1))
const regionalPools = computed(() => props.run.config.regions.map(code => ({ code, identified: props.run.progress.discoveredByRegion[code] ?? 0, pool: regionCandidatePool(props.run, code) })))
const batchCaption = computed(() => batch.value ? `本批稳定性已处理 ${batch.value.settled.length} / ${batch.value.candidates.length} 个 IP（${batch.value.percent}%）` : '正在准备本批候选')
const batchNumber = computed(() => props.run.progress.precisionBatch?.number ?? batch.value?.number)
const stagePercent = computed(() => stabilizing.value ? batch.value?.percent ?? null : phase.value.percent)
const stageCaption = computed(() => stabilizing.value ? batchCaption.value : phase.value.caption)
const currentScope = computed(() => discovering.value ? '发现阶段 · 连通性与 POP 识别各执行一次' : stabilityFirst.value ? '分区精测 · 稳定性全测 → 各区前 10 名服务校验 → 下载测速' : batchNumber.value ? `分区精测 · 第 ${batchNumber.value} 批（${regionalLimit.value == null ? '未找到足够结果会继续补位' : '仅在各区入围名单内补位'}）` : '分区精测 · 按批验证、检查稳定性并下载复测')
const criteria = computed(() => {
  const config = props.run.config
  if (config.stabilityAttempts == null || config.maxTcpMs == null) return '正在准备本批候选与本次稳定性判定参数。'
  if (config.stabilityFirst) return `累计 2 次连接失败立即淘汰，与允许失败率无关；其余候选测满 ${config.stabilityAttempts} 次后，按 ${stabilityMetricLabel(config.stabilityLatencyMetric ?? 'p95')} ≤ ${config.maxTcpMs} ms、连接失败率 ≤ ${config.maxStabilityFailurePercent}% 判定。全部入围候选测完，再从各区达标候选中选前 10 名服务校验与下载测速。`
  return `达标须测满 ${config.stabilityAttempts} 次，${stabilityMetricLabel(config.stabilityLatencyMetric ?? 'max')} ≤ ${config.maxTcpMs} ms、连接失败率 ≤ ${config.maxStabilityFailurePercent}%。已确定后续探测也无法达标时，立即淘汰并安排下一个；稳定性达标后还需下载复测。`
})
const downloadStatus = computed(() => {
  const progress = props.run.progress
  if (progress.downloadedBytes > 0) return `累计下载 ${(progress.downloadedBytes / 1024 / 1024).toFixed(1)} MiB`
  if (progress.stage === 'downloading') return '下载测速进行中，尚未读取响应体'
  if (progress.stage === 'connecting' || progress.stage === 'discovering' || stabilizing.value && batch.value?.number === 1) return '下载测速尚未开始'
  return stabilityFirst.value ? '全部稳定性测完后，仅对各区前 10 名进行下载测速' : '稳定性通过后进行下载测速'
})
const stopPolicy = computed(() => {
  const budgetStop = props.run.config.downloadBudgetEnabled !== false ? '下载预算用尽、' : ''
  if (stabilityFirst.value) return `先完成各区${regionalLimit.value == null ? '全部入围候选' : `前 ${regionalLimit.value} 个入围候选`}的稳定性测试，再从达标候选中选前 10 名服务校验和下载测速。入选名单全部测速，不从名单外补位；${budgetStop}名单测完或主动停止时结束。处理中包含等待任务。`
  return `${regionalLimit.value == null ? '' : `每区仅精测初筛 TCP 延迟最低的前 ${regionalLimit.value} 个，不补测未入围候选。`}每区找到 ${props.run.config.ipsPerRegion} 个达标结果即停止补测该区；${budgetStop}${regionalLimit.value == null ? '候选' : '入围名单'}测完或主动停止时结束。${queue.value.qualifiedRegions} / ${props.run.config.regions.length} 个地区已达到数量目标。处理中包含本批等待任务。`
})
function location(candidate: StabilityBatchCandidate) {
  const result = candidate.result
  if (!result?.colo) return '地区 / POP 待识别'
  if (!/^[A-Z]{2}$/.test(result.regionCode)) return `地区未识别 · ${result.colo}`
  return `${regionName(result.regionCode)} · ${result.colo}`
}
function milliseconds(value: number | null | undefined) { return value == null ? '—' : `${value.toFixed(1)} ms` }
function failure(candidate: StabilityBatchCandidate) {
  const value = observedFailurePercent(candidate.stability)
  return value == null ? '—' : `${value.toFixed(1)}%`
}
function verdict(candidate: StabilityBatchCandidate) {
  return candidate.result ? stabilityVerdict(candidate.result, props.run.config) : 'incomplete'
}
function p95(candidate: StabilityBatchCandidate) { return candidate.result ? completedTcpMetricValue(candidate.result, 'p95') : null }
</script>

<template>
  <div class="scan-progress-content">
    <div class="scan-progress-heading">
      <div class="scan-progress-title" aria-live="polite"><LoaderCircle :size="14" class="spinning" aria-hidden="true" /><strong>{{ stabilizing ? '正在检查 TCP 稳定性' : phase.title }}</strong></div>
      <div class="scan-run-summary"><span>已耗时 <strong>{{ elapsed }}</strong></span><span>已读取 <strong>{{ downloadedMiB }}</strong> MiB</span></div>
    </div>
    <div class="scan-phase-line">
      <ol class="scan-progress-steps" :aria-label="stabilityFirst ? '本轮测速流程' : '当前批次测速流程，可能循环补位'">
        <li v-for="(step, index) in steps" :key="step.stage" :class="{ current: currentStep === index, done: currentStep > index }" :aria-current="currentStep === index ? 'step' : undefined">
          <Check v-if="currentStep > index" :size="11" aria-hidden="true" /><span v-else class="scan-step-number">{{ index + 1 }}</span><span>{{ step.label }}</span><ArrowRight v-if="index < steps.length - 1" :size="10" class="scan-step-arrow" aria-hidden="true" />
        </li>
      </ol>
      <p class="scan-progress-caption">{{ stageCaption }}</p>
    </div>
    <div v-if="stagePercent != null" class="progress-track scan-stage-track" role="progressbar" :aria-label="stabilizing ? '本批稳定性候选处理进度' : '当前测速阶段进度'" :aria-valuenow="stagePercent" aria-valuemin="0" aria-valuemax="100" :aria-valuetext="stageCaption"><span :style="{ width: `${stagePercent}%` }" /></div>

    <details class="scan-details">
      <summary><span class="scan-cumulative-short" aria-live="polite">{{ precision ? `累计已处理 ${queue.processed} / ${queue.available} 个 · 完整测速通过 ${queue.measurementPassed} 个 · 处理中 ${queue.processing} · 待测 ${queue.queued}` : '查看地区识别与测试规则' }}</span><span class="scan-detail-toggle">测速详情<ChevronDown :size="12" aria-hidden="true" /></span></summary>
      <div class="scan-expanded-content">
        <p v-if="precision" class="scan-detail-description">累计已处理{{ regionalLimit == null ? '候选' : '入围候选' }} {{ queue.processed }} / {{ queue.available }} 个；本批处理中 {{ queue.processing }} · {{ regionalLimit == null ? '后备待选' : '入围待测' }} {{ queue.queued }}。已取得 {{ queue.qualified }} 个达标结果 · {{ queue.measurementPassed }} 个完整测速通过 · {{ queue.measurementPassed - queue.qualified }} 个低于速率目标 · {{ queue.earlyRejected }} 个提前淘汰。</p>
        <template v-if="stabilizing && batch">
          <p class="scan-batch-overview">本批 <strong>{{ batch.candidates.length }}</strong> 个 IP：测满 {{ batch.requestedPerIp }} 次 <strong>{{ batch.completed.length }}</strong> 个 · 提前淘汰 <strong>{{ batch.rejected.length }}</strong> 个 · 正在探测 <strong>{{ batch.active.length }}</strong> 个 · 等待 <strong>{{ batch.waiting.length }}</strong> 个</p>
          <div v-if="active.length" class="scan-active-grid" aria-label="正在探测的 IP，结果待复测完成">
            <article v-for="candidate in active" :key="`${batch.number}-${candidate.ip}`" class="scan-active-candidate">
              <div class="scan-candidate-count"><strong class="mono">{{ candidate.ip }}</strong><span>{{ candidate.attempted }} / {{ candidate.requested }} 次</span></div>
              <div class="progress-track scan-candidate-track" role="progressbar" :aria-label="`${candidate.ip} 已完成探测次数`" :aria-valuenow="candidate.attempted" aria-valuemin="0" :aria-valuemax="candidate.requested" :aria-valuetext="`已完成 ${candidate.attempted} / ${candidate.requested} 次，结果待复测完成`"><span :style="{ width: `${candidate.percent}%` }" /></div>
              <p class="muted">{{ location(candidate) }} · 结果待复测完成</p><p class="muted">连接成功 {{ candidate.stability?.succeeded ?? 0 }} 次 · 连接失败 {{ candidate.stability?.failed ?? 0 }} 次</p><p class="muted">耗时 &gt; {{ run.config.maxTcpMs }} ms 的样本 {{ candidate.stability?.overLimitCount ?? 0 }} 次（仅统计）</p>
            </article>
          </div>
          <details v-if="batch.settled.length" class="scan-batch-details"><summary><span>已处理 {{ batch.settled.length }} 个 IP · 查看稳定性结论</span><ChevronDown :size="12" aria-hidden="true" /></summary><p class="scan-detail-description">提前淘汰不会计作测满 {{ batch.requestedPerIp }} 次，也不会生成最终 P95；{{ stabilityFirst ? '稳定性达标后仍需进入本区前 10 名，再完成服务校验与下载复测。' : '稳定性通过仍需下载复测。' }}</p><ul><li v-for="candidate in batch.settled" :key="candidate.ip"><div class="scan-completed-identity"><span class="mono">{{ candidate.ip }}</span><span class="muted">{{ location(candidate) }}</span></div><template v-if="candidate.state === 'rejected'"><span class="scan-completed-values">已测 {{ candidate.attempted }} / {{ candidate.requested }} 次 · 连接失败 {{ candidate.stability?.failed ?? 0 }} 次</span><span class="status-label warning">提前淘汰</span><p class="scan-rejection-reason">{{ candidate.stability?.earlyRejection }}</p></template><template v-else><span class="scan-completed-values">最终 P95 {{ milliseconds(p95(candidate)) }} · 连接失败率 {{ failure(candidate) }}</span><span class="status-label" :class="{ good: verdict(candidate) === 'passed', warning: verdict(candidate) === 'failed', quiet: verdict(candidate) === 'incomplete' }">{{ verdict(candidate) === 'passed' ? '稳定性达标' : verdict(candidate) === 'failed' ? '稳定性未达标' : '统计待确认' }}</span></template></li></ul></details>
          <details v-if="batch.waiting.length" class="scan-batch-details"><summary><span>{{ batch.waiting.length }} 个候选等待开始</span><ChevronDown :size="12" aria-hidden="true" /></summary><ul><li v-for="candidate in batch.waiting" :key="candidate.ip"><span class="mono">{{ candidate.ip }}</span><span class="muted">{{ location(candidate) }}</span></li></ul></details>
        </template>
        <p v-else-if="stabilizing" class="scan-detail-description">正在准备本批候选；收到批次清单后，会逐个显示正在探测的 IP。</p>
        <div class="scan-rules">
          <p class="scan-detail-description">{{ currentScope }}</p><p class="scan-detail-description">{{ phase.explanation }}</p><p v-if="precision" class="scan-detail-description">{{ stopPolicy }}</p><p v-if="stabilizing" class="scan-detail-description">{{ criteria }}</p>
          <div class="scan-region-status"><span class="scan-footer-label">地区识别与精测入围（非测速通过数）</span><div class="scan-region-list"><div v-for="region in regionalPools" :key="region.code" class="scan-region-item"><span><span class="tiny-dot" :class="{ found: region.identified }" />{{ regionName(region.code) }} · 已识别 <strong>{{ region.identified }}</strong> 个 IP</span><template v-if="region.pool"><p>{{ region.pool.label }}</p><p>{{ region.pool.detail }}</p></template></div></div></div>
          <div class="scan-run-meta"><span>{{ precisionPolicyLabel(run.config) }}</span><span>{{ downloadStatus }} / {{ run.config.downloadBudgetEnabled !== false ? `预算 ${run.config.downloadBudgetMiB} MiB` : '不限制流量' }}</span><span>本次测试已耗时 {{ elapsed }}</span></div>
        </div>
      </div>
    </details>
  </div>
</template>

<style scoped>
.scan-progress-content { font-size: 12px; line-height: 20px; }
.scan-progress-heading, .scan-progress-title, .scan-run-summary, .scan-phase-line { display: flex; align-items: center; gap: 8px; }
.scan-progress-heading { justify-content: space-between; flex-wrap: wrap; gap: 3px 16px; }
.scan-progress-title { min-width: 0; }
.scan-progress-title>svg { color: var(--brand); flex: 0 0 auto; }
.scan-progress-title>strong { font-size: 12px; font-weight: 600; }
.scan-run-summary { color: var(--muted); gap: 16px; font-size: 11px; font-variant-numeric: tabular-nums; }
.scan-run-summary>span { white-space: nowrap; }
.scan-run-summary strong { color: var(--text); font-weight: 500; }
.scan-phase-line { justify-content: space-between; flex-wrap: wrap; gap: 0 16px; margin-top: 4px; }
.scan-progress-steps { display: flex; align-items: center; flex-wrap: wrap; gap: 8px; list-style: none; margin: 0; padding: 0; }
.scan-progress-steps li { display: flex; align-items: center; gap: 4px; color: var(--muted); font-size: 11px; line-height: 20px; white-space: nowrap; }
.scan-progress-steps li.current { color: var(--brand); font-weight: 600; }
.scan-progress-steps li.done { color: var(--good); }
.scan-step-number { font-size: 10px; font-variant-numeric: tabular-nums; }
.scan-step-arrow { color: var(--line-hover); margin-left: 3px; }
.scan-progress-caption { margin: 0; color: var(--muted); font-size: 11px; font-variant-numeric: tabular-nums; }
.scan-stage-track { height: 3px; margin: 5px 0 4px; border-radius: 2px; }
.scan-details>summary, .scan-batch-details>summary { display: flex; align-items: center; justify-content: space-between; gap: 12px; cursor: pointer; list-style: none; color: var(--muted); min-height: 20px; font-size: 11px; }
.scan-details>summary::-webkit-details-marker, .scan-batch-details>summary::-webkit-details-marker { display: none; }
.scan-detail-toggle { display: inline-flex; align-items: center; gap: 4px; color: var(--brand); white-space: nowrap; }
.scan-details[open]>summary .scan-detail-toggle>svg, .scan-batch-details[open]>summary>svg { transform: rotate(180deg); }
.scan-cumulative-short { font-variant-numeric: tabular-nums; }
.scan-expanded-content { margin-top: 8px; padding-top: 6px; border-top: 1px solid var(--line); }
.scan-detail-description, .scan-batch-overview { margin-top: 5px; color: var(--muted); font-size: 11px; line-height: 21px; overflow-wrap: anywhere; }
.scan-batch-overview strong { color: var(--text); font-weight: 500; }
.scan-active-grid { display: grid; grid-template-columns: repeat(2,minmax(0,1fr)); gap: 8px; margin-top: 8px; }
.scan-active-candidate { min-width: 0; padding: 8px 10px; border: 1px solid var(--line); border-radius: 5px; }
.scan-candidate-count { display: flex; align-items: center; justify-content: space-between; flex-wrap: wrap; gap: 4px 8px; }
.scan-candidate-count>strong { font-size: 12px; font-weight: 500; }
.scan-candidate-count>span, .scan-active-candidate p { font-size: 11px; font-variant-numeric: tabular-nums; line-height: 20px; }
.scan-candidate-track { height: 3px; margin: 5px 0; }
.scan-batch-details { margin-top: 6px; }
.scan-batch-details ul { list-style: none; margin: 6px 0 0; padding: 0; }
.scan-batch-details li { display: flex; align-items: center; justify-content: space-between; flex-wrap: wrap; gap: 4px 12px; padding: 6px 0; border-top: 1px solid var(--line); font-size: 11px; }
.scan-completed-identity { display: flex; align-items: center; gap: 8px; flex-wrap: wrap; }
.scan-completed-values { font-variant-numeric: tabular-nums; }
.scan-rejection-reason { flex-basis: 100%; color: var(--muted); overflow-wrap: anywhere; }
.scan-rules { margin-top: 8px; padding-top: 6px; border-top: 1px solid var(--line); }
.scan-region-status { margin-top: 8px; font-size: 11px; line-height: 20px; }
.scan-footer-label { display: block; margin-bottom: 5px; color: var(--muted); }
.scan-region-list { display: grid; grid-template-columns: repeat(2,minmax(0,1fr)); gap: 8px; }
.scan-region-item { min-width: 0; }
.scan-region-item>span { display: inline-flex; align-items: center; gap: 4px; }
.scan-region-item>p { color: var(--muted); font-size: 11px; line-height: 20px; }
.scan-region-list strong { font-weight: 500; font-variant-numeric: tabular-nums; }
.scan-run-meta { display: flex; align-items: center; flex-wrap: wrap; gap: 4px 12px; margin-top: 8px; font-size: 11px; line-height: 20px; color: var(--muted); }
@media (max-width: 600px) {
  .scan-active-grid, .scan-region-list { grid-template-columns: minmax(0,1fr); }
  .scan-progress-heading { align-items: flex-start; }
  .scan-run-summary { gap: 12px; }
  .scan-details>summary { align-items: flex-start; }
}
</style>
