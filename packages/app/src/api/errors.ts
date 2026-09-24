import type { components } from "@/api/schema";

/**
 * 后端错误体（api/error.rs 的 ErrorBody）：`code` 是稳定的机器可读
 * 原因（"not_found" / "bad_request" / "conflict" / "already_running" /
 * "nothing_to_resume" / "bad_gateway" / "internal"），`error` 是可直接
 * 展示的文案。所有非 2xx 响应共用这一个形状。
 */
export type ErrorBody = components["schemas"]["ErrorBody"];

/** 归一化后的失败结果：三类可区分。 */
export interface ApiError {
  /**
   * "http"：服务器返回的业务错误（ErrorBody 或裸 `{error}` 体）；
   * "network"：请求未送达或连接中断（fetch reject，非取消）；
   * "aborted"：请求被主动取消（AbortError）。
   */
  kind: "http" | "network" | "aborted";
  /** 后端稳定 code（仅 http，且体中携带时存在）。 */
  code?: string;
  message: string;
}

/**
 * 集中归一化 openapi-fetch 的错误返回（业务错误对象）与网络层异常
 * （fetch 抛出的 Error）。各 composable 以此取文案，保留自己的
 * 操作专属 fallback；分支逻辑走 `kind` / `code` 而非解析文案。
 */
export function apiError(cause: unknown, fallback: string): ApiError {
  // 取消优先判定（DOMException 也是 Error，顺序不能颠倒）。
  if (cause instanceof DOMException && cause.name === "AbortError") {
    return { kind: "aborted", message: fallback };
  }
  // fetch 网络异常：reject 出来的是 Error 实例（"Failed to fetch" 等），
  // 没有响应体可取。
  if (cause instanceof Error) {
    return { kind: "network", message: fallback };
  }
  // openapi-fetch 的业务错误返回：错误体对象（声明的 error schema 或
  // 运行时形状 `{code?, error}`）。
  const body = cause as { code?: unknown; error?: unknown } | null | undefined;
  if (body && typeof body === "object") {
    const message =
      typeof body.error === "string" && body.error ? body.error : fallback;
    const code = typeof body.code === "string" ? body.code : undefined;
    return { kind: "http", code, message };
  }
  return { kind: "network", message: fallback };
}

/** 只要文案的便捷形式。 */
export function apiErrorMessage(cause: unknown, fallback: string): string {
  return apiError(cause, fallback).message;
}
