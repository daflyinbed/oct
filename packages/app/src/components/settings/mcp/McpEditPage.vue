<template>
  <div class="flex items-center gap-2">
    <DialogTitle class="text-xl text-strong font-semibold">
      {{ server ? "Edit MCP Server" : "Add MCP Server" }}
    </DialogTitle>
    <span
      class="rounded bg-neutral-2 px-1.5 py-px text-badge text-dim font-medium leading-[16px]"
    >
      {{ form.transport }}
    </span>
  </div>
  <p class="mt-1 text-sm text-dim">
    {{ formIntro }}
  </p>

  <div class="mt-4 max-w-[520px] space-y-4">
    <label class="block">
      <span class="text-base text-strong font-semibold"> Name </span>
      <span class="ml-1 text-sm text-danger-10">*</span>
      <div class="mt-0.5 text-sm text-dim">唯一名称，用于识别该 server。</div>
      <input
        v-model="form.name"
        placeholder="e.g. filesystem"
        class="mt-2 h-[30px] w-full control-surface px-3 text-base control-focus"
      />
    </label>
    <label class="block">
      <span class="text-base text-strong font-semibold"> Transport </span>
      <div class="mt-0.5 text-sm text-dim">
        stdio 本地子进程；http 远程 Streamable HTTP 端点。
      </div>
      <div class="mt-2">
        <AppSelect
          :model-value="form.transport"
          :options="transportOptions"
          title="transport"
          trigger-class="w-full text-neutral-10"
          @update:model-value="selectTransport"
        />
      </div>
    </label>
    <!-- stdio 专属字段：本地进程三件套 -->
    <template v-if="form.transport === 'stdio'">
      <label class="block">
        <span class="text-base text-strong font-semibold"> Command </span>
        <span class="ml-1 text-sm text-danger-10">*</span>
        <div class="mt-0.5 text-sm text-dim">启动命令（可执行文件）。</div>
        <input
          v-model="form.command"
          placeholder="e.g. npx"
          class="mt-2 h-[30px] w-full control-surface px-3 text-code font-mono control-focus"
        />
      </label>
      <label class="block">
        <span class="text-base text-strong font-semibold"> Args </span>
        <div class="mt-0.5 text-sm text-dim">命令行参数，每行一个。</div>
        <textarea
          v-model="form.args"
          rows="3"
          placeholder="-y&#10;@modelcontextprotocol/server-filesystem&#10;/tmp"
          class="mt-2 block w-full resize-y border border-neutral-5 rounded-md bg-surface px-3 py-2 text-code text-neutral-10 font-mono control-focus outline-none transition-colors placeholder:text-faint"
        />
      </label>
      <label class="block">
        <span class="text-base text-strong font-semibold"> Env </span>
        <div class="mt-0.5 text-sm text-dim">
          环境变量，每行一个 KEY=value。
        </div>
        <textarea
          v-model="form.env"
          rows="2"
          placeholder="API_TOKEN=secret"
          class="mt-2 block w-full resize-y border border-neutral-5 rounded-md bg-surface px-3 py-2 text-code text-neutral-10 font-mono control-focus outline-none transition-colors placeholder:text-faint"
        />
      </label>
    </template>
    <!-- http 专属字段：端点 + 请求头（bearer 认证入口） -->
    <template v-else>
      <label class="block">
        <span class="text-base text-strong font-semibold"> URL </span>
        <span class="ml-1 text-sm text-danger-10">*</span>
        <div class="mt-0.5 text-sm text-dim">
          Streamable HTTP 端点（http:// 或 https:// 开头）。
        </div>
        <input
          v-model="form.url"
          placeholder="https://example.com/mcp"
          class="mt-2 h-[30px] w-full control-surface px-3 text-base font-mono control-focus"
        />
      </label>
      <label class="block">
        <span class="text-base text-strong font-semibold"> Headers </span>
        <div class="mt-0.5 text-sm text-dim">
          请求头，每行一个 KEY=value；Bearer 认证写 Authorization=Bearer
          &lt;token&gt;。
        </div>
        <textarea
          v-model="form.headers"
          rows="2"
          placeholder="Authorization=Bearer sk-..."
          class="mt-2 block w-full resize-y border border-neutral-5 rounded-md bg-surface px-3 py-2 text-code text-neutral-10 font-mono control-focus outline-none transition-colors placeholder:text-faint"
        />
      </label>
      <!-- OAuth 登录区：仅已保存的 http 行（start 需要服务端行与落库的
           url；新建表单先保存再回来登录）。 -->
      <div
        v-if="server"
        class="border border-line-soft rounded-lg bg-surface p-3"
      >
        <div class="flex items-center gap-2">
          <span class="text-base text-strong font-semibold"> OAuth </span>
          <span
            v-if="oauthState?.kind === 'authorized'"
            class="rounded bg-success-7 px-1.5 py-px text-badge text-success-contrast font-medium leading-[16px]"
            :title="
              oauthState.expiresAt
                ? `有效期至 ${formatOauthExpiry(oauthState.expiresAt)}`
                : '长期有效'
            "
          >
            已授权
          </span>
          <span
            v-else-if="oauthState?.kind === 'expired'"
            class="rounded bg-danger-7 px-1.5 py-px text-badge text-danger-10 font-medium leading-[16px]"
          >
            需重新登录
          </span>
          <span
            v-else
            class="rounded bg-neutral-2 px-1.5 py-px text-badge text-dim font-medium leading-[16px]"
          >
            未授权
          </span>
        </div>
        <div class="mt-0.5 text-sm text-dim">
          远端服务器需要 OAuth 2.0 时在此登录（授权码 + PKCE，基于已保存的 URL
          发起）；已登录的 Bearer 优先于 Headers 里的静态 Authorization。
          <template v-if="oauthState?.kind === 'authorized'">
            {{
              oauthState.expiresAt
                ? `有效期至 ${formatOauthExpiry(oauthState.expiresAt)}`
                : "长期有效"
            }}
            {{ server.oauth?.has_refresh_token ? "· 支持自动刷新" : "" }}
          </template>
        </div>
        <div class="mt-2 flex items-center gap-2">
          <button
            class="h-[26px] inline-flex items-center gap-1.5 control-solid rounded-md px-3 text-sm font-medium control-focus transition-colors disabled:opacity-50"
            :disabled="oauthBusy"
            :title="
              oauthState?.kind === 'absent'
                ? '登录并授权该服务器'
                : '重新登录（更换凭据）'
            "
            @click="loginOauth"
          >
            {{
              oauthWaiting
                ? "等待授权完成…"
                : oauthState?.kind === "absent"
                  ? "登录"
                  : "重新登录"
            }}
          </button>
          <button
            v-if="oauthState && oauthState.kind !== 'absent'"
            class="h-[26px] inline-flex items-center control-ghost rounded-md px-2.5 text-sm control-focus transition-colors disabled:opacity-50"
            :disabled="oauthBusy"
            @click="logoutOauthLogin"
          >
            退出登录
          </button>
        </div>
        <p v-if="oauthError" class="mt-1.5 text-sm text-danger-10">
          {{ oauthError }}
        </p>
      </div>
    </template>
    <div class="flex items-start gap-4">
      <label class="block w-[180px] flex-none">
        <span class="text-base text-strong font-semibold"> Tool Timeout </span>
        <div class="mt-0.5 text-sm text-dim">
          工具调用超时（毫秒），留空用默认值。
        </div>
        <input
          v-model="form.toolTimeout"
          placeholder="30000"
          inputmode="numeric"
          class="mt-2 h-[30px] w-full control-surface px-3 text-base font-mono control-focus"
        />
      </label>
      <label class="mt-[1px] min-w-0 flex-1">
        <span class="mt-[11px] text-base text-strong font-semibold">
          Project Scope
        </span>
        <div class="mt-0.5 text-sm text-dim">
          全局对所有项目生效；项目内仅该项目的对话可见。
        </div>
        <div class="mt-2">
          <AppSelect
            :model-value="form.projectId"
            :options="projectScopeOptions"
            placeholder="全局"
            title="作用域"
            trigger-class="w-full text-neutral-10"
            @update:model-value="selectScope"
          />
        </div>
      </label>
    </div>
    <!-- 只显示表单自己的错误；fetch/列表级错误留在列表视图顶部，不经 ??
         泄漏进表单 -->
    <p v-if="error" class="text-sm text-danger-10">{{ error }}</p>
    <div class="flex items-center gap-2">
      <button
        class="h-[28px] control-solid rounded-md px-4 text-base font-medium control-focus transition-colors disabled:opacity-50"
        :disabled="!canSubmit || submitting"
        @click="submitForm"
      >
        {{ submitting ? "Saving…" : server ? "Save Changes" : "Create Server" }}
      </button>
      <button
        class="h-[28px] control-ghost rounded-md px-3 text-base control-focus"
        @click="emit('cancel')"
      >
        Cancel
      </button>
    </div>
  </div>
</template>

<script setup lang="ts">
import { DialogTitle } from "reka-ui";
import { computed, onBeforeUnmount, ref, watch } from "vue";
import AppSelect from "@/components/ui/AppSelect.vue";
import { useMcpServerForm } from "@/composables/useMcpServerForm";
import { mcpOauthState, useMcpServers } from "@/composables/useMcpServers";

/**
 * MCP server 新增 / 编辑表单。表单与 OAuth 等待状态都归本页所有：进入
 * 即重建、离开即销毁（disposed 守卫拦住迟到的异步回写——提交/OAuth 等待
 * 完成时页面已离开，则只丢弃，不回写、不导航）。
 */
const props = defineProps<{
  /** 编辑目标行 id；null = 新增。 */
  serverId: string | null;
  /** 作用域 select 的选项（含全局哨兵），由壳层从项目列表派生。 */
  projectScopeOptions: { value: string; label: string }[];
  /** 作用域 select 的全局哨兵值（与壳层共享的常量约定）。 */
  globalScopeValue: string;
  /** 新建表单的默认作用域（项目作用域下新增默认归属该项目）。 */
  defaultProjectId?: string | null;
}>();

const emit = defineEmits<{
  /** 提交成功：壳层回到列表（并带最新列表状态）。 */
  saved: [];
  cancel: [];
  /** 编辑目标已被他处删除：壳层退回列表。 */
  targetGone: [];
}>();

const {
  servers: mcpServers,
  error: mcpError,
  startOauthLogin: startMcpOauth,
  waitForOauth: waitForMcpOauth,
  logoutOauth: logoutMcpOauth,
} = useMcpServers();

const { form, error, submitting, canSubmit, fillFrom, submit } =
  useMcpServerForm({
    globalScopeValue: props.globalScopeValue,
    defaultProjectId: props.defaultProjectId,
  });

/** 编辑目标行（新增时为 null）；随列表刷新响应式更新。 */
const server = computed(() =>
  props.serverId
    ? (mcpServers.value.find((s) => s.id === props.serverId) ?? null)
    : null,
);

// 编辑入口：用当前行填充一次（后续行变化不覆盖用户正在编辑的表单）。
if (server.value) {
  fillFrom(server.value);
}

// 编辑目标已被删除（如他处同步）时退回列表
watch(server, (value) => {
  if (props.serverId && !value) emit("targetGone");
});

/** 迟到异步结果的丢弃闸门：离开页面后不再回写任何状态/事件。 */
let disposed = false;
onBeforeUnmount(() => {
  disposed = true;
});

const transportOptions = [
  { value: "stdio", label: "stdio（本地子进程）" },
  { value: "http", label: "http（Streamable HTTP）" },
];

const formIntro = computed(() =>
  form.value.transport === "http"
    ? "连接远程 Streamable HTTP 端点（http transport），Agent 在对话中按需调用其工具。"
    : "以本地子进程方式启动（stdio transport），Agent 在对话中按需调用其工具。",
);

function selectTransport(value: string) {
  form.value.transport = value;
}

function selectScope(value: string) {
  form.value.projectId = value;
}

async function submitForm() {
  const saved = await submit(props.serverId);
  if (saved && !disposed) emit("saved");
}

/* ---------- OAuth（http 行编辑表单）：状态徽章 / 登录 / 退出 ---------- */

/** 编辑目标的 OAuth 徽章状态（waitForOauth 会刷新列表行，徽章随之响应）。 */
const oauthState = computed(() => {
  const target = server.value;
  return target ? mcpOauthState(target) : null;
});

const oauthBusy = ref(false);
const oauthWaiting = ref(false);
const oauthError = ref<string | null>(null);

const oauthExpiryFormatter = new Intl.DateTimeFormat(undefined, {
  dateStyle: "short",
  timeStyle: "short",
});

function formatOauthExpiry(date: Date): string {
  return oauthExpiryFormatter.format(date);
}

/**
 * 登录：POST start 拿 authorization_url → 新标签页打开授权服务器 →
 * 轮询本行详情直到回调把 token 入库（徽章翻为已授权）。发起失败（发现/
 * 注册 502 等）与等待超时分开呈现。等待期间离开页面：结果丢弃。
 */
async function loginOauth() {
  const target = server.value;
  if (!target || oauthBusy.value) return;
  oauthError.value = null;
  oauthBusy.value = true;
  try {
    const authorizationUrl = await startMcpOauth(target.id);
    window.open(authorizationUrl, "_blank", "noopener,noreferrer");
    oauthWaiting.value = true;
    const ready = await waitForMcpOauth(target.id);
    if (disposed) return;
    if (!ready) {
      oauthError.value =
        "等待授权完成超时（或该 server 已被删除），可重试登录。";
    }
  } catch (error_) {
    if (!disposed) {
      oauthError.value =
        error_ instanceof Error ? error_.message : "发起 OAuth 登录失败";
    }
  } finally {
    oauthBusy.value = false;
    oauthWaiting.value = false;
  }
}

async function logoutOauthLogin() {
  const target = server.value;
  if (!target || oauthBusy.value) return;
  oauthError.value = null;
  const ok = await logoutMcpOauth(target.id);
  if (!disposed && !ok) {
    oauthError.value = mcpError.value ?? "退出 OAuth 登录失败";
  }
}
</script>
