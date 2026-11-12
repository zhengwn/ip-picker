<script setup lang="ts">
import type { TcpDisplayMetric } from '../result-preferences'
import type { ScanConfig } from '../types'
import { stabilityMetricLabel } from '../ui-state'
import SelectControl from './SelectControl.vue'

defineProps<{ runMetric?: ScanConfig['stabilityLatencyMetric'] }>()
const displayMetric = defineModel<TcpDisplayMetric>({ required: true })
</script>

<template>
  <div class="tcp-latency-header">
    <div class="tcp-latency-heading">
      <span>TCP<span class="tcp-latency-detail"> 延迟</span></span>
      <SelectControl compact class="tcp-latency-control" :class="{ 'tcp-latency-control-wide': runMetric === 'average' }">
        <select v-model="displayMetric" aria-label="TCP 延迟列显示指标" title="切换每行显示的 TCP 指标">
          <option value="runMetric">{{ runMetric ? stabilityMetricLabel(runMetric) : '复测' }}</option>
          <option value="median">中位</option>
          <option value="average">{{ runMetric === 'average' ? '样本平均' : '平均' }}</option>
          <option value="peak">峰值</option>
        </select>
      </SelectControl>
      <span class="tcp-latency-unit">ms</span>
    </div>
  </div>
</template>

<style scoped>
.tcp-latency-header { min-width: 0; }
.tcp-latency-heading { display: flex; justify-content: flex-end; align-items: center; gap: 5px; white-space: nowrap; }
.tcp-latency-control { flex: 0 0 57px; }
.tcp-latency-control-wide { flex-basis: 74px; }
.tcp-latency-heading .tcp-latency-control :deep(select) { min-height: 22px; height: 22px; padding: 0 17px 0 6px; border-radius: 4px; background: var(--panel); font-size: 11px; line-height: 18px; }
.tcp-latency-heading .tcp-latency-control :deep(.select-control-icon) { right: 4px; width: 11px; height: 11px; }
.tcp-latency-unit { display: inline-block; flex: none; height: 15px; padding: 0 4px; border: 1px solid color-mix(in srgb, var(--line) 85%, transparent); border-radius: 4px; background: color-mix(in srgb, var(--panel) 70%, transparent); font-size: 9px; font-weight: 400; line-height: 13px; }
@media (max-width: 940px) {
  .tcp-latency-heading { gap: 2px; }
  .tcp-latency-detail { display: none; }
}
</style>
