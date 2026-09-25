<template>
  <footer
    class="h-[30px] flex flex-none select-none items-center gap-0.5 border-t border-line-soft bg-titlebar px-2"
  >
    <button
      class="h-[22px] w-[26px] inline-flex items-center justify-center control-ghost rounded-[5px] control-focus transition-colors"
      :class="sidebarVisible ? 'text-accent-10' : ''"
      title="切换侧栏"
      @click="$emit('toggleSidebar')"
    >
      <i-lucide-panel-left class="h-3.5 w-3.5" />
    </button>
    <button
      class="h-[22px] w-[26px] inline-flex items-center justify-center control-ghost rounded-[5px] control-focus transition-colors"
      :class="chatVisible ? 'text-accent-10' : ''"
      title="切换聊天区"
      @click="$emit('toggleChat')"
    >
      <i-lucide-message-square class="h-3.5 w-3.5" />
    </button>

    <span class="mx-1.5 h-3.5 w-px bg-line" />
    <span class="flex items-center gap-1.5 px-1 text-[11.5px] text-faint">
      <span class="text-success-10">✓</span> Ready
    </span>

    <!-- MCP 指示器：进入项目即对账建连，菜单列出当前项目有效集合的实时
         连接状态（绿=已连接、红=失败、灰=未连接），行悬停向右弹出子菜单
         （查看加载的工具 / 配置 / 状态行），打开时顺带刷新 -->
    <template v-if="activeProjectId">
      <span class="mx-1.5 h-3.5 w-px bg-line" />
      <DropdownMenuRoot @update:open="handleMenuOpen">
        <DropdownMenuTrigger
          class="relative h-[22px] w-[26px] inline-flex items-center justify-center control-ghost rounded-[5px] control-focus transition-colors"
          title="MCP 服务器状态"
        >
          <i-lucide-loader-circle
            v-if="ensuring"
            class="h-3.5 w-3.5 animate-spin"
          />
          <i-lucide-plug v-else class="h-3.5 w-3.5" />
          <span
            v-if="summaryDot"
            class="absolute right-[2px] top-[2px] h-1.5 w-1.5 rounded-full"
            :class="summaryDot"
          />
        </DropdownMenuTrigger>
        <DropdownMenuPortal>
          <DropdownMenuContent
            side="top"
            align="start"
            :side-offset="6"
            class="z-50 w-[264px] border border-neutral-5 rounded-lg bg-surface p-1 shadow-[var(--oct-shadow)]"
          >
            <p
              class="px-2 pb-1 pt-1.5 text-[10.5px] text-faint font-medium tracking-wide uppercase"
            >
              MCP Servers
            </p>
            <!-- 按作用域分组：全局在上，本项目在下（组间分隔线），空的组不渲染 -->
            <template v-for="(group, index) in serverGroups" :key="group.label">
              <DropdownMenuSeparator
                v-if="index > 0"
                class="mx-2 my-1 bg-line-soft"
              />
              <p
                class="px-2 py-0.5 text-[10.5px] text-faint font-medium tracking-wide uppercase"
              >
                {{ group.label }}
              </p>
              <DropdownMenuSub v-for="server in group.servers" :key="server.id">
                <DropdownMenuSubTrigger
                  class="flex items-center gap-2 rounded-[5px] px-2 py-1 text-[12.5px] outline-none data-[highlighted]:bg-neutral-1"
                >
                  <span
                    class="h-1.5 w-1.5 flex-none rounded-full"
                    :class="mcpStatusDot(server.status)"
                  />
                  <span class="min-w-0 flex-1 truncate">{{ server.name }}</span>
                  <span
                    class="flex-none text-[11px]"
                    :class="mcpStatusClass(server.status)"
                  >
                    {{ mcpStatusShortText(server) }}
                  </span>
                  <i-lucide-chevron-right
                    class="h-3 w-3 flex-none text-faint"
                  />
                </DropdownMenuSubTrigger>
                <DropdownMenuSubContent
                  side="right"
                  align="start"
                  :side-offset="6"
                  class="z-50 w-[190px] border border-neutral-5 rounded-lg bg-surface p-1 shadow-[var(--oct-shadow)]"
                >
                  <!-- 首项按状态切换：失败 → 打开错误日志；其余 → 工具清单。
                       两者都打开该 server 自己的面板 tab -->
                  <DropdownMenuItem
                    v-if="server.status === 'failed'"
                    class="h-[26px] flex cursor-pointer items-center gap-2 rounded-[5px] px-2 text-[12.5px] text-neutral-10 outline-none data-[highlighted]:bg-neutral-1"
                    @select="openServerPanel(server)"
                  >
                    <i-lucide-scroll-text class="h-3.5 w-3.5 text-dim" />
                    打开错误日志
                  </DropdownMenuItem>
                  <DropdownMenuItem
                    v-else
                    class="h-[26px] flex cursor-pointer items-center gap-2 rounded-[5px] px-2 text-[12.5px] text-neutral-10 outline-none data-[highlighted]:bg-neutral-1"
                    @select="openServerPanel(server)"
                  >
                    <i-lucide-wrench class="h-3.5 w-3.5 text-dim" />
                    查看加载的工具
                  </DropdownMenuItem>
                  <DropdownMenuItem
                    class="h-[26px] flex cursor-pointer items-center gap-2 rounded-[5px] px-2 text-[12.5px] text-neutral-10 outline-none data-[highlighted]:bg-neutral-1"
                    @select="emit('configureMcpServer', server.id)"
                  >
                    <i-lucide-settings class="h-3.5 w-3.5 text-dim" />
                    配置…
                  </DropdownMenuItem>
                  <DropdownMenuSeparator class="mx-1 my-1 bg-line-soft" />
                  <!-- 状态行（只读）：对齐 Zed 的 Running — 12.4 MB 尾行 -->
                  <div
                    class="flex items-center gap-2 px-2 py-1 text-[11.5px]"
                    :title="mcpStatusText(server)"
                  >
                    <span
                      class="h-1.5 w-1.5 flex-none rounded-full"
                      :class="mcpStatusDot(server.status)"
                    />
                    <span
                      class="min-w-0 flex-1 truncate"
                      :class="mcpStatusClass(server.status)"
                      >{{ mcpStatusText(server) }}</span
                    >
                  </div>
                </DropdownMenuSubContent>
              </DropdownMenuSub>
            </template>
            <p
              v-if="servers.length === 0"
              class="px-2 py-2 text-[12px] text-faint"
            >
              当前项目没有启用的 MCP server。
            </p>
          </DropdownMenuContent>
        </DropdownMenuPortal>
      </DropdownMenuRoot>
    </template>

    <!-- 子代理指示器：当前根会话的任务集合（无任务即隐藏）。运行中显示
         脉冲点与运行计数；存在 lost（且无运行中）显示「待恢复」；全部
         结算后转为安静的完成态但入口保留。点击向上展开任务列表，行点击
         打开子代理 panel（与 TaskCard 同一个 openSubagentPanel），运行行
         提供独立停止（click.stop，不触发打开） -->
    <template v-if="subagentTasks.length > 0">
      <span class="mx-1.5 h-3.5 w-px bg-line" />
      <DropdownMenuRoot @update:open="handleSubagentMenuOpen">
        <DropdownMenuTrigger
          class="relative h-[22px] inline-flex items-center gap-1 control-ghost rounded-[5px] px-1 control-focus transition-colors"
          title="子代理任务"
        >
          <i-lucide-bot class="h-3.5 w-3.5" />
          <span class="text-[11.5px]">子代理 {{ runningTasks.length }}</span>
          <!-- 运行中：脉冲点（demo .pulse 的不透明度脉冲） -->
          <span
            v-if="runningTasks.length > 0"
            class="h-1.5 w-1.5 animate-pulse rounded-full bg-accent-7"
          />
          <!-- 待恢复：crash 后的 lost 任务，不能误显示为运行/完成 -->
          <span
            v-else-if="hasLostTasks"
            class="h-[15px] inline-flex items-center whitespace-nowrap rounded-full bg-warning-7 px-1.5 text-[10px] text-warning-10 font-medium leading-none"
            >待恢复</span
          >
          <!-- 全部结算：安静完成态，入口保留 -->
          <span v-else class="text-[11.5px] text-success-10">✓</span>
        </DropdownMenuTrigger>
        <DropdownMenuPortal>
          <DropdownMenuContent
            side="top"
            align="start"
            :side-offset="6"
            class="z-50 w-[300px] border border-neutral-5 rounded-lg bg-surface p-1 shadow-[var(--oct-shadow)]"
          >
            <p
              class="px-2 pb-1 pt-1.5 text-[10.5px] text-faint font-medium tracking-wide uppercase"
            >
              子代理任务
            </p>
            <DropdownMenuItem
              v-for="task in subagentTasks"
              :key="task.run_id"
              class="flex cursor-pointer items-center gap-2 rounded-[5px] px-2 py-1 text-[12.5px] outline-none data-[highlighted]:bg-neutral-1"
              @select="openTask(task)"
            >
              <span
                class="h-1.5 w-1.5 flex-none rounded-full"
                :class="subagentStatusDot(task.status)"
              />
              <span class="min-w-0 flex-1">
                <span class="block truncate text-neutral-10">{{
                  task.title || task.description
                }}</span>
                <span
                  v-if="task.activity && task.status === 'running'"
                  class="block truncate text-[11px] text-faint"
                  >{{ task.activity }}</span
                >
              </span>
              <span class="flex-none text-[11px] text-faint tabular-nums">{{
                taskElapsed(task)
              }}</span>
              <span
                v-if="task.status === 'running'"
                class="h-[20px] w-[20px] inline-flex flex-none items-center justify-center rounded-[5px] text-dim transition-colors hover:bg-danger-1 hover:text-danger-10"
                title="停止此子任务"
                @click.stop="stopTask(task)"
              >
                <i-lucide-square class="h-3 w-3 fill-current" />
              </span>
            </DropdownMenuItem>
          </DropdownMenuContent>
        </DropdownMenuPortal>
      </DropdownMenuRoot>
    </template>

    <!-- 后台任务指示器：当前根会话的后台命令集合（无任务即隐藏），照抄
         子代理指示器。运行中显示脉冲点与运行计数；存在 lost（且无运行中）
         显示「已中断」；全部结算后转为安静完成态但入口保留。行点击定位
         消息流里的信封卡片，运行行提供独立停止（click.stop） -->
    <template v-if="backgroundTasks.length > 0">
      <span class="mx-1.5 h-3.5 w-px bg-line" />
      <DropdownMenuRoot @update:open="handleBackgroundMenuOpen">
        <DropdownMenuTrigger
          class="relative h-[22px] inline-flex items-center gap-1 control-ghost rounded-[5px] px-1 control-focus transition-colors"
          title="后台任务"
        >
          <i-lucide-terminal class="h-3.5 w-3.5" />
          <span class="text-[11.5px]"
            >后台任务 {{ runningBackgroundTasks.length }}</span
          >
          <span
            v-if="runningBackgroundTasks.length > 0"
            class="h-1.5 w-1.5 animate-pulse rounded-full bg-accent-7"
          />
          <!-- 已中断：后端重启击杀的后台命令 -->
          <span
            v-else-if="hasInterruptedBackgroundTasks"
            class="h-[15px] inline-flex items-center whitespace-nowrap rounded-full bg-warning-7 px-1.5 text-[10px] text-warning-10 font-medium leading-none"
            >已中断</span
          >
          <!-- 全部结算：安静完成态，入口保留 -->
          <span v-else class="text-[11.5px] text-success-10">✓</span>
        </DropdownMenuTrigger>
        <DropdownMenuPortal>
          <DropdownMenuContent
            side="top"
            align="start"
            :side-offset="6"
            class="z-50 w-[340px] border border-neutral-5 rounded-lg bg-surface p-1 shadow-[var(--oct-shadow)]"
          >
            <p
              class="px-2 pb-1 pt-1.5 text-[10.5px] text-faint font-medium tracking-wide uppercase"
            >
              后台任务
            </p>
            <DropdownMenuItem
              v-for="task in backgroundTasks"
              :key="task.id"
              class="flex cursor-pointer items-center gap-2 rounded-[5px] px-2 py-1 text-[12.5px] outline-none data-[highlighted]:bg-neutral-1"
              @select="locateBackgroundTask(task)"
            >
              <span
                class="h-1.5 w-1.5 flex-none rounded-full"
                :class="backgroundStatusDot(task.status)"
              />
              <span class="min-w-0 flex-1">
                <span
                  class="block truncate text-[11.5px] text-neutral-10 font-mono"
                  :title="task.command"
                  >{{ task.command }}</span
                >
              </span>
              <span
                v-if="
                  task.status === 'completed' &&
                  task.exit_code != null &&
                  task.exit_code !== 0
                "
                class="flex-none text-[11px] text-danger-10"
                >exit {{ task.exit_code }}</span
              >
              <span class="flex-none text-[11px] text-faint tabular-nums">{{
                backgroundTaskElapsed(task)
              }}</span>
              <span
                v-if="!isBackgroundTaskTerminal(task.status)"
                class="h-[20px] w-[20px] inline-flex flex-none items-center justify-center rounded-[5px] text-dim transition-colors hover:bg-danger-1 hover:text-danger-10"
                title="停止此后台任务"
                @click.stop="stopBackgroundTask(task)"
              >
                <i-lucide-square class="h-3 w-3 fill-current" />
              </span>
            </DropdownMenuItem>
          </DropdownMenuContent>
        </DropdownMenuPortal>
      </DropdownMenuRoot>
    </template>

    <div class="flex-1" />

    <button
      class="h-[22px] w-[26px] inline-flex items-center justify-center control-ghost rounded-[5px] control-focus transition-colors"
      :title="isDark ? '切换亮色主题' : '切换暗色主题'"
      @click="toggleTheme"
    >
      <i-lucide-sun v-if="isDark" class="h-3.5 w-3.5" />
      <i-lucide-moon v-else class="h-3.5 w-3.5" />
    </button>

    <span class="mx-1.5 h-3.5 w-px bg-line" />
    <button
      class="h-[22px] w-[26px] inline-flex items-center justify-center control-ghost rounded-[5px] control-focus transition-colors"
      :class="diffVisible ? 'text-accent-10' : ''"
      title="Changes 面板"
      @click="$emit('toggleDiff')"
    >
      <i-lucide-git-compare class="h-3.5 w-3.5" />
    </button>
    <button
      class="h-[22px] w-[26px] inline-flex items-center justify-center control-ghost rounded-[5px] control-focus transition-colors"
      :class="fileTreeVisible ? 'text-accent-10' : ''"
      title="Files 面板"
      @click="$emit('toggleFileTree')"
    >
      <i-lucide-files class="h-3.5 w-3.5" />
    </button>
  </footer>
</template>

<script setup lang="ts">
import { useLocalStorage, useNow } from "@vueuse/core";
import {
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuPortal,
  DropdownMenuRoot,
  DropdownMenuSeparator,
  DropdownMenuSub,
  DropdownMenuSubContent,
  DropdownMenuSubTrigger,
  DropdownMenuTrigger,
} from "reka-ui";
import { computed, watch, watchEffect } from "vue";
import {
  backgroundStatusDot,
  cancelBackgroundTask,
  highlightBackgroundTask,
  isBackgroundTaskTerminal,
  refreshBackgroundTasks,
  useBackgroundTasks,
} from "@/composables/useBackgroundTasks";
import {
  groupMcpServersByScope,
  mcpStatusClass,
  mcpStatusDot,
  mcpStatusShortText,
  mcpStatusText,
  useMcpStatus,
} from "@/composables/useMcpStatus";
import { useProjects } from "@/composables/useProjects";
import {
  cancelSubagent,
  formatSubagentElapsed,
  openSubagentPanel,
  refreshSubagents,
  subagentStatusDot,
  useSubagents,
} from "@/composables/useSubagents";
import { useTabs } from "@/composables/useTabs";
import type { components } from "@/api/schema";
import type { BackgroundTask } from "@/composables/useBackgroundTasks";
import type { SubagentTask } from "@/composables/useSubagents";

type McpServerResponse = components["schemas"]["McpServerResponse"];

const props = defineProps<{
  sidebarVisible: boolean;
  chatVisible: boolean;
  diffVisible: boolean;
  fileTreeVisible: boolean;
  activeProjectId: string | null;
  /** 当前根会话 id：指示器展示它的子任务集合（null/无任务时隐藏）。 */
  rootConversationId: string | null;
}>();

const emit = defineEmits<{
  toggleSidebar: [];
  toggleChat: [];
  toggleDiff: [];
  toggleFileTree: [];
  /** 子菜单「配置…」：请宿主打开设置弹窗并定位到该 server 的编辑表单 */
  configureMcpServer: [serverId: string];
}>();

const { servers, ensuring, ensureServers } = useMcpStatus();
const { projects } = useProjects();
const { openTab } = useTabs();

// 进入项目（选中其任一会话）即对账建连：连接在后台完成，指示器在发消息
// 前就能反映各 server 的连接状态，首轮消息也免去了建连延迟。菜单打开
// 时再 ensure 一次，吸收设置页的增删改。
watch(
  () => props.activeProjectId,
  (projectId) => {
    if (projectId) void ensureServers(projectId);
  },
  { immediate: true },
);

/** 有效集合按作用域分组（全局在上、项目专属在下），空组剔除。 */
const serverGroups = computed(() => {
  const projectName =
    projects.value.find((project) => project.id === props.activeProjectId)
      ?.name ?? null;
  return groupMcpServersByScope(servers.value, projectName);
});

function handleMenuOpen(open: boolean) {
  if (open && props.activeProjectId) void ensureServers(props.activeProjectId);
}

// --- 子代理任务指示器 --------------------------------------------------------

const { subagentsFor } = useSubagents();
// 作用域跟随当前根会话（非聊天标签也沿用所选根会话；切换会话时 store
// 换集合，指示器跟着收敛，不混入其他会话/项目的任务）。
const subagentTasks = subagentsFor(() => props.rootConversationId);
const runningTasks = computed(() =>
  subagentTasks.value.filter((task) => task.status === "running"),
);
const hasLostTasks = computed(() =>
  subagentTasks.value.some((task) => task.status === "lost"),
);

// 运行中的耗时需要每秒走表（useNow 随组件作用域停止；BottomBar 常驻）
const now = useNow({ interval: 1000 });

function taskElapsed(task: SubagentTask): string {
  return formatSubagentElapsed(
    task.created_at,
    task.finished_at ?? null,
    now.value.valueOf(),
  );
}

/** 打开菜单时对账一次快照（吸收别处启动/结算的任务）。 */
function handleSubagentMenuOpen(open: boolean) {
  if (open && props.rootConversationId) {
    void refreshSubagents(props.rootConversationId);
  }
}

function openTask(task: SubagentTask) {
  openSubagentPanel(task.agent_id, task.description, props.rootConversationId);
}

function stopTask(task: SubagentTask) {
  void cancelSubagent(task.agent_id); // click.stop：只停止，不打开 panel
}

// --- 后台任务指示器 ----------------------------------------------------------

const { backgroundTasksFor } = useBackgroundTasks();
// 作用域跟随当前根会话（与子代理指示器一致：切换会话时 store 换集合）。
const backgroundTasks = backgroundTasksFor(() => props.rootConversationId);
const runningBackgroundTasks = computed(() =>
  backgroundTasks.value.filter(
    (task) => !isBackgroundTaskTerminal(task.status),
  ),
);
const hasInterruptedBackgroundTasks = computed(() =>
  backgroundTasks.value.some((task) => task.status === "lost"),
);

function backgroundTaskElapsed(task: BackgroundTask): string {
  return formatSubagentElapsed(
    task.created_at,
    task.finished_at ?? null,
    now.value.valueOf(),
  );
}

/** 打开菜单时对账一次快照（吸收别处启动/结算的任务）。 */
function handleBackgroundMenuOpen(open: boolean) {
  if (open && props.rootConversationId) {
    void refreshBackgroundTasks(props.rootConversationId);
  }
}

/** 定位：store 高亮（BashTaskCard 切 accent 边框）+ 平滑滚动到卡片。 */
function locateBackgroundTask(task: BackgroundTask) {
  highlightBackgroundTask(task.id);
  document
    .querySelector(`[data-bg-task-id="${CSS.escape(task.id)}"]`)
    ?.scrollIntoView({ behavior: "smooth", block: "center" });
}

function stopBackgroundTask(task: BackgroundTask) {
  void cancelBackgroundTask(task.id); // click.stop：只停止，不触发定位
}

/** 每个 server 一个独立面板 tab（失败时同一面板呈现错误日志）。 */
function openServerPanel(server: McpServerResponse) {
  openTab({
    id: `mcp:${server.id}`,
    kind: "mcp",
    payload: server.id,
    title: server.name,
  });
}

/** 图标角落的汇总点：任一失败即红，全部已连接才绿，其余灰。 */
const summaryDot = computed(() => {
  if (servers.value.length === 0) return null;
  if (servers.value.some((server) => server.status === "failed")) {
    return "bg-danger-10";
  }
  if (servers.value.every((server) => server.status === "connected")) {
    return "bg-success-10";
  }
  return "bg-neutral-5";
});

// 主题持久化到 localStorage，index.html 里有首帧前的恢复脚本
const theme = useLocalStorage<"light" | "dark">("oct-theme", "light");
const isDark = computed(() => theme.value === "dark");

watchEffect(() => {
  document.documentElement.dataset.theme = theme.value;
});

function toggleTheme() {
  theme.value = theme.value === "dark" ? "light" : "dark";
}
</script>
