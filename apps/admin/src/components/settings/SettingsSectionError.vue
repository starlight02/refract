<script setup lang="ts">
defineProps<{
  message?: string | null
}>()

const emit = defineEmits<{
  retry: []
}>()
</script>

<template>
  <div
    v-if="message"
    class="flex items-center justify-between rounded-lg bg-danger/10 p-3 text-xs text-danger"
  >
    <span>加载失败：{{ message }}</span>
    <button type="button" class="font-medium hover:underline" @click="emit('retry')">重试</button>
  </div>
</template>
