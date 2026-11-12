<script setup lang="ts">
import { computed } from 'vue'
import type { TcpDisplayMetric } from '../result-preferences'
import { describeTcpCell } from '../result-row-display'
import type { IpResult, ScanConfig } from '../types'
import TcpProbeChart from './TcpProbeChart.vue'

const props = defineProps<{
  result: IpResult
  displayMetric: TcpDisplayMetric
  scaleMaxMs: number
  runMetric?: ScanConfig['stabilityLatencyMetric']
  completedValue?: number | null
  labelRunMetric?: boolean
}>()

const stability = computed(() => props.result.tcpStability)
const measurement = computed(() => describeTcpCell(props.result, props.displayMetric, props.runMetric, props.completedValue, props.labelRunMetric))
</script>

<template>
  <div class="tcp-latency-cell">
    <TcpProbeChart v-if="stability?.probesMs?.length" class="tcp-latency-chart"
      :samples="stability.probesMs" :requested="stability.requested"
      :scale-max-ms="scaleMaxMs" :metrics-tooltip="measurement.tooltip" />
    <span v-if="measurement.value != null" class="tcp-latency-value" :title="measurement.tooltip">
      <small v-if="measurement.label">{{ measurement.label }}</small>
      <span>{{ measurement.value.toFixed(1) }}</span>
    </span>
    <span v-else class="tcp-latency-missing" :title="measurement.tooltip">—</span>
  </div>
</template>

<style scoped>
.tcp-latency-cell { display: flex; align-items: center; justify-content: flex-end; gap: 6px; min-width: 0; max-width: 100%; }
.tcp-latency-chart { flex: 0 1 96px; min-width: 16px; margin-inline-end: auto; }
.tcp-latency-value { display: inline-flex; flex-wrap: wrap; justify-content: flex-end; align-items: baseline; gap: 0 4px; min-width: min-content; font-variant-numeric: tabular-nums; }
.tcp-latency-value small, .tcp-latency-missing { color: var(--muted); font-size: var(--font-caption); }
.tcp-latency-value small, .tcp-latency-value span { white-space: nowrap; }
.tcp-latency-missing { line-height: 16px; }
@media (max-width: 940px) {
  .tcp-latency-chart { height: 15px; }
}
</style>
