<template>
  <section
    class="min-h-0 min-w-0 flex flex-1 flex-col bg-background"
    aria-label="终端"
  >
    <div
      class="min-h-8 flex flex-none items-center gap-3 border-b border-line-soft px-3 text-xs text-dim"
    >
      <i-lucide-terminal class="h-3.5 w-3.5" />
      <span class="min-w-0 flex-1 truncate" :title="workingDir">{{
        workingDir
      }}</span>
      <span role="status">{{ statusText }}</span>
      <button
        v-if="
          connection.status.value === 'disconnected' ||
          connection.status.value === 'error'
        "
        class="rounded px-2 py-1 hover:bg-neutral-1 hover:text-strong"
        :disabled="initializing"
        @click="reconnect"
      >
        重连
      </button>
    </div>
    <p
      v-if="initializationError || connection.error.value"
      role="alert"
      class="flex-none px-3 py-2 text-xs text-danger-10"
    >
      {{ initializationError || connection.error.value }}
    </p>
    <p
      v-if="connection.notice.value"
      class="flex-none px-3 py-1 text-xs text-dim"
    >
      {{ connection.notice.value }}
    </p>
    <div
      ref="container"
      class="terminal-container min-h-0 min-w-0 flex-1 overflow-hidden p-2"
      @pointerdown="terminal?.focus()"
    />
  </section>
</template>

<script setup lang="ts">
import { computed, nextTick, onBeforeUnmount, ref, watch } from "vue";
import { loadGhostty } from "@/components/terminal/ghostty";
import { createTerminalConnection } from "@/composables/useTerminal";
import type { FitAddon, Terminal } from "ghostty-web";

const props = defineProps<{
  projectId: string;
  sessionId: string;
  workingDir: string;
  active: boolean;
}>();

const container = ref<HTMLElement>();
const initializationError = ref("");
let terminal: Terminal | undefined;
let fit: FitAddon | undefined;
let observer: ResizeObserver | undefined;
const initializing = ref(false);
let disposed = false;
const subscriptions: { dispose: () => void }[] = [];
const connection = createTerminalConnection(props.projectId, props.sessionId, {
  write: (bytes) => renderer().write(bytes),
  reset: () => renderer().reset(),
  resize: (cols, rows) => renderer().resize(cols, rows),
});

function renderer() {
  if (!terminal) throw new Error("终端尚未初始化");
  return terminal;
}

const statusText = computed(() => {
  switch (connection.status.value) {
    case "connecting":
      return "连接中…";
    case "connected":
      return "已连接";
    case "disconnected":
      return "已断开";
    case "error":
      return "连接异常";
    case "exited":
      return `已退出${connection.exitCode.value === null ? "" : ` (${connection.exitCode.value})`}`;
  }
});

function fitTerminal() {
  if (
    !props.active ||
    !container.value?.clientWidth ||
    !container.value.clientHeight
  )
    return;
  const dimensions = fit?.proposeDimensions();
  if (!dimensions || !terminal) return;
  const cols = Math.min(500, Math.max(2, dimensions.cols));
  const rows = Math.min(300, Math.max(1, dimensions.rows));
  if (terminal.cols !== cols || terminal.rows !== rows)
    terminal.resize(cols, rows);
  connection.resize(cols, rows);
}

async function initialize() {
  if (terminal || initializing.value || disposed || !props.active) return;
  initializing.value = true;
  initializationError.value = "";
  try {
    const ghostty = await loadGhostty();
    if (disposed || !container.value) return;
    const style = getComputedStyle(container.value);
    terminal = new ghostty.Terminal({
      fontSize: 13,
      fontFamily: "'JetBrains Mono', 'Noto Sans Mono', monospace",
      cursorBlink: true,
      scrollback: 5000,
      theme: {
        background: style.getPropertyValue("--oct-color-background").trim(),
        foreground: style.getPropertyValue("--oct-color-strong").trim(),
        cursor: style.getPropertyValue("--oct-color-strong").trim(),
        selectionBackground: "#3b4563",
      },
    });
    fit = new ghostty.FitAddon();
    terminal.loadAddon(fit);
    terminal.open(container.value);
    subscriptions.push(terminal.onData(connection.write));
    observer = new ResizeObserver(fitTerminal);
    observer.observe(container.value);
    fitTerminal();
    terminal.focus();
    connection.connect();
  } catch (error) {
    disposeRenderer();
    initializationError.value =
      error instanceof Error ? error.message : "终端初始化失败";
  } finally {
    initializing.value = false;
  }
}

async function reconnect() {
  if (terminal) connection.reconnect();
  else await initialize();
}

function disposeRenderer() {
  observer?.disconnect();
  observer = undefined;
  for (const subscription of subscriptions.splice(0)) subscription.dispose();
  fit?.dispose();
  fit = undefined;
  terminal?.dispose();
  terminal = undefined;
}

watch(
  () => props.active,
  async (active) => {
    if (!active) return;
    await nextTick();
    await initialize();
    fitTerminal();
    terminal?.focus();
  },
  { immediate: true },
);

watch(connection.status, async (status) => {
  if (status === "connected") {
    await nextTick();
    fitTerminal();
    if (props.active) terminal?.focus();
  }
});

onBeforeUnmount(() => {
  disposed = true;
  connection.dispose();
  disposeRenderer();
});
</script>

<style scoped>
.terminal-container {
  /* Ghostty uses a contenteditable host for input; the canvas draws the cursor. */
  caret-color: transparent;
}
</style>
