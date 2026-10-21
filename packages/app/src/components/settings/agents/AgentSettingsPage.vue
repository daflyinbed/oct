<template>
  <div>
    <h2 class="text-xl text-strong font-semibold">Agents</h2>
    <p class="mt-1 text-sm text-dim">
      分别配置子代理与自动标题；项目按字段覆盖全局。
    </p>
    <div class="mt-5 flex flex-wrap gap-2">
      <button
        v-for="item in views"
        :key="item.key"
        class="rounded-md px-3 py-1.5 text-sm control-focus"
        :class="
          selectedKey === item.key
            ? 'bg-neutral-3 text-strong'
            : 'control-surface'
        "
        :disabled="saving"
        @click="selectAgent(item.key)"
      >
        {{ item.name }}
      </button>
    </div>
    <p v-if="loading" class="mt-5 text-sm text-dim">加载配置与工具目录…</p>
    <p v-if="error" role="alert" class="mt-4 text-sm text-red-9">
      {{ error }}
    </p>
    <template v-if="current && !loading">
      <p class="mt-4 text-sm text-dim">{{ current.description }}</p>
      <div class="mt-5 space-y-5">
        <section>
          <div
            class="mb-2 flex items-center gap-2 text-base text-strong font-semibold"
          >
            模型
            <span class="text-xs text-dim font-normal">{{
              sourceLabel(current.sources.model)
            }}</span>
          </div>
          <div class="flex flex-wrap gap-2">
            <AppSelect
              v-model="modelMode"
              :disabled="saving"
              :options="modelModes"
              title="Agent 模型策略"
            />
            <AppSelect
              v-if="modelMode === 'fixed'"
              v-model="modelValue"
              :disabled="saving"
              :options="modelChoices"
              title="Agent 专用模型"
              placeholder="选择模型"
              trigger-class="max-w-[420px]"
            />
          </div>
          <p class="mt-2 text-xs text-dim">
            当前生效：{{ modelSummary(current.effective.model) }}
          </p>
        </section>
        <section>
          <div
            class="mb-2 flex items-center gap-2 text-base text-strong font-semibold"
          >
            {{ selectedKey === "title" ? "额外标题指令" : "额外角色指令" }}
            <span class="text-xs text-dim font-normal">{{
              sourceLabel(current.sources.instructions)
            }}</span>
          </div>
          <AppSelect
            v-if="projectId"
            v-model="instructionsMode"
            :disabled="saving"
            :options="[
              { value: 'inherit', label: '继承全局' },
              { value: 'custom', label: '项目自定义（覆盖全局）' },
            ]"
            title="Agent 指令策略"
          />
          <textarea
            v-if="instructionsMode === 'custom'"
            v-model="instructions"
            :disabled="saving"
            class="mt-2 min-h-[120px] w-full border border-line-soft rounded-md bg-surface p-3 text-sm text-neutral-10 outline-none focus:border-accent-6"
            :aria-label="
              selectedKey === 'title' ? '额外标题指令' : '额外角色指令'
            "
            placeholder="填写额外要求；留空表示不使用额外指令。"
          />
          <p v-else class="mt-2 whitespace-pre-wrap text-sm text-dim">
            {{ current.global_effective.instructions || "无全局额外指令" }}
          </p>
          <details class="mt-2 text-xs text-dim">
            <summary class="cursor-pointer">内置提示词说明</summary>
            <p class="mt-1">
              {{
                selectedKey === "title"
                  ? "生成简短单行标题，保留关键技术信息，仅输出标题。"
                  : "独立上下文完成委派任务，报告结论、证据、验证结果及未解决事项；工具说明按实际开放列表生成。"
              }}
            </p>
          </details>
        </section>
        <section>
          <div
            class="mb-2 flex items-center gap-2 text-base text-strong font-semibold"
          >
            工具
            <span class="text-xs text-dim font-normal">{{
              sourceLabel(current.sources.tools)
            }}</span>
          </div>
          <p v-if="!current.tools_editable" class="text-sm text-dim">
            无工具 · 单次标题生成
          </p>
          <template v-else>
            <AppSelect
              v-model="toolsMode"
              :disabled="saving"
              :options="toolModes"
              title="Agent 工具策略"
            />
            <div v-if="toolsMode === 'allowlist'" class="mt-3 space-y-3">
              <p class="text-xs text-dim">
                {{
                  projectId ? "覆盖全局工具选择。" : "只开放下方选中的工具。"
                }}全不选表示无工具；选中工具仍遵守审批规则。
              </p>
              <div v-for="group in toolGroups" :key="group.name">
                <div class="mb-1 text-sm text-dim font-semibold">
                  {{ group.name }}
                </div>
                <label
                  v-for="option in group.items"
                  :key="toolKey(option.selector)"
                  class="flex cursor-pointer items-center gap-2 py-1 text-sm text-neutral-10"
                >
                  <input
                    type="checkbox"
                    :checked="
                      selectedTools.some(
                        (t) => toolKey(t) === toolKey(option.selector),
                      )
                    "
                    :disabled="
                      saving ||
                      isLockedReadTool(option.selector) ||
                      (!option.available &&
                        !selectedTools.some(
                          (t) => toolKey(t) === toolKey(option.selector),
                        ))
                    "
                    @change="
                      toggleTool(
                        option.selector,
                        ($event.target as HTMLInputElement).checked,
                      )
                    "
                  />
                  {{ option.label
                  }}<span
                    v-if="isLockedReadTool(option.selector)"
                    class="text-faint"
                    >（代码模式必需）</span
                  ><span v-if="!option.available" class="text-faint"
                    >（不可用）</span
                  >
                </label>
              </div>
              <p class="text-xs text-dim">
                edit_file 需要 read_file，选择会联动。execute_command 与 MCP
                可能修改数据。
              </p>
              <div v-if="requireReadTools" class="flex items-center gap-2">
                <p class="text-xs text-dim">
                  代码模式已启用：read_file / grep / glob
                  用于浏览本运行生成的工具
                  SDK，非空列表必须包含（后端同样校验）。
                </p>
                <button
                  type="button"
                  class="shrink-0 control-ghost rounded-md px-2.5 py-1 text-xs control-focus"
                  :disabled="saving"
                  @click="clearAllTools"
                >
                  清空（无工具）
                </button>
              </div>
            </div>
            <p class="mt-2 text-xs text-dim">
              当前生效：{{
                current.effective.tools.mode === "parent"
                  ? "跟随父代理可派生工具"
                  : `${current.effective.tools.items.length} 个工具`
              }}
            </p>
          </template>
        </section>
      </div>
      <div class="mt-6 flex items-center gap-2">
        <button
          class="control-surface rounded-md px-3 py-1.5 text-sm control-focus"
          :disabled="saving || !validModel"
          @click="saveDraft"
        >
          {{ saving ? "保存中…" : "保存配置" }}
        </button>
        <button
          class="control-ghost rounded-md px-3 py-1.5 text-sm control-focus"
          :disabled="saving"
          @click="reset"
        >
          {{ projectId ? "全部恢复继承" : "恢复内置默认" }}
        </button>
        <span v-if="dirty" class="text-xs text-dim">有未保存修改</span>
      </div>
      <p v-if="saved" role="status" class="mt-3 text-sm text-dim">
        {{ saved }}
      </p>
    </template>
  </div>
  <AlertDialogRoot :open="discardOpen" @update:open="discardOpen = $event">
    <AlertDialogPortal>
      <AlertDialogOverlay class="fixed inset-0 z-60 bg-overlay" />
      <AlertDialogContent
        class="fixed left-1/2 top-1/2 z-60 w-[360px] border border-line-soft rounded-xl bg-panel p-5 shadow-[var(--oct-shadow)] -translate-x-1/2 -translate-y-1/2"
      >
        <AlertDialogTitle class="text-lg text-strong font-semibold"
          >丢弃未保存修改？</AlertDialogTitle
        >
        <AlertDialogDescription class="mt-2 text-sm text-dim"
          >离开当前配置后，未保存的修改将被丢弃。</AlertDialogDescription
        >
        <div class="mt-5 flex justify-end gap-2">
          <AlertDialogCancel
            class="control-surface rounded-md px-3 py-1.5 text-sm"
            >继续编辑</AlertDialogCancel
          >
          <AlertDialogAction
            class="control-surface rounded-md px-3 py-1.5 text-sm"
            @click="discard"
            >丢弃修改</AlertDialogAction
          >
        </div>
      </AlertDialogContent>
    </AlertDialogPortal>
  </AlertDialogRoot>
</template>

<script setup lang="ts">
import {
  AlertDialogAction,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogOverlay,
  AlertDialogPortal,
  AlertDialogRoot,
  AlertDialogTitle,
} from "reka-ui";
import { computed, onBeforeUnmount, ref, watch } from "vue";
import client from "@/api/client";
import AppSelect from "@/components/ui/AppSelect.vue";
import { toolKey, useAgentSettings } from "@/composables/useAgentSettings";
import { useSettings } from "@/composables/useSettings";
import type { components } from "@/api/schema";
import type { ToolSelector } from "@/composables/useAgentSettings";

const props = defineProps<{ projectId: string | null }>();
const editor = useAgentSettings();
const { views, tools, loading, saving, error, saved, modelOptions } = editor;
const settings = useSettings();
const selectedKey = ref("worker");
const current = computed(() =>
  views.value.find((v) => v.key === selectedKey.value),
);
const modelMode = ref("parent");
const modelValue = ref("");
const instructionsMode = ref("custom");
const instructions = ref("");
const toolsMode = ref("parent");
const selectedTools = ref<ToolSelector[]>([]);
const baseline = ref("");
const serializedDraft = computed(() =>
  JSON.stringify([
    modelMode.value,
    modelValue.value,
    instructionsMode.value,
    instructions.value,
    toolsMode.value,
    selectedTools.value,
  ]),
);
const dirty = computed(
  () => !!baseline.value && serializedDraft.value !== baseline.value,
);
const discardOpen = ref(false);
let pendingLeave: (() => void) | undefined;
function requestLeave(action: () => void) {
  if (saving.value) return;
  if (dirty.value) {
    pendingLeave = action;
    discardOpen.value = true;
  } else action();
}
function discard() {
  const action = pendingLeave;
  pendingLeave = undefined;
  discardOpen.value = false;
  action?.();
}
defineExpose({ dirty, requestLeave });

// 代码模式下 worker 用这三个只读工具浏览自己的 SDK：非空列表必须包含。
// 状态必须先于下方 immediate watcher 初始化——watcher 在 setup 期同步调用
// loadCodeMode，晚于此处的 let 声明会触发 TDZ ReferenceError。
const REQUIRED_READ_TOOLS = ["read_file", "grep", "glob"] as const;
const codeMode = ref(false);
let codeModeRequest = 0;
async function loadCodeMode(id: string | null) {
  const request = ++codeModeRequest;
  try {
    if (id) {
      const { data, error: apiError } = await client.GET(
        "/api/projects/{id}/settings/mcp",
        { params: { path: { id } } },
      );
      if (request !== codeModeRequest || apiError) return;
      codeMode.value = data.effective_mcp_tool_mode === "code";
    } else {
      // 强制刷新：模式可能在其它入口被修改过，单例缓存的旧值不可信。
      await settings.load(true);
      if (request !== codeModeRequest) return;
      codeMode.value = settings.mcpToolMode.value === "code";
    }
  } catch {
    // 仅影响提示，加载失败不打断面板。
  }
}
const requireReadTools = computed(
  () =>
    codeMode.value &&
    selectedKey.value === "worker" &&
    toolsMode.value === "allowlist" &&
    selectedTools.value.length > 0,
);
watch(requireReadTools, (required) => {
  if (!required) return;
  for (const name of REQUIRED_READ_TOOLS)
    if (
      !selectedTools.value.some((t) => t.kind === "builtin" && t.name === name)
    )
      selectedTools.value.push({ kind: "builtin", name });
});
function isLockedReadTool(selector: ToolSelector) {
  return (
    requireReadTools.value &&
    selector.kind === "builtin" &&
    (REQUIRED_READ_TOOLS as readonly string[]).includes(selector.name)
  );
}
// 锁定的三件套无法逐个取消；清空是回到「无工具」worker 的唯一出口。
function clearAllTools() {
  selectedTools.value = [];
}

watch(
  () => props.projectId,
  (id) => {
    baseline.value = "";
    void editor.load(id);
    void loadCodeMode(id);
  },
  { immediate: true },
);
watch(current, (value) => {
  if (!value) return;
  const local = value.overrides;
  modelMode.value =
    local.model?.mode ?? (props.projectId ? "inherit" : "parent");
  const model = local.model ?? value.effective.model;
  modelValue.value =
    model.mode === "fixed"
      ? JSON.stringify([model.provider_id, model.model_id])
      : "";
  instructionsMode.value =
    props.projectId && local.instructions === undefined ? "inherit" : "custom";
  instructions.value = local.instructions ?? value.effective.instructions;
  toolsMode.value =
    local.tools?.mode ?? (props.projectId ? "inherit" : "parent");
  const choices = local.tools ?? value.effective.tools;
  selectedTools.value =
    choices.mode === "allowlist" ? choices.items.map((t) => ({ ...t })) : [];
  baseline.value = serializedDraft.value;
});
onBeforeUnmount(editor.dispose);

const modelModes = computed(() => [
  ...(props.projectId ? [{ value: "inherit", label: "继承全局" }] : []),
  {
    value: "parent",
    label: selectedKey.value === "title" ? "跟随会话模型" : "跟随父代理",
  },
  { value: "fixed", label: "指定模型" },
]);
const toolModes = computed(() => [
  ...(props.projectId ? [{ value: "inherit", label: "继承全局" }] : []),
  { value: "parent", label: "跟随父代理" },
  { value: "allowlist", label: "自定义列表" },
]);
const modelChoices = computed(() =>
  modelValue.value &&
  !modelOptions.value.some((o) => o.value === modelValue.value)
    ? [
        ...modelOptions.value,
        { value: modelValue.value, label: "已选模型（目录中不存在）" },
      ]
    : modelOptions.value,
);
const validModel = computed(
  () => modelMode.value !== "fixed" || !!modelValue.value,
);
const toolGroups = computed(() => {
  const options = [...tools.value];
  for (const selected of selectedTools.value)
    if (!options.some((o) => toolKey(o.selector) === toolKey(selected))) {
      options.push({
        selector: selected,
        label: selected.kind === "builtin" ? selected.name : selected.tool,
        group: "已选但不可用",
        available: false,
      });
    }
  return Array.from(new Set(options.map((o) => o.group)), (name) => ({
    name,
    items: options.filter((o) => o.group === name),
  }));
});
function sourceLabel(source: string) {
  return (
    { builtin: "内置默认", global: "来自全局", project: "本项目覆盖" }[
      source
    ] ?? source
  );
}
function modelSummary(model: components["schemas"]["ModelChoice"]) {
  return model.mode === "parent"
    ? selectedKey.value === "title"
      ? "跟随会话模型"
      : "跟随父代理"
    : `${model.provider_id} / ${model.model_id}`;
}
function selectAgent(key: string) {
  if (key === selectedKey.value) return;
  requestLeave(() => {
    selectedKey.value = key;
    saved.value = "";
    error.value = "";
  });
}
function toggleTool(tool: ToolSelector, checked: boolean) {
  const key = toolKey(tool);
  selectedTools.value = selectedTools.value.filter((t) => toolKey(t) !== key);
  if (checked) selectedTools.value.push(tool);
  if (
    tool.kind === "builtin" &&
    tool.name === "edit_file" &&
    checked &&
    !selectedTools.value.some(
      (t) => t.kind === "builtin" && t.name === "read_file",
    )
  )
    selectedTools.value.push({ kind: "builtin", name: "read_file" });
  if (tool.kind === "builtin" && tool.name === "read_file" && !checked)
    selectedTools.value = selectedTools.value.filter(
      (t) => t.kind !== "builtin" || t.name !== "edit_file",
    );
}
async function saveDraft() {
  let model: components["schemas"]["ModelChoice"] | null = null;
  if (modelMode.value === "parent") model = { mode: "parent" };
  if (modelMode.value === "fixed") {
    const [provider_id, model_id] = JSON.parse(modelValue.value) as [
      string,
      string,
    ];
    model = { mode: "fixed", provider_id, model_id };
  }
  await editor.save(selectedKey.value, {
    model,
    instructions:
      instructionsMode.value === "inherit" ? null : instructions.value,
    tools:
      selectedKey.value === "title" || toolsMode.value === "inherit"
        ? null
        : toolsMode.value === "parent"
          ? { mode: "parent" }
          : { mode: "allowlist", items: selectedTools.value },
  });
}
async function reset() {
  await editor.save(selectedKey.value, {
    model: null,
    instructions: null,
    tools: null,
  });
}
</script>
