<script setup lang="ts">
import { computed, ref } from 'vue'
import type { IpResult, ScanConfig } from '../types'
import { completedTcpMetricValue, initialTcpFailureReason, observedFailurePercent, stabilityMetricLabel, tcpConnectionFailureLabels } from '../ui-state'

const props = defineProps<{ result: IpResult; config: ScanConfig }>()
const stats = computed(() => props.result.tcpStability)
const samplesOpen = ref(false)

type ConnectFailure = NonNullable<IpResult['tcpConnectFailure']>

function milliseconds(value?: number | null) {
  return typeof value === 'number' && Number.isFinite(value) && value >= 0 ? `${value.toFixed(1)} ms` : ''
}

function failureMessage(failure: ConnectFailure) {
  const message = failure.message?.trim()
  return message?.replace(/^TCP (?:连接超时|连接失败|探测任务失败)(?:[：:]\s*|$)/, '') || ''
}

const initialFailure = computed(() => props.result.tcpConnectFailure)
const legacyInitialFailure = computed(() => initialTcpFailureReason(props.result))
const initialTime = computed(() => {
  const { tcpMs, attempts, colo } = props.result
  return tcpMs > 0 || (tcpMs === 0 && (attempts > 0 || !!colo)) ? milliseconds(tcpMs) : ''
})
const completeP95 = computed(() => milliseconds(completedTcpMetricValue(props.result, 'p95')))
const failurePercent = computed(() => observedFailurePercent(stats.value)?.toFixed(1))
const failureBreakdown = computed(() => {
  const details = stats.value?.failureDetails
  if (!details?.length || !stats.value) return ''
  const counts = { timeout: 0, connect: 0, worker: 0 }
  for (const failure of details) counts[failure.kind]++
  const parts = [
    counts.timeout ? `超时 ${counts.timeout} 次` : '',
    counts.connect ? `连接错误 ${counts.connect} 次` : '',
    counts.worker ? `探测任务异常 ${counts.worker} 次` : '',
  ].filter(Boolean)
  const unknown = stats.value.failed - details.length
  if (unknown > 0) parts.push(`类型未记录 ${unknown} 次`)
  return parts.join(' · ')
})
const probeRows = computed(() => {
  const failures = new Map(stats.value?.failureDetails?.map(failure => [failure.attempt, failure]))
  return (stats.value?.probesMs ?? []).map((sample, index) => {
    const attempt = index + 1
    const failure = sample === null ? failures.get(attempt) : undefined
    const slow = sample !== null && stats.value != null && sample > stats.value.delayLimitMs
    return {
      attempt,
      time: milliseconds(sample),
      outcome: sample !== null ? (slow ? '连接成功 · 超限' : '连接成功') : failure ? tcpConnectionFailureLabels[failure.kind] : '连接失败',
      failed: sample === null,
      slow,
      wait: milliseconds(failure?.elapsedMs),
      message: failure ? failureMessage(failure) : '',
    }
  })
})

function syncSamplesOpen(event: Event) {
  if (event.currentTarget instanceof HTMLDetailsElement) samplesOpen.value = event.currentTarget.open
}
</script>

<template>
  <section class="tcp-diagnostics" aria-label="TCP 诊断明细">
    <dl class="tcp-facts">
      <div>
        <dt>TCP 初筛</dt>
        <dd v-if="initialFailure">
          <span class="tcp-failed">{{ tcpConnectionFailureLabels[initialFailure.kind] }}</span>
          <span v-if="milliseconds(initialFailure.elapsedMs)"> · 失败前等待 {{ milliseconds(initialFailure.elapsedMs) }}</span>
          <span v-else class="muted"> · 等待时间未记录</span>
          <span v-if="failureMessage(initialFailure)" class="tcp-subtext">{{ failureMessage(initialFailure) }}</span>
        </dd>
        <dd v-else-if="legacyInitialFailure">
          <span class="tcp-failed">{{ legacyInitialFailure }}</span>
          <span class="muted"> · 等待时间未记录</span>
        </dd>
        <dd v-else-if="initialTime">连接成功 · {{ initialTime }}</dd>
        <dd v-else class="muted">暂无初筛记录</dd>
      </div>
      <div>
        <dt>TCP 连接等待上限</dt>
        <dd>{{ milliseconds(config.connectTimeoutMs) || '未记录' }}</dd>
      </div>
      <div v-if="config.stabilityAttempts != null">
        <dt>本轮稳定性设置</dt>
        <dd>
          计划 {{ config.stabilityAttempts }} 次 · 间隔 {{ config.stabilityIntervalMs }} ms
          <span class="tcp-subtext">
            <template v-if="config.maxTcpMs != null">{{ stabilityMetricLabel(config.stabilityLatencyMetric) }} ≤ {{ config.maxTcpMs }} ms · </template>
            允许失败率 {{ config.maxStabilityFailurePercent }}%
          </span>
        </dd>
      </div>
      <template v-if="stats?.attempted">
        <div>
          <dt>稳定性探测</dt>
          <dd>
            已测 {{ stats.attempted }} / {{ stats.requested }} 次 · 成功 {{ stats.succeeded }} 次
            <span class="tcp-subtext">失败 {{ stats.failed }} 次<template v-if="failurePercent !== undefined">（已测样本的 {{ failurePercent }}%）</template></span>
            <span v-if="failureBreakdown" class="tcp-subtext">{{ failureBreakdown }}</span>
          </dd>
        </div>
        <div>
          <dt>{{ stats.completed ? '成功样本延迟' : '已取得的成功样本延迟' }}</dt>
          <dd v-if="stats.succeeded > 0">
            平均 {{ milliseconds(stats.averageMs) || '—' }} · 中位 {{ milliseconds(stats.medianMs) || '—' }}
            <span class="tcp-subtext">范围 {{ milliseconds(stats.minMs) || '—' }}–{{ milliseconds(stats.maxMs) || '—' }}</span>
          </dd>
          <dd v-else class="muted">暂无成功样本</dd>
        </div>
        <div v-if="milliseconds(stats.delayLimitMs)">
          <dt>单次延迟超过 {{ milliseconds(stats.delayLimitMs) }}</dt>
          <dd>{{ stats.overLimitCount }} 次</dd>
        </div>
        <div v-if="completeP95">
          <dt>测满后的 P95</dt>
          <dd>{{ completeP95 }}</dd>
        </div>
      </template>
      <div v-else>
        <dt>稳定性探测</dt>
        <dd class="muted">暂无已执行的探测记录</dd>
      </div>
    </dl>

    <details v-if="stats?.probesMs?.length" class="tcp-sample-details" @toggle="syncSamplesOpen">
      <summary>查看逐次 TCP 探测（{{ stats.probesMs.length }} 次）</summary>
      <div v-if="samplesOpen" class="tcp-sample-scroll" role="region" aria-label="逐次 TCP 探测结果" tabindex="0">
        <table class="tcp-sample-table">
          <thead>
            <tr><th scope="col">探测序号</th><th scope="col">结果</th><th scope="col">建连耗时</th><th scope="col">失败前等待</th></tr>
          </thead>
          <tbody>
            <tr v-for="probe in probeRows" :key="probe.attempt">
              <th scope="row">{{ probe.attempt }}</th>
              <td>
                <span :class="{ 'tcp-failed': probe.failed, 'tcp-slow': probe.slow }">{{ probe.outcome }}</span>
                <span v-if="probe.message" class="tcp-subtext">{{ probe.message }}</span>
              </td>
              <td :class="{ 'tcp-slow': probe.slow }">{{ probe.time || '—' }}</td>
              <td>{{ probe.wait || (probe.failed ? '未记录' : '—') }}</td>
            </tr>
          </tbody>
        </table>
      </div>
    </details>
  </section>
</template>

<style scoped>
.tcp-diagnostics { min-width: 0; font-size: var(--font-control); color: var(--text); }
.tcp-facts { display: grid; grid-template-columns: repeat(auto-fit, minmax(min(200px, 100%), 1fr)); gap: var(--space-2) var(--space-4); margin: var(--space-2) 0; }
.tcp-facts dt { color: var(--muted); font-size: var(--font-caption); }
.tcp-facts dd { margin: 2px 0 0; font-variant-numeric: tabular-nums; overflow-wrap: anywhere; }
.tcp-subtext { display: block; color: var(--muted); font-size: var(--font-caption); overflow-wrap: anywhere; }
.tcp-failed { color: var(--error); }
.tcp-slow { color: var(--warn); font-weight: 600; }
.tcp-sample-details { margin-top: var(--space-2); }
.tcp-sample-details > summary { width: fit-content; color: var(--brand); cursor: pointer; }
.tcp-sample-scroll { max-height: 210px; margin-top: var(--space-2); overflow: auto; border: 1px solid var(--line); border-radius: var(--radius-control); }
.tcp-sample-table { width: 100%; min-width: 290px; border-collapse: separate; border-spacing: 0; font-size: var(--font-control); font-variant-numeric: tabular-nums; }
.tcp-sample-table th, .tcp-sample-table td { padding: var(--space-1) var(--space-2); border-bottom: 1px solid var(--line-subtle); text-align: left; vertical-align: top; }
.tcp-sample-table thead th { position: sticky; top: 0; z-index: 1; color: var(--muted); background: var(--bg); white-space: nowrap; }
.tcp-sample-table tbody th { color: var(--muted); font-weight: 400; }
.tcp-sample-table tbody tr:nth-child(even) { background: var(--surface-sunken); }
.tcp-sample-table td:nth-child(2) { white-space: normal; overflow-wrap: anywhere; }
.tcp-sample-table td:nth-last-child(-n+2) { white-space: nowrap; }
.tcp-sample-table td:last-child { min-width: 0; max-width: none; }
</style>
