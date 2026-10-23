<template>
  <div class="text-xs space-y-1">
    <div
      v-for="call in calls"
      :key="call.id"
      class="flex items-start gap-2 rounded bg-surface px-2.5 py-1.5"
    >
      <i-lucide-loader-circle
        v-if="call.status === 'running' && running"
        class="mt-0.5 h-3 w-3 flex-none animate-spin text-accent-7"
      />
      <span
        v-else
        class="flex-none"
        :class="
          call.status === 'completed' ? 'text-success-10' : 'text-warning-10'
        "
        >{{
          call.status === "completed"
            ? "完成"
            : call.status === "failed"
              ? "失败"
              : "结果未知"
        }}</span
      >
      <div class="min-w-0">
        <div class="break-all text-dim">{{ call.title || call.name }}</div>
        <div
          v-if="call.duration_ms != null || call.error_code"
          class="text-badge text-dim"
        >
          <span v-if="call.duration_ms != null">{{ call.duration_ms }} ms</span>
          <span v-if="call.error_code" class="ml-2">{{ call.error_code }}</span>
        </div>
        <div v-if="call.error" class="break-all text-danger-10">
          {{ call.error }}
        </div>
      </div>
    </div>
  </div>
</template>

<script setup lang="ts">
import type { NestedCall } from "./toolCardMetadata";

defineProps<{ calls: NestedCall[]; running: boolean }>();
</script>
