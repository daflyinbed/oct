import { readonly, ref } from "vue";
import client from "@/api/client";

/**
 * 应用设置（模块级单例）：LLM wire 日志和工具调用方式。GET 惰性拉取一次，
 * PUT 落库 + 后端即时生效；settings 弹窗开关行消费。
 */
const loaded = ref(false);
const llmWireLog = ref(true);
const mcpToolMode = ref<"direct" | "code">("code");
const codeModeAvailable = ref(false);

function apply(data: {
  llm_wire_log: boolean;
  mcp_tool_mode: "direct" | "code";
  code_mode_available: boolean;
}) {
  llmWireLog.value = data.llm_wire_log;
  mcpToolMode.value = data.mcp_tool_mode;
  codeModeAvailable.value = data.code_mode_available;
  loaded.value = true;
}

export function useSettings() {
  async function load(force = false): Promise<void> {
    if (loaded.value && !force) return;
    const { data, error } = await client.GET("/api/settings");
    if (error !== undefined) return;
    apply(data);
  }

  async function setLlmWireLog(on: boolean): Promise<boolean> {
    const { data, error } = await client.PUT("/api/settings", {
      body: { llm_wire_log: on },
    });
    if (error !== undefined) return false;
    // 回显后端的实际生效值（进程被 OCT_LLM_WIRE_LOG=off 硬关时，
    // 持久化成功但生效仍为 false）。
    apply(data);
    return data.llm_wire_log === on;
  }

  async function setMcpToolMode(mode: "direct" | "code"): Promise<boolean> {
    const { data, error } = await client.PUT("/api/settings", {
      body: { mcp_tool_mode: mode },
    });
    if (error !== undefined) return false;
    apply(data);
    return data.mcp_tool_mode === mode;
  }

  return {
    llmWireLog: readonly(llmWireLog),
    mcpToolMode: readonly(mcpToolMode),
    codeModeAvailable: readonly(codeModeAvailable),
    load,
    setLlmWireLog,
    setMcpToolMode,
  };
}
