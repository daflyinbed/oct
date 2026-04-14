/** Matches backend AgentEvent variants */
export type AgentEvent =
  | { type: 'text_delta'; data: string }
  | { type: 'reasoning_delta'; data: string }
  | { type: 'tool_call_start'; data: { id: string; name: string; arguments: string } }
  | { type: 'tool_result'; data: { call_id: string; content: string; is_error: boolean } }
  | { type: 'usage'; data: { input_tokens: number | null; output_tokens: number | null; reasoning_tokens: number | null } }
  | { type: 'finish'; data: null }
  | { type: 'error'; data: string }

export interface Conversation {
  id: string
  title: string
  working_dir: string
  provider_spec: string
  created_at: string
  updated_at: string
}

export interface StoredMessage {
  id: string
  conversation_id: string
  role: string
  parts_json: string
  ordering: number
  created_at: string
}

export interface AgentConfigResponse {
  provider_spec: string
  working_dir: string
}

/** Parsed content part from parts_json */
export type ContentPart =
  | { Text: string }
  | { Reasoning: string }
  | { ToolCall: { id: string; name: string; arguments: string } }
  | { ToolResult: { call_id: string; content: unknown; is_error: boolean } }

/** A UI-friendly message representation */
export interface DisplayMessage {
  id: string
  role: 'user' | 'assistant' | 'system' | 'tool'
  text: string
  reasoning?: string
  toolCalls?: { id: string; name: string; arguments: string }[]
  toolResults?: { call_id: string; content: string; is_error: boolean }[]
  createdAt: string
}

/** Active tool call in the streaming UI */
export interface ActiveToolCall {
  id: string
  name: string
  arguments: string
  result?: { content: string; is_error: boolean }
}
