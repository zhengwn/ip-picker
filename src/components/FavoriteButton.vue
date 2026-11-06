<script setup lang="ts">
import { Star } from '@lucide/vue'

defineProps<{ ip: string; saved?: boolean; disabled?: boolean; compact?: boolean }>()
defineEmits<{ click: [event: MouseEvent] }>()
</script>

<template>
  <button
    :class="[compact ? 'icon-button' : 'button', 'favorite-button', { 'is-saved': saved }]"
    :disabled="disabled"
    :aria-label="saved ? `管理 ${ip} 的收藏` : `收藏 ${ip} 的测试结果`"
    :title="saved ? '管理此 IP 的收藏' : '收藏 IP 和本次测试结果'"
    aria-haspopup="dialog"
    @click="$emit('click', $event)"
  >
    <Star :size="14" :fill="saved ? 'currentColor' : 'none'" aria-hidden="true" />
    <span v-if="!compact">{{ saved ? '管理收藏' : '收藏' }}</span>
  </button>
</template>

<style scoped>
.favorite-button.is-saved { color: var(--brand); }
</style>
