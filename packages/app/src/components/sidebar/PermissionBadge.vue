<template>
  <!-- 权限模式档位：非 default 时显示短标 -->
  <span
    v-if="badge"
    class="mr-1.5 h-[15px] inline-flex flex-none items-center whitespace-nowrap rounded-full px-1.5 text-[10px] font-medium leading-none"
    :class="
      isBypass ? 'bg-warning-7 text-warning-10' : 'bg-accent-3 text-accent-10'
    "
    :title="badge.title"
  >
    {{ badge.label }}
  </span>
</template>

<script setup lang="ts">
import { computed } from "vue";
import {
  normalizePermissionMode,
  permissionModeBadge,
} from "@/composables/useProjects";

const props = defineProps<{
  mode: string;
}>();

const badge = computed(() => permissionModeBadge(props.mode));
const isBypass = computed(
  () => normalizePermissionMode(props.mode) === "bypass",
);
</script>
