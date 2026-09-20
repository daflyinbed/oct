import type { components } from "@/api/schema";

// 全部 fixture 显式对齐 src/api/schema.d.ts 生成的类型:后端改字段时
// 这里编译期报错,而不是默默漂移。

type Project = components["schemas"]["Project"];
type Conversation = components["schemas"]["Conversation"];
type ProviderResponse = components["schemas"]["ProviderResponse"];
type ModelSummary = components["schemas"]["ModelSummary"];
type StoredMessage = components["schemas"]["StoredMessage"];
type AgentEvent = components["schemas"]["AgentEvent"];
type OutputStream = components["schemas"]["OutputStream"];

const T0 = "2026-09-20T00:00:00";

export function projectFixture(id = "p1", name = "demo"): Project {
  return {
    id,
    name,
    working_dir: "/tmp/demo",
    created_at: T0,
    updated_at: T0,
  };
}

export function conversationFixture(
  id = "c1",
  projectId = "p1",
  title: string | null = null,
): Conversation {
  return {
    id,
    project_id: projectId,
    title,
    created_at: T0,
    updated_at: T0,
  };
}

function modelSummaryFixture(modelId = "model-a"): ModelSummary {
  return {
    model_id: modelId,
    name: `Model ${modelId}`,
    family: null,
    reasoning: false,
    tool_call: true,
    attachment: false,
    is_enabled: true,
    source: "custom",
  };
}

/** api_key_set=true 的 provider:前端会自动选中它,model 变为可发状态。 */
export function providerFixture(id = "mock-prov"): ProviderResponse {
  return {
    id,
    name: "Mock Provider",
    adapter_type: "openai",
    base_url: "http://mock.invalid",
    api_key_set: true,
    models: [modelSummaryFixture()],
    created_at: T0,
    updated_at: T0,
    source: "custom",
  };
}

export function storedMessage(
  role: string,
  partsJson: string,
  extra: Partial<StoredMessage> = {},
): StoredMessage {
  return {
    id: `msg-${role}-${Math.random().toString(36).slice(2, 8)}`,
    conversation_id: "c1",
    role,
    parts_json: partsJson,
    details_json: null,
    ordering: 0,
    provider_id: null,
    model_id: null,
    input_tokens: null,
    output_tokens: null,
    reasoning_tokens: null,
    created_at: T0,
    ...extra,
  };
}

// --- AgentEvent 构造器(线格式:serde tag="type" content="data") ----------

export function textDelta(data: string): AgentEvent {
  return {
    type: "text_delta",
    data,
  };
}

export function toolCallDelta(
  callId: string,
  argumentsDelta: string,
  name: string | null = null,
): AgentEvent {
  return {
    type: "tool_call_delta",
    data: { call_id: callId, name, arguments_delta: argumentsDelta },
  };
}

export function toolCallStart(
  id: string,
  name: string,
  title: string,
  args: string,
): AgentEvent {
  return {
    type: "tool_call_start",
    data: { id, name, title, arguments: args },
  };
}

export function toolOutputDelta(
  callId: string,
  stream: OutputStream,
  delta: string,
): AgentEvent {
  return {
    type: "tool_output_delta",
    data: { call_id: callId, stream, delta },
  };
}

export function toolResult(
  callId: string,
  content: string,
  details: Record<string, unknown> | null = null,
  isError = false,
): AgentEvent {
  return {
    type: "tool_result",
    data: {
      call_id: callId,
      content,
      details,
      is_error: isError,
    },
  } as AgentEvent;
}

export const finishEvent = (): AgentEvent => ({ type: "finish" });

export const cancelledEvent = (): AgentEvent => ({ type: "cancelled" });
