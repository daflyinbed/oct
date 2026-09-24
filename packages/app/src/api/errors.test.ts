import { describe, expect, it } from "vitest";
import { apiError } from "@/api/errors";

describe("apiError 归一化", () => {
  it("hTTP 业务错误：带 code 的 ErrorBody 原样取出", () => {
    const result = apiError(
      { code: "already_running", error: "Agent is already running" },
      "fallback",
    );
    expect(result).toEqual({
      kind: "http",
      code: "already_running",
      message: "Agent is already running",
    });
  });

  it("hTTP 业务错误：无 code 的旧形状仍有文案、code 缺省", () => {
    const result = apiError({ error: "boom" }, "fallback");
    expect(result.kind).toBe("http");
    expect(result.code).toBeUndefined();
    expect(result.message).toBe("boom");
  });

  it("hTTP 业务错误：体里无可读文案时用操作专属 fallback", () => {
    const result = apiError({}, "加载失败");
    expect(result).toEqual({
      kind: "http",
      code: undefined,
      message: "加载失败",
    });
  });

  it("网络异常（fetch reject 的 Error）与取消（AbortError）可区分", () => {
    expect(apiError(new TypeError("Failed to fetch"), "网络失败")).toEqual({
      kind: "network",
      message: "网络失败",
    });
    const abort = new DOMException("The user aborted", "AbortError");
    expect(apiError(abort, "已取消")).toEqual({
      kind: "aborted",
      message: "已取消",
    });
  });
});
