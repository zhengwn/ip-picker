<script setup lang="ts">
import { ChevronDown } from '@lucide/vue'

defineProps<{ compact?: boolean }>()
</script>

<template>
  <span class="select-control" :class="{ 'is-compact': compact }">
    <slot />
    <ChevronDown class="select-control-icon" :size="compact ? 14 : 16" aria-hidden="true" focusable="false" />
  </span>
</template>
