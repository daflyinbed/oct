/**
 * 拦截层的"后端":一个剧本化 mock HTTP 服务器(node:http,零依赖)。
 *
 * vitest browser mode 的测试跑在浏览器页面内,拿不到 Playwright 的
 * `page.route`;wire 级拦截的等价做法是把该服务器挂在 Vite dev server
 * 的 `/api` 代理后面(与 dev/preview 同一条代理路径),让浏览器发起
 * 真实 fetch → 真实 HTTP → 代理 → mock。SSE 分块边界由 `slices`/`cut`
 * 精确控制(可在多字节 UTF-8、`data:` 行、JSON 载荷中间切开),流中
 * 暂停用命名 gate(测试经 `/__mock/gates/:name/open` 放行),取代
 * sleep 类同步等待。
 *
 * 管理 API(`/__mock/*`,同样走代理,与页面同源):
 * - `POST /__mock/reset`            清空状态、放行/关闭所有 gate、掐断所有打开的 SSE
 * - `PUT  /__mock/scenario`         整体替换状态与剧本(见 Scenario)
 * - `POST /__mock/gates/:name/open` 放行一个 gate(幂等)
 * - `GET  /__mock/requests`         请求日志 [{method, path, body}]
 */

import { Buffer } from "node:buffer";
import http from "node:http";
import type { components } from "../../src/api/schema";
import type { AddressInfo, IncomingMessage, ServerResponse } from "node:http";

type MessageDto = components["schemas"]["MessageDto"];

// ---------------------------------------------------------------------------
// 剧本类型(浏览器侧测试仅 type-import 本文件,不会引入 node:http)
// ---------------------------------------------------------------------------

/** 把一段 SSE 文本整体编码为 UTF-8 后按 `slices` 等分切开写入(边界会自然
 * 落在多字节字符/JSON 中间);`cut` 显式给字节偏移,二者给其一即可。 */
export interface StepEvents {
  type: "events";
  events: unknown[];
  slices?: number;
  cut?: number[];
}

export interface StepRaw {
  type: "raw";
  text: string;
  slices?: number;
  cut?: number[];
}

/** 写一行 SSE 注释(axum KeepAlive 的形状),前端解析器必须跳过。 */
export interface StepComment {
  type: "comment";
}

/** 暂停流,直到对应 gate 被 open(测试的确定性同步原语)。 */
export interface StepGate {
  type: "gate";
  name: string;
}

/** 以该状态码直接返回 JSON 错误(不发 SSE),需为剧本第一步。 */
export interface StepStatus {
  type: "status";
  code: number;
}

/** 写完前面的步骤后挂住流不结束(取消场景:只能经 cancel 终结)。 */
export interface StepHang {
  type: "hang";
}

export type ScriptStep =
  StepEvents | StepRaw | StepComment | StepGate | StepStatus | StepHang;

export interface Scenario {
  settings?: components["schemas"]["AppSettings"];
  projectMcpModes?: Record<string, "direct" | "code" | null>;
  git?: {
    status: components["schemas"]["GitStatus"];
    history?: components["schemas"]["GitEntry"][];
    stashes?: components["schemas"]["GitEntry"][];
    patch?: string;
    actionError?: string;
  };
  /** GET /api/projects 的返回体(形状见 schema.d.ts 的 Project)。 */
  projects?: unknown[];
  /** 全部会话(按 project_id 过滤返回)。 */
  conversations?: unknown[];
  /** GET /api/conversations/:id/messages 的返回体(MessageDto[])。 */
  messages?: Record<string, unknown[]>;
  /** 按序控制历史请求的延迟或失败；队列耗尽后恢复正常历史响应。 */
  historyReplies?: Record<string, Array<{ gate?: string; status?: number }>>;
  /** Versioned history / fork replies are explicit scripts, not a DB emulator. */
  contextRevisions?: Record<string, number>;
  /** Explicit resumability, independent of whether a scripted stream is open. */
  resumable?: Record<string, boolean>;
  /** Persisted terminal snapshots installed before scripted SSE EOF. */
  afterStreamHistory?: Record<
    string,
    { messages: MessageDto[]; revision: number; resumable?: boolean }
  >;
  revertReplies?: Record<
    string,
    {
      status?: number;
      code?: string;
      error?: string;
      response?: components["schemas"]["RevertResponse"];
      messages?: MessageDto[];
      backupMessages?: MessageDto[];
    }
  >;
  forkReplies?: Record<
    string,
    {
      status?: number;
      code?: string;
      error?: string;
      response?: components["schemas"]["ForkResponse"];
      messages?: MessageDto[];
    }
  >;
  /** GET /api/providers 的返回体(ProviderResponse[])。 */
  providers?: unknown[];
  /** Agent editor read snapshots and explicit save replies, keyed by global/project id. */
  agentSettings?: Record<string, components["schemas"]["AgentSettingsView"][]>;
  agentTools?: components["schemas"]["AgentToolOption"][];
  agentPatchReplies?: Record<
    string,
    components["schemas"]["AgentSettingsView"] | { error: string }
  >;
  /** MCP server 初始行(McpServerResponse[]);POST/PATCH/DELETE 就地增删改。 */
  mcpServers?: unknown[];
  /**
   * oauth start 剧本:serverId → 非默认结果。缺省走成功路径(发放
   * authorization_url 指回 mock 的 callback);`status` 非 2xx 时连同
   * `error` 直接返回(如 502 发现失败/400 非 http 行)。
   */
  mcpOauthStarts?: Record<string, { status?: number; error?: string }>;
  /**
   * prompts 渲染剧本:serverId → prompt name → 结果。`status` 缺省 200,
   * `messages` 即响应体;`status` 非 2xx 时连同 `error` 直接返回。
   */
  mcpPromptRenders?: Record<
    string,
    Record<string, { status?: number; messages?: unknown[]; error?: string }>
  >;
  /** resources 读取剧本:serverId → uri → 结果(status 缺省 200)。 */
  mcpResourceReads?: Record<
    string,
    Record<
      string,
      {
        status?: number;
        mime_type?: string | null;
        text?: string | null;
        blob_base64?: string | null;
        error?: string;
      }
    >
  >;
  /** POST 建会话时按序发放的确定性 id(耗尽后回退 c-auto-N)。 */
  newConversationIds?: string[];
  /** 每个会话的 SSE 剧本,POST 消息时惰性取用。 */
  scripts?: Record<string, ScriptStep[]>;
  approvals?: Record<string, components["schemas"]["PendingApprovalDto"][]>;
  subagents?: Record<string, components["schemas"]["SubagentTaskDto"][]>;
  /** Decide opens a scripted gate; the next event supplies its resolution. */
  approvalGates?: Record<string, string>;
}

export interface CapturedRequest {
  method: string;
  path: string;
  body: unknown;
}

/** mock 内存态的 MCP server 行,形状对齐 schema.d.ts 的 McpServerResponse。 */
interface McpRow {
  id: string;
  name: string;
  transport: string;
  command: string | null;
  args: string[] | null;
  env: Record<string, string> | null;
  url: string | null;
  headers: Record<string, string> | null;
  enabled: boolean;
  tool_timeout_ms: number | null;
  project_id: string | null;
  created_at: string;
  updated_at: string;
  status: string;
  tool_count: number | null;
  error: string | null;
  tools: { name: string; description: string | null }[] | null;
  prompts:
    | {
        name: string;
        description: string | null;
        arguments: {
          name: string;
          description: string | null;
          required: boolean;
        }[];
      }[]
    | null;
  resources:
    | {
        uri: string;
        name: string;
        description: string | null;
        mime_type: string | null;
      }[]
    | null;
  /** OAuth 凭据概要(形状对齐 McpServerOauthSummary;行创建即兜底补全)。 */
  oauth: {
    has_token: boolean;
    has_refresh_token: boolean;
    expires_at: string | null;
  };
}

/** 未授权形状的 oauth 概要。 */
function absentOauth(): McpRow["oauth"] {
  return { has_token: false, has_refresh_token: false, expires_at: null };
}

// ---------------------------------------------------------------------------
// 状态
// ---------------------------------------------------------------------------

interface OpenStream {
  res: ServerResponse;
  closed: boolean;
}

const state: {
  scenario: Scenario;
  autoId: number;
  requests: CapturedRequest[];
  /** 每个会话当前打开的 SSE 响应(cancel 时向其写入 cancelled 并结束)。 */
  streams: Map<string, Set<OpenStream>>;
  gates: Map<string, Array<() => void>>;
  openedGates: Set<string>;
  /** oauth start 发放的 pending flow:state → serverId(单次使用)。 */
  oauthFlows: Map<string, string>;
} = {
  scenario: {},
  autoId: 0,
  requests: [],
  streams: new Map(),
  gates: new Map(),
  openedGates: new Set(),
  oauthFlows: new Map(),
};

/** 相邻两次 socket 写入的间隔:促使 TCP 分段,字节边界才会真实生效。 */
const WRITE_GAP_MS = 4;

const sleep = (ms: number) => new Promise((resolve) => setTimeout(resolve, ms));

// --- 路由匹配(模块级正则) ---------------------------------------------------

const RE_PROJECT_CONVERSATIONS = /^\/api\/projects\/([^/]+)\/conversations$/;
const RE_CONVERSATION_DELETE =
  /^\/api\/projects\/([^/]+)\/conversations\/([^/]+)$/;
const RE_CONVERSATION_MESSAGES = /^\/api\/conversations\/([^/]+)\/messages$/;
const RE_CONVERSATION_REVERT = /^\/api\/conversations\/([^/]+)\/revert$/;
const RE_CONVERSATION_FORK = /^\/api\/conversations\/([^/]+)\/fork$/;
const RE_CONVERSATION_CANCEL = /^\/api\/conversations\/([^/]+)\/cancel$/;
const RE_CONVERSATION_RESUME = /^\/api\/conversations\/([^/]+)\/resume$/;
const RE_CONVERSATION_COMPACT = /^\/api\/conversations\/([^/]+)\/compact$/;
const RE_CONVERSATION_RUN = /^\/api\/conversations\/([^/]+)\/run$/;
const RE_APPROVALS = /^\/api\/conversations\/([^/]+)\/approvals$/;
const RE_SUBAGENTS = /^\/api\/conversations\/([^/]+)\/subagents$/;
const RE_APPROVAL_DECIDE =
  /^\/api\/conversations\/([^/]+)\/approvals\/([^/]+)$/;
const RE_MCP_SERVER = /^\/api\/mcp\/servers\/([^/]+)$/;
const RE_MCP_ENSURE = /^\/api\/projects\/([^/]+)\/mcp\/ensure$/;
const RE_MCP_PROMPT_RENDER = /^\/api\/mcp\/servers\/([^/]+)\/prompts\/([^/]+)$/;
const RE_MCP_RESOURCE_READ = /^\/api\/mcp\/servers\/([^/]+)\/resources\/read$/;
const RE_MCP_OAUTH_START = /^\/api\/mcp\/servers\/([^/]+)\/oauth\/start$/;
const RE_MCP_OAUTH_DELETE = /^\/api\/mcp\/servers\/([^/]+)\/oauth$/;
const RE_GATE_OPEN = /^\/__mock\/gates\/([^/]+)\/open$/;
const RE_AGENT_SETTINGS =
  /^\/api\/(?:projects\/([^/]+)\/)?settings\/agents(?:\/([^/]+))?$/;
const RE_AGENT_TOOLS = /^\/api\/(?:projects\/[^/]+\/)?settings\/agent-tools$/;
const RE_MCP_SETTINGS = /^\/api\/projects\/([^/]+)\/settings\/mcp$/;

function resetState() {
  state.scenario = {};
  state.requests = [];
  // 放行所有等待中的 gate,让挂起的剧本走到尽头;响应已关闭,写入是安全的。
  for (const waiters of state.gates.values()) for (const w of waiters) w();
  state.gates.clear();
  state.openedGates.clear();
  for (const streams of state.streams.values()) {
    for (const s of streams) if (!s.closed) s.res.end();
  }
  state.streams.clear();
  state.oauthFlows.clear();
}

function gateWait(name: string): Promise<void> {
  if (state.openedGates.has(name)) return Promise.resolve();
  return new Promise((resolve) => {
    const waiters = state.gates.get(name) ?? [];
    waiters.push(resolve);
    state.gates.set(name, waiters);
  });
}

function openGate(name: string) {
  state.openedGates.add(name);
  const waiters = state.gates.get(name);
  if (waiters) {
    for (const w of waiters) w();
    state.gates.delete(name);
  }
}

// ---------------------------------------------------------------------------
// SSE 写入
// ---------------------------------------------------------------------------

function sseFrames(events: unknown[]): string {
  return events.map((e) => `data: ${JSON.stringify(e)}\n\n`).join("");
}

function cutPoints(buf: Buffer, step: { slices?: number; cut?: number[] }) {
  let cuts: number[];
  if (step.slices && step.slices >= 2) {
    cuts = Array.from({ length: step.slices - 1 }, (_, i) =>
      Math.round((buf.length * (i + 1)) / step.slices),
    );
  } else {
    cuts = step.cut ?? [];
  }
  // 注意:不能用 `new Set(...).toSorted(...)`(Set 无此方法,eslint 的
  // 自动修复曾错误改写成该形式导致运行时 TypeError)
  const points = [
    ...new Set(
      cuts.filter((c) => Number.isInteger(c) && c > 0 && c < buf.length),
    ),
  ];
  points.sort((a, b) => a - b);
  return points;
}

async function writeSplit(
  stream: OpenStream,
  text: string,
  step: { slices?: number; cut?: number[] },
) {
  const { res } = stream;
  const buf = Buffer.from(text, "utf8");
  const points = cutPoints(buf, step);
  let prev = 0;
  for (const p of points) {
    if (stream.closed) return;
    res.write(buf.subarray(prev, p));
    prev = p;
    await sleep(WRITE_GAP_MS);
  }
  if (!stream.closed) res.write(buf.subarray(prev));
  await sleep(WRITE_GAP_MS);
}

// ---------------------------------------------------------------------------
// HTTP 处理
// ---------------------------------------------------------------------------

async function readBody(req: IncomingMessage): Promise<unknown> {
  const chunks: Buffer[] = [];
  for await (const chunk of req) chunks.push(chunk as Buffer);
  if (chunks.length === 0) return null;
  const raw = Buffer.concat(chunks).toString("utf8");
  try {
    return JSON.parse(raw);
  } catch {
    return raw;
  }
}

// 请求体流只能消费一次(请求日志与路由 handler 都要读),按请求 memoize。
const bodyCache = new WeakMap<IncomingMessage, Promise<unknown>>();
function readBodyCached(req: IncomingMessage): Promise<unknown> {
  let pending = bodyCache.get(req);
  if (!pending) {
    pending = readBody(req);
    bodyCache.set(req, pending);
  }
  return pending;
}

function sendJson(res: ServerResponse, status: number, body: unknown) {
  res.writeHead(status, { "Content-Type": "application/json" });
  res.end(JSON.stringify(body));
}

function nowIso() {
  return "2026-09-20T00:00:00";
}

function installTerminalHistory(convId: string) {
  const snapshot = state.scenario.afterStreamHistory?.[convId];
  if (!snapshot) return;
  (state.scenario.messages ??= {})[convId] = snapshot.messages;
  (state.scenario.contextRevisions ??= {})[convId] = snapshot.revision;
  if (snapshot.resumable != null) {
    (state.scenario.resumable ??= {})[convId] = snapshot.resumable;
  }
}

async function handleSse(
  req: IncomingMessage,
  res: ServerResponse,
  convId: string,
) {
  const script = state.scenario.scripts?.[convId] ?? [];
  const first = script[0];

  // 剧本以 status 开头:直接返回 JSON 错误,不建立 SSE。
  if (first && first.type === "status") {
    sendJson(res, first.code, { error: `scripted ${first.code}` });
    return;
  }

  const stream: OpenStream = { res, closed: false };
  res.on("close", () => {
    stream.closed = true;
    state.streams.get(convId)?.delete(stream);
  });
  const streams = state.streams.get(convId) ?? new Set();
  streams.add(stream);
  state.streams.set(convId, streams);

  res.writeHead(200, {
    "Content-Type": "text/event-stream",
    "Cache-Control": "no-cache",
  });

  for (const step of script) {
    if (stream.closed) return;
    switch (step.type) {
      case "events":
        await writeSplit(stream, sseFrames(step.events), step);
        break;
      case "raw":
        await writeSplit(stream, step.text, step);
        break;
      case "comment":
        if (!stream.closed) res.write(": keep-alive\n\n");
        await sleep(WRITE_GAP_MS);
        break;
      case "gate":
        await gateWait(step.name);
        break;
      case "hang":
        // 挂住直到连接关闭(客户端 abort 或 cancel 注入后结束)。
        await new Promise<void>((resolve) =>
          res.once("close", () => resolve()),
        );
        return;
      case "status":
        // 仅作为第一步有意义,流已建立则忽略。
        break;
    }
  }
  if (!stream.closed) {
    installTerminalHistory(convId);
    res.end();
  }
}

async function handleApi(
  req: IncomingMessage,
  res: ServerResponse,
  pathname: string,
  searchParams: URLSearchParams,
): Promise<void> {
  const s = state.scenario;

  if (pathname === "/api/settings") {
    const settings = (s.settings ??= {
      llm_wire_log: true,
      mcp_tool_mode: "code",
      code_mode_available: true,
    });
    if (req.method === "PUT")
      Object.assign(settings, await readBodyCached(req));
    return sendJson(res, 200, settings);
  }
  const mcpSettings = pathname.match(RE_MCP_SETTINGS);
  if (mcpSettings) {
    const project = mcpSettings[1]!;
    const modes = (s.projectMcpModes ??= {});
    if (req.method === "PUT") {
      const body = (await readBodyCached(
        req,
      )) as components["schemas"]["UpdateProjectMcpSettings"];
      modes[project] = body.mcp_tool_mode ?? null;
    }
    return sendJson(res, 200, {
      mcp_tool_mode: modes[project] ?? null,
      effective_mcp_tool_mode:
        modes[project] ?? s.settings?.mcp_tool_mode ?? "code",
      code_mode_available: s.settings?.code_mode_available ?? true,
    });
  }

  const agents = pathname.match(RE_AGENT_SETTINGS);
  if (agents) {
    const scope = agents[1] ? decodeURIComponent(agents[1]) : "global";
    const key = agents[2] ? decodeURIComponent(agents[2]) : undefined;
    const views = s.agentSettings?.[scope] ?? [];
    if (req.method === "GET")
      return sendJson(res, 200, key ? views.find((v) => v.key === key) : views);
    if (req.method === "PATCH" && key) {
      const reply = s.agentPatchReplies?.[`${scope}/${key}`];
      if (!reply)
        return sendJson(res, 500, {
          error: "Missing scripted agent save reply",
        });
      if ("error" in reply) return sendJson(res, 400, reply);
      const index = views.findIndex((v) => v.key === key);
      if (index >= 0) views[index] = reply;
      return sendJson(res, 200, reply);
    }
  }
  if (RE_AGENT_TOOLS.test(pathname) && req.method === "GET")
    return sendJson(res, 200, s.agentTools ?? []);

  // Deterministic Git responses and index mutations for Changes UI tests.
  if (pathname.includes("/git/")) {
    const fixture = s.git;
    if (pathname.endsWith("/actions")) {
      const body = (await readBodyCached(
        req,
      )) as components["schemas"]["GitActionRequest"];
      if (fixture?.actionError)
        return sendJson(res, 409, { error: fixture.actionError });
      if (fixture && (body.action === "stage" || body.action === "unstage")) {
        for (const file of fixture.status.files) {
          if (body.paths && !body.paths.includes(file.path)) continue;
          const stage = body.action === "stage";
          file.index = stage ? "M" : " ";
          file.worktree = stage ? " " : "M";
          file.untracked = false;
        }
      }
      return sendJson(res, 200, { message: "Done" });
    }
    if (pathname.endsWith("/status"))
      return sendJson(
        res,
        200,
        fixture?.status ?? {
          root: "/tmp/demo",
          branch: "main",
          upstream: null,
          ahead: 0,
          behind: 0,
          remotes: [],
          files: [],
        },
      );
    if (pathname.endsWith("/history"))
      return sendJson(res, 200, fixture?.history ?? []);
    if (pathname.endsWith("/stashes"))
      return sendJson(res, 200, fixture?.stashes ?? []);
    if (pathname.endsWith("/diff"))
      return sendJson(res, 200, { patch: fixture?.patch ?? "" });
  }

  // GET /api/projects
  if (req.method === "GET" && pathname === "/api/projects") {
    return sendJson(res, 200, s.projects ?? []);
  }
  // POST /api/projects
  if (req.method === "POST" && pathname === "/api/projects") {
    const body = (await readBodyCached(req)) as {
      name?: string;
      working_dir?: string;
      additional_dirs?: string[];
    };
    const project = {
      id: `p-auto-${++state.autoId}`,
      name: body?.name ?? "Untitled",
      working_dir: body?.working_dir ?? "/tmp",
      additional_dirs: body?.additional_dirs ?? [],
      created_at: nowIso(),
      updated_at: nowIso(),
    };
    (s.projects ??= []).push(project);
    return sendJson(res, 201, project);
  }

  // GET|POST /api/projects/:pid/conversations
  let m = pathname.match(RE_PROJECT_CONVERSATIONS);
  if (m) {
    const pid = decodeURIComponent(m[1]);
    if (req.method === "GET") {
      const list = (s.conversations ?? []).filter(
        (c) => (c as { project_id?: string }).project_id === pid,
      );
      return sendJson(res, 200, list);
    }
    if (req.method === "POST") {
      const body = (await readBodyCached(req)) as { title?: string | null };
      const queued = s.newConversationIds?.shift();
      const conversation = {
        id: queued ?? `c-auto-${++state.autoId}`,
        project_id: pid,
        title: body?.title ?? null,
        // 与 schema.d.ts 对齐：显式命名即 user 来源，未命名为 default。
        title_source: body?.title ? "user" : "default",
        created_at: nowIso(),
        updated_at: nowIso(),
      };
      (s.conversations ??= []).push(conversation);
      return sendJson(res, 201, conversation);
    }
  }

  // DELETE /api/projects/:pid/conversations/:id
  m = pathname.match(RE_CONVERSATION_DELETE);
  if (m && req.method === "DELETE") {
    const id = decodeURIComponent(m[2]);
    s.conversations = (s.conversations ?? []).filter(
      (c) => (c as { id?: string }).id !== id,
    );
    res.writeHead(204);
    res.end();
    return;
  }

  m = pathname.match(RE_CONVERSATION_FORK);
  if (m && req.method === "POST") {
    const id = decodeURIComponent(m[1]);
    const reply = s.forkReplies?.[id];
    if (!reply?.response || (reply.status ?? 201) !== 201) {
      return sendJson(res, reply?.status ?? 400, {
        code: reply?.code ?? "invalid_cut",
        error: reply?.error ?? "No fork script",
      });
    }
    const result = reply.response;
    (s.conversations ??= []).push(result.conversation);
    (s.messages ??= {})[result.conversation.id] = reply.messages ?? [];
    (s.contextRevisions ??= {})[result.conversation.id] =
      result.context_revision;
    return sendJson(res, 201, result);
  }

  m = pathname.match(RE_CONVERSATION_REVERT);
  if (m && req.method === "POST") {
    const id = decodeURIComponent(m[1]);
    const reply = s.revertReplies?.[id];
    if (!reply?.response || (reply.status ?? 200) !== 200)
      return sendJson(res, reply?.status ?? 400, {
        code: reply?.code ?? "invalid_cut",
        error: reply?.error ?? "No revert script",
      });
    const result = reply.response;
    s.conversations = (s.conversations ?? []).filter(
      (c) => (c as { id: string }).id !== id,
    );
    s.conversations.push(result.conversation, result.backup);
    (s.messages ??= {})[id] = reply.messages ?? [];
    s.messages[result.backup_conversation_id] = reply.backupMessages ?? [];
    (s.contextRevisions ??= {})[id] = result.context_revision;
    s.contextRevisions[result.backup_conversation_id] =
      result.backup.backup_at_revision ?? 0;
    (s.resumable ??= {})[id] = false;
    return sendJson(res, 200, result);
  }

  // GET|POST /api/conversations/:id/messages
  m = pathname.match(RE_APPROVALS);
  if (m && req.method === "GET") {
    return sendJson(res, 200, s.approvals?.[decodeURIComponent(m[1])] ?? []);
  }
  m = pathname.match(RE_SUBAGENTS);
  if (m && req.method === "GET") {
    return sendJson(res, 200, s.subagents?.[decodeURIComponent(m[1])] ?? []);
  }
  m = pathname.match(RE_APPROVAL_DECIDE);
  if (m && req.method === "POST") {
    const id = decodeURIComponent(m[1]);
    const approval = decodeURIComponent(m[2]);
    const pending = s.approvals?.[id] ?? [];
    const gate = s.approvalGates?.[approval];
    if (!gate || !pending.some((item) => item.approval_id === approval))
      return sendJson(res, 404, { error: "Approval already resolved" });
    const body = (await readBodyCached(req)) as { decision: string };
    if (!["allow", "allow_always", "deny"].includes(body.decision))
      return sendJson(res, 400, { error: "Invalid decision" });
    s.approvals![id] = pending.filter((item) => item.approval_id !== approval);
    sendJson(res, 200, { resolved: true });
    openGate(gate);
    return;
  }

  // GET|POST /api/conversations/:id/messages
  m = pathname.match(RE_CONVERSATION_MESSAGES);
  if (m) {
    const id = decodeURIComponent(m[1]);
    if (req.method === "GET") {
      const reply = s.historyReplies?.[id]?.shift();
      if (reply?.gate) await gateWait(reply.gate);
      if (reply?.status && reply.status >= 400)
        return sendJson(res, reply.status, { error: "History unavailable" });
      if (s.contextRevisions?.[id] != null)
        res.setHeader("X-Context-Revision", String(s.contextRevisions[id]));
      return sendJson(res, 200, s.messages?.[id] ?? []);
    }
    if (req.method === "POST") {
      return handleSse(req, res, id);
    }
  }

  // POST /api/conversations/:id/cancel —— 向该会话所有打开的 SSE 注入
  // cancelled 事件并结束流(真实后端里 cancelled 由 agent loop 广播)。
  m = pathname.match(RE_CONVERSATION_CANCEL);
  if (m && req.method === "POST") {
    const id = decodeURIComponent(m[1]);
    installTerminalHistory(id);
    for (const stream of state.streams.get(id) ?? []) {
      if (!stream.closed) {
        stream.res.write(`data: ${JSON.stringify({ type: "cancelled" })}\n\n`);
        stream.res.end();
      }
    }
    res.writeHead(204);
    res.end();
    return;
  }

  // POST /api/conversations/:id/compact —— 手动压缩:向该会话的 messages
  // 追加一条 compaction 标记行(details kind=compaction)并回 CompactResult,
  // 前端随后的 GET messages 由此渲染出分隔条(与真实后端同一条链路)。
  m = pathname.match(RE_CONVERSATION_COMPACT);
  if (m && req.method === "POST") {
    const id = decodeURIComponent(m[1]);
    const rows = (s.messages ??= {})[id] ?? (s.messages[id] = []);
    const compacted = rows.length;
    const summary = "E2E 压缩摘要：完成了登录修复并跑通测试";
    const marker: MessageDto = {
      id: `m-compact-${++state.autoId}`,
      role: "user",
      parts: [
        {
          type: "text",
          text: `[Context compaction] The earlier conversation history was compacted.\n\n<summary>\n${summary}\n</summary>\n\nContinue the task based on the summary above.`,
        },
      ],
      details: {
        kind: "compaction",
        compaction: {
          triggered_by: "manual",
          compacted_message_count: compacted,
          estimated_tokens_before: 9000,
          estimated_tokens_after: 800,
        },
      },
      ordering: compacted + 1,
      created_at: nowIso(),
    };
    rows.push(marker);
    return sendJson(res, 200, {
      summary_message_id: marker.id,
      compacted_messages: compacted,
      est_tokens_before: 9000,
      est_tokens_after: 800,
      summary,
    });
  }

  // GET /api/conversations/:id/run —— 重连探测：会话存在未关闭的 SSE 流
  // 即视为运行中（真实后端以 sessions entry 的存续为准）。
  m = pathname.match(RE_CONVERSATION_RUN);
  if (m && req.method === "GET") {
    const id = decodeURIComponent(m[1]);
    const running = (state.streams.get(id) ?? []).size > 0;
    return sendJson(res, 200, {
      running,
      resumable: s.resumable?.[id] ?? false,
    });
  }

  m = pathname.match(RE_CONVERSATION_RESUME);
  if (m && req.method === "POST") {
    return handleSse(req, res, decodeURIComponent(m[1]));
  }

  // GET /api/providers
  if (req.method === "GET" && pathname === "/api/providers") {
    return sendJson(res, 200, s.providers ?? []);
  }

  // --- MCP servers:内存态 CRUD,行形状对齐 schema.d.ts 的 McpServerResponse --

  /** 行集合;顺手兜底补全 oauth 概要(剧本手写的行可能缺该字段)。 */
  function mcpRows(): McpRow[] {
    const rows = (s.mcpServers ?? []) as McpRow[];
    for (const row of rows) row.oauth ??= absentOauth();
    return rows;
  }

  function projectExists(projectId: string): boolean {
    return (s.projects ?? []).some(
      (p) => (p as { id?: string }).id === projectId,
    );
  }

  /** 公共字段校验,与真实后端(api/mcp.rs 的 validate_mcp_fields)对齐。 */
  function validateMcpFields(
    name: unknown,
    transport: unknown,
    command: unknown,
    url: unknown,
    toolTimeoutMs: unknown,
  ): { code: number; message: string } | null {
    if (typeof name === "string" && name.trim() === "") {
      return { code: 400, message: "name must not be empty" };
    }
    if (
      typeof transport === "string" &&
      transport !== "stdio" &&
      transport !== "http"
    ) {
      return {
        code: 400,
        message: `unsupported transport '${transport}'; supported: stdio, http`,
      };
    }
    if (typeof command === "string" && command.trim() === "") {
      return { code: 400, message: "command must not be empty" };
    }
    if (typeof url === "string" && url.trim() === "") {
      return { code: 400, message: "url must not be empty" };
    }
    if (typeof toolTimeoutMs === "number" && toolTimeoutMs <= 0) {
      return {
        code: 400,
        message: `tool_timeout_ms must be a positive number of milliseconds, got ${toolTimeoutMs}`,
      };
    }
    return null;
  }

  /** transport ↔ 寻址字段交叉校验,与真实后端 validate_transport_fields
   * 对齐:stdio 要 command、http 要 http(s) url;另一组的残留字段
   * (url/headers 或 command)刻意容忍并保留存储。 */
  function validateTransportFields(
    transport: string,
    command: unknown,
    url: unknown,
  ): { code: number; message: string } | null {
    if (transport === "stdio") {
      if (typeof command !== "string" || command.trim().length === 0) {
        return { code: 400, message: "stdio transport requires a command" };
      }
      return null;
    }
    if (transport === "http") {
      if (typeof url !== "string" || url.trim().length === 0) {
        return { code: 400, message: "http transport requires a url" };
      }
      if (!url.startsWith("http://") && !url.startsWith("https://")) {
        return {
          code: 400,
          message: "http transport url must start with http:// or https://",
        };
      }
      return null;
    }
    return {
      code: 400,
      message: `unsupported transport '${transport}'; supported: stdio, http`,
    };
  }

  function isStringRecord(value: unknown): value is Record<string, string> {
    return (
      typeof value === "object" &&
      value !== null &&
      !Array.isArray(value) &&
      Object.values(value).every((v) => typeof v === "string")
    );
  }

  /** PATCH 合并语义与 db::update_mcp_server 一致:提供即替换(含空集合),缺省保持。 */
  function mergeMcpRow(row: McpRow, body: Record<string, unknown>): void {
    if (typeof body.name === "string") row.name = body.name;
    if (typeof body.transport === "string") row.transport = body.transport;
    if (typeof body.command === "string") row.command = body.command;
    if (typeof body.enabled === "boolean") row.enabled = body.enabled;
    if (typeof body.tool_timeout_ms === "number") {
      row.tool_timeout_ms = body.tool_timeout_ms;
    }
    if (typeof body.project_id === "string") row.project_id = body.project_id;
    if (Array.isArray(body.args)) row.args = body.args as string[];
    if (isStringRecord(body.env)) row.env = body.env;
    if (typeof body.url === "string") row.url = body.url;
    if (isStringRecord(body.headers)) row.headers = body.headers;
    row.updated_at = nowIso();
  }

  // GET|POST /api/mcp/servers
  if (pathname === "/api/mcp/servers") {
    if (req.method === "GET") {
      const projectId = searchParams.get("project_id");
      let rows = mcpRows();
      if (projectId) {
        if (!projectExists(projectId)) {
          return sendJson(res, 404, { error: "Project not found" });
        }
        // 有效集合:enabled 且(全局或该项目专属)
        rows = rows.filter(
          (r) => r.enabled && (!r.project_id || r.project_id === projectId),
        );
      }
      return sendJson(res, 200, rows);
    }
    if (req.method === "POST") {
      const body = (await readBodyCached(req)) as {
        name?: unknown;
        transport?: unknown;
        command?: unknown;
        args?: unknown;
        env?: unknown;
        url?: unknown;
        headers?: unknown;
        enabled?: unknown;
        tool_timeout_ms?: unknown;
        project_id?: unknown;
      };
      const invalid = validateMcpFields(
        body?.name,
        body?.transport,
        body?.command,
        body?.url,
        body?.tool_timeout_ms,
      );
      if (invalid)
        return sendJson(res, invalid.code, { error: invalid.message });
      // transport ↔ 寻址字段交叉校验(与 create handler 的显式检查一致)
      const cross = validateTransportFields(
        body?.transport as string,
        body?.command,
        body?.url,
      );
      if (cross) return sendJson(res, cross.code, { error: cross.message });
      if (
        typeof body?.project_id === "string" &&
        !projectExists(body.project_id)
      ) {
        return sendJson(res, 404, { error: "Project not found" });
      }
      if (mcpRows().some((r) => r.name === body.name)) {
        return sendJson(res, 409, {
          error: `MCP server '${body.name}' already exists`,
        });
      }
      const row: McpRow = {
        id: `m-auto-${++state.autoId}`,
        name: body.name as string,
        transport: body.transport as string,
        command: (body.command as string) ?? null,
        args: Array.isArray(body.args) ? (body.args as string[]) : null,
        env: isStringRecord(body.env) ? body.env : null,
        url: (body.url as string) ?? null,
        headers: isStringRecord(body.headers) ? body.headers : null,
        enabled: typeof body.enabled === "boolean" ? body.enabled : true,
        tool_timeout_ms:
          typeof body.tool_timeout_ms === "number"
            ? body.tool_timeout_ms
            : null,
        project_id:
          typeof body.project_id === "string" ? body.project_id : null,
        created_at: nowIso(),
        updated_at: nowIso(),
        status: "not_connected",
        tool_count: null,
        error: null,
        tools: null,
        prompts: null,
        resources: null,
        oauth: absentOauth(),
      };
      (s.mcpServers ??= []).push(row);
      return sendJson(res, 201, row);
    }
  }

  // GET|PATCH|DELETE /api/mcp/servers/:id
  m = pathname.match(RE_MCP_SERVER);
  if (m) {
    const id = decodeURIComponent(m[1]);
    const rows = mcpRows();
    const idx = rows.findIndex((r) => r.id === id);
    if (req.method === "GET") {
      if (idx === -1) {
        return sendJson(res, 404, { error: "MCP server not found" });
      }
      return sendJson(res, 200, rows[idx]);
    }
    if (req.method === "PATCH") {
      if (idx === -1) {
        return sendJson(res, 404, { error: "MCP server not found" });
      }
      const body = (await readBodyCached(req)) as Record<string, unknown>;
      // update 的校验口径:字段缺省时按 existing 代入(与 validate_mcp_fields
      // 在 update 路径的取值一致),交叉校验同样跑在合并后的有效值上——
      // 与真实后端 update handler 先取 existing 再 validate_transport_fields
      // 的顺序一致。
      const existing = rows[idx];
      const invalid = validateMcpFields(
        body.name ?? existing.name,
        body.transport ?? existing.transport,
        body.command ?? existing.command,
        body.url ?? existing.url,
        body.tool_timeout_ms ?? existing.tool_timeout_ms,
      );
      if (invalid) {
        return sendJson(res, invalid.code, { error: invalid.message });
      }
      const cross = validateTransportFields(
        (body.transport ?? existing.transport) as string,
        body.command ?? existing.command,
        body.url ?? existing.url,
      );
      if (cross) {
        return sendJson(res, cross.code, { error: cross.message });
      }
      if (
        typeof body.project_id === "string" &&
        !projectExists(body.project_id)
      ) {
        return sendJson(res, 404, { error: "Project not found" });
      }
      if (
        typeof body.name === "string" &&
        rows.some((r) => r.name === body.name && r.id !== id)
      ) {
        return sendJson(res, 409, {
          error: `MCP server '${body.name}' already exists`,
        });
      }
      mergeMcpRow(existing, body);
      // 真实后端 PATCH 只回 200,无响应体
      res.writeHead(200);
      res.end();
      return;
    }
    if (req.method === "DELETE") {
      if (idx === -1) {
        return sendJson(res, 404, { error: "MCP server not found" });
      }
      s.mcpServers = rows.filter((r) => r.id !== id);
      res.writeHead(204);
      res.end();
      return;
    }
  }

  // POST /api/projects/:id/mcp/ensure —— 对账后返回该项目有效集合。
  // mock 没有真实进程可 spawn:有效集合里仍是 not_connected 的行翻成
  // connected(tool_count 缺省 1),已是 failed/connected 的保持原样。
  m = pathname.match(RE_MCP_ENSURE);
  if (m && req.method === "POST") {
    const projectId = decodeURIComponent(m[1]);
    if (!projectExists(projectId)) {
      return sendJson(res, 404, { error: "Project not found" });
    }
    const rows = mcpRows().filter(
      (r) => r.enabled && (!r.project_id || r.project_id === projectId),
    );
    for (const row of rows) {
      if (row.status === "not_connected") {
        row.status = "connected";
        row.tool_count ??= 1;
        // 真实后端返回 reconcile 时缓存的 tools/list;mock 无进程,
        // 用确定性合成工具代表"加载到了哪些工具"。
        row.tools ??= [
          { name: `${row.name}_tool`, description: `mock tool of ${row.name}` },
        ];
      }
    }
    return sendJson(res, 200, rows);
  }

  // POST /api/mcp/servers/:id/prompts/:name —— prompts/get 渲染。
  // 行缺失/未连接/剧本没写该 prompt 都回 404(对齐真实后端三态)。
  m = pathname.match(RE_MCP_PROMPT_RENDER);
  if (m && req.method === "POST") {
    const id = decodeURIComponent(m[1]);
    const name = decodeURIComponent(m[2]);
    const row = mcpRows().find((r) => r.id === id);
    if (!row) {
      return sendJson(res, 404, { error: "MCP server not found" });
    }
    if (row.status !== "connected") {
      return sendJson(res, 404, {
        error: `MCP server not connected or prompt \`${name}\` not found`,
      });
    }
    const scripted = s.mcpPromptRenders?.[id]?.[name];
    if (!scripted) {
      return sendJson(res, 404, {
        error: `MCP server not connected or prompt \`${name}\` not found`,
      });
    }
    if (scripted.status && scripted.status !== 200) {
      return sendJson(res, scripted.status, {
        error: scripted.error ?? `scripted ${scripted.status}`,
      });
    }
    return sendJson(res, 200, { messages: scripted.messages ?? [] });
  }

  // POST /api/mcp/servers/:id/resources/read —— resources/read。
  m = pathname.match(RE_MCP_RESOURCE_READ);
  if (m && req.method === "POST") {
    const id = decodeURIComponent(m[1]);
    const body = (await readBodyCached(req)) as { uri?: string };
    const uri = typeof body?.uri === "string" ? body.uri : "";
    const row = mcpRows().find((r) => r.id === id);
    if (!row) {
      return sendJson(res, 404, { error: "MCP server not found" });
    }
    if (row.status !== "connected") {
      return sendJson(res, 404, {
        error: `MCP server not connected or resource \`${uri}\` not found`,
      });
    }
    const scripted = s.mcpResourceReads?.[id]?.[uri];
    if (!scripted) {
      return sendJson(res, 404, {
        error: `MCP server not connected or resource \`${uri}\` not found`,
      });
    }
    if (scripted.status && scripted.status !== 200) {
      return sendJson(res, scripted.status, {
        error: scripted.error ?? `scripted ${scripted.status}`,
      });
    }
    return sendJson(res, 200, {
      mime_type: scripted.mime_type ?? null,
      text: scripted.text ?? null,
      blob_base64: scripted.blob_base64 ?? null,
    });
  }

  // --- MCP OAuth(http transport):start / callback / logout ---------------
  // 真实后端把浏览器送往授权服务器再 302 回 callback;mock 把"授权服务器"
  // 折叠成直接发放一个指回自身 callback 的 authorization_url——测试 stub 掉
  // window.open,取该 URL 直接 fetch 即可驱动"回调落库"这一步。

  // POST /api/mcp/servers/:id/oauth/start —— 校验 + 发放 authorization_url。
  m = pathname.match(RE_MCP_OAUTH_START);
  if (m && req.method === "POST") {
    const id = decodeURIComponent(m[1]);
    const row = mcpRows().find((r) => r.id === id);
    if (!row) {
      return sendJson(res, 404, { error: "MCP server not found" });
    }
    const scripted = s.mcpOauthStarts?.[id];
    if (scripted?.status && scripted.status !== 200) {
      return sendJson(res, scripted.status, {
        error: scripted.error ?? `scripted ${scripted.status}`,
      });
    }
    if (row.transport !== "http") {
      return sendJson(res, 400, {
        error: "OAuth login is only available for http-transport MCP servers",
      });
    }
    const stateValue = `oauth-state-${++state.autoId}`;
    state.oauthFlows.set(stateValue, id);
    return sendJson(res, 200, {
      authorization_url: `/api/mcp/oauth/callback?code=mock-code-${state.autoId}&state=${stateValue}`,
    });
  }

  // GET /api/mcp/oauth/callback —— state 单次使用;成功把 oauth 概要翻为
  // 已授权(远期过期),回极简 HTML(对齐真实后端的浏览器页)。
  if (pathname === "/api/mcp/oauth/callback" && req.method === "GET") {
    const sendPage = (status: number, ok: boolean, message: string) => {
      res.writeHead(status, { "Content-Type": "text/html; charset=utf-8" });
      res.end(
        `<!doctype html><html lang="zh"><body><h1>${ok ? "OAuth 授权成功" : "OAuth 授权失败"}</h1><p>${message}</p></body></html>`,
      );
    };
    const code = searchParams.get("code");
    const stateValue = searchParams.get("state");
    const error = searchParams.get("error");
    if (error) {
      sendPage(
        400,
        false,
        `${error}: ${searchParams.get("error_description") ?? ""}`,
      );
      return;
    }
    if (!code || !stateValue) {
      sendPage(400, false, "回调缺少 code 或 state 参数");
      return;
    }
    const serverId = state.oauthFlows.get(stateValue);
    if (serverId === undefined) {
      sendPage(400, false, "state 无效或已过期(可能是回调被重复访问)");
      return;
    }
    state.oauthFlows.delete(stateValue);
    const row = mcpRows().find((r) => r.id === serverId);
    if (!row) {
      sendPage(400, false, "MCP server not found");
      return;
    }
    row.oauth = {
      has_token: true,
      has_refresh_token: true,
      expires_at: "2030-01-01T00:00:00",
    };
    sendPage(200, true, "授权成功,可关闭此页返回应用。");
    return;
  }

  // DELETE /api/mcp/servers/:id/oauth —— 退出登录:概要回到未授权,204。
  m = pathname.match(RE_MCP_OAUTH_DELETE);
  if (m && req.method === "DELETE") {
    const id = decodeURIComponent(m[1]);
    const row = mcpRows().find((r) => r.id === id);
    if (!row) {
      return sendJson(res, 404, { error: "MCP server not found" });
    }
    row.oauth = absentOauth();
    res.writeHead(204);
    res.end();
    return;
  }

  sendJson(res, 404, { error: `mock has no route: ${req.method} ${pathname}` });
}

async function handleAdmin(
  req: IncomingMessage,
  res: ServerResponse,
  pathname: string,
): Promise<void> {
  if (req.method === "POST" && pathname === "/__mock/reset") {
    resetState();
    return sendJson(res, 200, { ok: true });
  }
  if (req.method === "PUT" && pathname === "/__mock/scenario") {
    state.scenario = (await readBodyCached(req)) as Scenario;
    return sendJson(res, 200, { ok: true });
  }
  const gateMatch = pathname.match(RE_GATE_OPEN);
  if (req.method === "POST" && gateMatch) {
    openGate(decodeURIComponent(gateMatch[1]));
    return sendJson(res, 200, { ok: true });
  }
  if (req.method === "GET" && pathname === "/__mock/requests") {
    return sendJson(res, 200, state.requests);
  }
  sendJson(res, 404, { error: `no admin route: ${pathname}` });
}

export async function startMockServer(): Promise<{
  url: string;
  close: () => Promise<void>;
}> {
  const server = http.createServer(async (req, res) => {
    const url = new URL(req.url ?? "/", "http://mock");
    const pathname = url.pathname;
    try {
      if (pathname.startsWith("/__mock")) {
        await handleAdmin(req, res, pathname);
        return;
      }
      // 请求日志只记 /api(管理通道自身的请求不计入)。
      state.requests.push({
        method: req.method ?? "?",
        path: pathname,
        body: pathname.startsWith("/api") ? await readBodyCached(req) : null,
      });
      if (pathname.startsWith("/api")) {
        await handleApi(req, res, pathname, url.searchParams);
        return;
      }
      sendJson(res, 404, { error: `unexpected path: ${pathname}` });
    } catch (error) {
      // 单个请求的异常不拖垮服务器;测试会对缺失路由拿到 500。
      if (!res.headersSent) {
        sendJson(res, 500, { error: String(error) });
      } else {
        res.end();
      }
    }
  });

  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  // unref:不挂 Vite dev server 生命周期(vitest 重启 dev server 时会执行
  // configureServer 清理回调,但不会重新求值配置函数,代理仍指向旧端口,
  // 提前 close 会让整个测试运行期 ECONNREFUSED)。unref 让监听 socket
  // 不阻塞进程退出,运行期由 dev server 保活。
  server.unref();
  const port = (server.address() as AddressInfo).port;
  return {
    url: `http://127.0.0.1:${port}`,
    close: async () => {
      server.closeAllConnections?.();
      await new Promise<void>((resolve) => server.close(() => resolve()));
    },
  };
}
