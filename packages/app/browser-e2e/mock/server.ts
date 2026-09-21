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
import type { AddressInfo, IncomingMessage, ServerResponse } from "node:http";

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
  | StepEvents
  | StepRaw
  | StepComment
  | StepGate
  | StepStatus
  | StepHang;

export interface Scenario {
  /** GET /api/projects 的返回体(形状见 schema.d.ts 的 Project)。 */
  projects?: unknown[];
  /** 全部会话(按 project_id 过滤返回)。 */
  conversations?: unknown[];
  /** GET /api/conversations/:id/messages 的返回体(StoredMessage[])。 */
  messages?: Record<string, unknown[]>;
  /** GET /api/providers 的返回体(ProviderResponse[])。 */
  providers?: unknown[];
  /** POST 建会话时按序发放的确定性 id(耗尽后回退 c-auto-N)。 */
  newConversationIds?: string[];
  /** 每个会话的 SSE 剧本,POST 消息时惰性取用。 */
  scripts?: Record<string, ScriptStep[]>;
}

export interface CapturedRequest {
  method: string;
  path: string;
  body: unknown;
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
} = {
  scenario: {},
  autoId: 0,
  requests: [],
  streams: new Map(),
  gates: new Map(),
  openedGates: new Set(),
};

/** 相邻两次 socket 写入的间隔:促使 TCP 分段,字节边界才会真实生效。 */
const WRITE_GAP_MS = 4;

const sleep = (ms: number) => new Promise((resolve) => setTimeout(resolve, ms));

// --- 路由匹配(模块级正则) ---------------------------------------------------

const RE_PROJECT_CONVERSATIONS = /^\/api\/projects\/([^/]+)\/conversations$/;
const RE_CONVERSATION_DELETE =
  /^\/api\/projects\/([^/]+)\/conversations\/([^/]+)$/;
const RE_CONVERSATION_MESSAGES = /^\/api\/conversations\/([^/]+)\/messages$/;
const RE_CONVERSATION_CANCEL = /^\/api\/conversations\/([^/]+)\/cancel$/;
const RE_CONVERSATION_RUN = /^\/api\/conversations\/([^/]+)\/run$/;
const RE_GATE_OPEN = /^\/__mock\/gates\/([^/]+)\/open$/;

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

function sendJson(res: ServerResponse, status: number, body: unknown) {
  res.writeHead(status, { "Content-Type": "application/json" });
  res.end(JSON.stringify(body));
}

function nowIso() {
  return "2026-09-20T00:00:00";
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
  if (!stream.closed) res.end();
}

async function handleApi(
  req: IncomingMessage,
  res: ServerResponse,
  pathname: string,
): Promise<void> {
  const s = state.scenario;

  // GET /api/projects
  if (req.method === "GET" && pathname === "/api/projects") {
    return sendJson(res, 200, s.projects ?? []);
  }
  // POST /api/projects
  if (req.method === "POST" && pathname === "/api/projects") {
    const body = (await readBody(req)) as {
      name?: string;
      working_dir?: string;
    };
    const project = {
      id: `p-auto-${++state.autoId}`,
      name: body?.name ?? "Untitled",
      working_dir: body?.working_dir ?? "/tmp",
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
      const body = (await readBody(req)) as { title?: string | null };
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

  // GET|POST /api/conversations/:id/messages
  m = pathname.match(RE_CONVERSATION_MESSAGES);
  if (m) {
    const id = decodeURIComponent(m[1]);
    if (req.method === "GET") {
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

  // GET /api/conversations/:id/run —— 重连探测：会话存在未关闭的 SSE 流
  // 即视为运行中（真实后端以 sessions entry 的存续为准）。
  m = pathname.match(RE_CONVERSATION_RUN);
  if (m && req.method === "GET") {
    const id = decodeURIComponent(m[1]);
    const running = (state.streams.get(id) ?? []).size > 0;
    return sendJson(res, 200, { running });
  }

  // GET /api/providers
  if (req.method === "GET" && pathname === "/api/providers") {
    return sendJson(res, 200, s.providers ?? []);
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
    state.scenario = (await readBody(req)) as Scenario;
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
    const pathname = new URL(req.url ?? "/", "http://mock").pathname;
    try {
      if (pathname.startsWith("/__mock")) {
        await handleAdmin(req, res, pathname);
        return;
      }
      // 请求日志只记 /api(管理通道自身的请求不计入)。
      state.requests.push({
        method: req.method ?? "?",
        path: pathname,
        body: pathname.startsWith("/api") ? await readBody(req) : null,
      });
      if (pathname.startsWith("/api")) {
        await handleApi(req, res, pathname);
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
