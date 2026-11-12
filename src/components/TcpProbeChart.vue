<script setup lang="ts">
import { computed } from 'vue'

const props = defineProps<{
  samples: readonly (number | null)[]
  requested: number
  scaleMaxMs: number
  metricsTooltip?: string
}>()

const width = 96
const height = 18
const maxBars = 100
const format = new Intl.NumberFormat('zh-CN', { maximumFractionDigits: 1 })

function bar(left: number, right: number, top: number): string {
  const start = left.toFixed(2)
  return `M${start} ${top.toFixed(2)}H${right.toFixed(2)}V${height}H${start}Z`
}

const chart = computed(() => {
  const attempted = props.samples.length
  if (!attempted) return null

  const barCount = Math.min(attempted, maxBars)
  const buckets = Array.from({ length: barCount }, () => ({ peak: -1, failed: false }))
  let succeeded = 0
  let failed = 0
  let min = Infinity
  let max = 0

  props.samples.forEach((sample, index) => {
    const bucket = buckets[Math.floor(index * barCount / attempted)]
    if (sample === null) {
      bucket.failed = true
      failed++
    } else if (Number.isFinite(sample) && sample >= 0) {
      bucket.peak = Math.max(bucket.peak, sample)
      min = Math.min(min, sample)
      max = Math.max(max, sample)
      succeeded++
    }
  })
  if (!succeeded && !failed) return null

  const scale = Math.max(1, max, Number.isFinite(props.scaleMaxMs) ? props.scaleMaxMs : 0)
  const step = width / barCount
  const successfulBars: string[] = []
  const failureBars: string[] = []

  buckets.forEach(({ peak, failed }, index) => {
    const left = index * step
    const right = (index + 1) * step
    const middle = left + step / 2
    if (peak >= 0) {
      const barHeight = Math.max(1.5, peak / scale * height)
      successfulBars.push(bar(left, failed ? middle : right, height - barHeight))
    }
    if (failed) {
      const failureWidth = Math.min(2.5, peak >= 0 ? step / 2 : step)
      const center = peak >= 0 ? middle + step / 4 : middle
      failureBars.push(bar(center - failureWidth / 2, center + failureWidth / 2, 0))
    }
  })

  const probeCount = Number.isInteger(props.requested) && props.requested >= attempted
    ? `${attempted} / ${props.requested}`
    : String(attempted)
  const details = [
    `TCP 逐次探测，已探测 ${probeCount} 次，成功 ${succeeded} 次，失败 ${failed} 次。`,
    succeeded ? `延迟 ${format.format(min)}–${format.format(max)} ms，刻度 0–${format.format(scale)} ms。` : '',
    '从左到右按探测顺序排列，绿柱高度表示延迟，红柱表示连接失败。',
    attempted > maxBars ? '每组显示最高延迟及是否有连接失败。' : '',
  ]
  return {
    successfulBars: successfulBars.join(' '),
    failureBars: failureBars.join(' '),
    label: details.join(''),
  }
})
</script>

<template>
  <svg v-if="chart" class="tcp-probe-chart" role="img" :aria-label="chart.label"
    :width="width" :height="height" :viewBox="`0 0 ${width} ${height}`" preserveAspectRatio="none">
    <title>{{ metricsTooltip ? chart.label + '\n' + metricsTooltip : chart.label }}</title>
    <path v-if="chart.successfulBars" class="successful-probes" :d="chart.successfulBars" />
    <path v-if="chart.failureBars" class="failed-probes" :d="chart.failureBars" />
  </svg>
</template>

<style scoped>
.tcp-probe-chart { display: block; max-width: 100%; }
.successful-probes { fill: var(--good); }
.failed-probes { fill: var(--error); }
</style>
