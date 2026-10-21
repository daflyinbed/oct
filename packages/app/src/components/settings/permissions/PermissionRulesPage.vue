<template>
  <DialogTitle class="text-xl text-strong font-semibold">
    权限规则
  </DialogTitle>
  <p class="mt-1 text-sm text-dim">
    工具权限层的持久规则：匹配优先级 deny &gt; ask &gt; allow &gt;
    会话模式默认，deny 在任何模式（含全部放行）下都生效。{{
      inProjectScope
        ? `项目「${activeScopeProjectName}」的规则只对该项目生效；下方同时列出对所有项目生效的全局规则。`
        : "全局规则对所有项目的会话生效。"
    }}
  </p>
  <p v-if="error" class="mt-3 text-sm text-danger-10">{{ error }}</p>

  <!-- 新建表单：effect 三选 + 工具名 + 可选 pattern -->
  <div
    class="mt-4 border border-line-soft rounded-md border-dashed bg-surface px-3 py-2.5"
  >
    <div class="min-w-0 flex flex-wrap items-center gap-2">
      <AppSelect
        :model-value="effect"
        :options="EFFECT_OPTIONS"
        title="规则效果"
        trigger-class="w-[92px] text-neutral-10"
        @update:model-value="(v: string) => (effect = v)"
      />
      <input
        v-model="tool"
        type="text"
        placeholder="工具名，如 write_file / mcp__server__*"
        class="h-[26px] min-w-[180px] flex-1 border border-neutral-5 rounded-md bg-background px-2 text-sm text-neutral-10 outline-none transition-colors focus:border-accent-6 placeholder:text-faint"
      />
      <input
        v-model="pattern"
        type="text"
        placeholder="pattern（可空）"
        class="h-[26px] min-w-[160px] flex-1 border border-neutral-5 rounded-md bg-background px-2 text-code text-neutral-10 font-mono outline-none transition-colors focus:border-accent-6 placeholder:text-faint"
      />
      <button
        type="button"
        class="h-[26px] inline-flex flex-none items-center gap-1.5 control-surface rounded-md px-2.5 text-sm font-medium control-focus disabled:pointer-events-none disabled:opacity-50"
        :disabled="!tool.trim() || creating"
        :title="`在${inProjectScope ? '本项目' : '全局作用域'}创建规则`"
        @click="createRule"
      >
        <i-lucide-plus class="h-3 w-3" />
        添加规则
      </button>
    </div>
    <p class="mt-1.5 text-xs text-faint">
      pattern 留空 = 该工具的任意调用；execute_command
      为命令前缀，文件类工具为绝对路径
      glob（相对路径请以项目工作目录展开后填写）。
    </p>
  </div>

  <!-- 规则分组：本项目（仅项目作用域）在上、全局在下 -->
  <template v-for="group in groups" :key="group.label">
    <div
      class="mt-5 border-b border-line border-dashed pb-1.5 text-xs text-dim tracking-[0.5px] font-mono"
    >
      {{ group.label }}
    </div>
    <p v-if="group.rules.length === 0" class="py-2.5 text-sm text-faint">
      暂无规则。
    </p>
    <div
      v-for="rule in group.rules"
      :key="rule.id"
      class="min-w-0 flex items-center gap-2 border-b border-line-soft border-dashed py-2.5 last:border-b-0"
    >
      <span
        class="h-[16px] inline-flex flex-none items-center whitespace-nowrap rounded-full px-1.5 text-badge font-medium leading-none"
        :class="EFFECT_META[rule.effect]?.badge ?? 'bg-neutral-2 text-dim'"
        >{{ EFFECT_META[rule.effect]?.label ?? rule.effect }}</span
      >
      <span
        class="min-w-0 flex-1 truncate text-base text-strong font-mono"
        :title="rule.tool"
        >{{ rule.tool }}</span
      >
      <span
        v-if="rule.pattern"
        class="max-w-[38%] min-w-0 flex-none truncate text-xs text-dim font-mono"
        :title="rule.pattern"
        >{{ rule.pattern }}</span
      >
      <span
        class="flex-none rounded bg-neutral-2 px-1.5 py-px text-badge text-dim font-normal leading-[16px]"
        :title="
          rule.source === 'approval'
            ? '来自审批卡的「总是允许」'
            : '来自设置页手动创建'
        "
      >
        {{ rule.source === "approval" ? "审批产生" : "手动" }}
      </span>
      <button
        class="h-[24px] w-[24px] flex-none rounded-md text-dim control-focus transition-colors active:bg-neutral-4 hover:bg-neutral-3 hover:text-neutral-10"
        :title="`删除规则：${rule.tool}`"
        @click="removeRule(rule)"
      >
        <i-lucide-trash-2 class="h-3.5 w-3.5" />
      </button>
    </div>
  </template>

  <div
    v-if="groups.length === 0 && !loading"
    class="py-10 text-center text-base text-faint"
  >
    {{
      inProjectScope
        ? "本项目与全局作用域下均无规则。"
        : "全局作用域下尚无规则。"
    }}
  </div>
  <p v-if="loading" class="py-8 text-center text-base text-faint">Loading…</p>
</template>

<script setup lang="ts">
import { DialogTitle } from "reka-ui";
import { computed, ref, watch } from "vue";
import client from "@/api/client";
import AppSelect from "@/components/ui/AppSelect.vue";
import type { components } from "@/api/schema";

type PermissionRuleDto = components["schemas"]["PermissionRuleDto"];

const props = defineProps<{
  /** 当前是否项目作用域（壳层的 scope 派生值）。 */
  inProjectScope: boolean;
  /** 作用域 id：全局哨兵或项目 id（项目作用域时是有效的项目 id）。 */
  scopeId: string;
  /** 项目作用域下的项目名（文案用）。 */
  activeScopeProjectName: string;
}>();

// --- effect 词表 -------------------------------------------------------------

const EFFECT_OPTIONS: readonly { value: string; label: string }[] = [
  { value: "allow", label: "允许" },
  { value: "deny", label: "拒绝" },
  { value: "ask", label: "询问" },
];

const EFFECT_META: Record<string, { label: string; badge: string }> = {
  allow: { label: "允许", badge: "bg-success-7 text-success-10" },
  deny: { label: "拒绝", badge: "bg-danger-7 text-danger-10" },
  ask: { label: "询问", badge: "bg-warning-7 text-warning-10" },
};

// --- 列表加载（作用域切换即重拉） ----------------------------------------------

const rules = ref<PermissionRuleDto[]>([]);
const loading = ref(false);
const error = ref<string | null>(null);

async function loadRules() {
  loading.value = true;
  error.value = null;
  try {
    const { data, error: err } = props.inProjectScope
      ? await client.GET("/api/projects/{projectId}/permission-rules", {
          params: { path: { projectId: props.scopeId } },
        })
      : await client.GET("/api/permission-rules");
    if (err || !data) {
      error.value = "规则列表加载失败。";
      return;
    }
    rules.value = data;
  } catch {
    error.value = "规则列表加载失败。";
  } finally {
    loading.value = false;
  }
}

watch(
  () => props.scopeId,
  () => void loadRules(),
  { immediate: true },
);

/** 分组：项目作用域 = 本项目 + 全局两组；全局作用域 = 仅全局组。 */
const groups = computed(() => {
  const globalRules = rules.value.filter((rule) => !rule.project_id);
  const result: { label: string; rules: PermissionRuleDto[] }[] = [];
  if (props.inProjectScope) {
    const projectRules = rules.value.filter(
      (rule) => rule.project_id === props.scopeId,
    );
    result.push({
      label: `本项目（${props.activeScopeProjectName || "未命名项目"}）`,
      rules: projectRules,
    });
  }
  if (globalRules.length > 0) {
    result.push({ label: "全局", rules: globalRules });
  }
  return result;
});

// --- 新建 ---------------------------------------------------------------------

const effect = ref("allow");
const tool = ref("");
const pattern = ref("");
const creating = ref(false);

async function createRule() {
  const name = tool.value.trim();
  if (!name || creating.value) return;
  creating.value = true;
  error.value = null;
  try {
    const { data, error: err } = props.inProjectScope
      ? await client.POST("/api/projects/{projectId}/permission-rules", {
          params: { path: { projectId: props.scopeId } },
          body: {
            effect: effect.value,
            tool: name,
            pattern: pattern.value.trim() || null,
          },
        })
      : await client.POST("/api/permission-rules", {
          body: {
            effect: effect.value,
            tool: name,
            pattern: pattern.value.trim() || null,
          },
        });
    if (err || !data) {
      error.value = "规则创建失败（检查 effect 与工具名）。";
      return;
    }
    rules.value = [...rules.value, data];
    tool.value = "";
    pattern.value = "";
  } catch {
    error.value = "规则创建失败。";
  } finally {
    creating.value = false;
  }
}

// --- 删除 ---------------------------------------------------------------------

async function removeRule(rule: PermissionRuleDto) {
  error.value = null;
  try {
    const { error: err } = await client.DELETE("/api/permission-rules/{id}", {
      params: { path: { id: rule.id } },
    });
    if (err) {
      error.value = "规则删除失败。";
      return;
    }
    rules.value = rules.value.filter((item) => item.id !== rule.id);
  } catch {
    error.value = "规则删除失败。";
  }
}
</script>
