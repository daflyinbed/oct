import { readonly, ref } from "vue";
import client from "@/api/client";

/**
 * 应用设置（模块级单例）：目前只有 LLM wire 日志开关。GET 惰性拉取一次，
 * PUT 落库 + 后端即时生效；settings 弹窗开关行消费。
 */
const loaded = ref(false);
const llmWireLog = ref(true);

export function useSettings() {
  async function load(force = false): Promise<void> {
    if (loaded.value && !force) return;
    const { data, error } = await client.GET("/api/settings");
    if (error !== undefined) return;
    llmWireLog.value = data.llm_wire_log;
    loaded.value = true;
  }

  async function setLlmWireLog(on: boolean): Promise<boolean> {
    const { data, error } = await client.PUT("/api/settings", {
      body: { llm_wire_log: on },
    });
    if (error !== undefined) return false;
    // 回显后端的实际生效值（进程被 OCT_LLM_WIRE_LOG=off 硬关时，
    // 持久化成功但生效仍为 false）。
    llmWireLog.value = data.llm_wire_log;
    loaded.value = true;
    return data.llm_wire_log === on;
  }

  return {
    llmWireLog: readonly(llmWireLog),
    load,
    setLlmWireLog,
  };
}
