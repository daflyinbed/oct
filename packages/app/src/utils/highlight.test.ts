import { describe, expect, it, vi } from "vitest";
import { highlightCode, languageForPath } from "./highlight";

const cppImported = vi.hoisted(() => vi.fn());
vi.mock("shiki/langs/cpp.mjs", async (importOriginal) => {
  cppImported();
  return importOriginal();
});

describe("file preview highlighting", () => {
  it("loads C++ only when requested and supports concurrent language loads", async () => {
    expect(languageForPath("src/main.cpp")).toBe("cpp");
    const html = await highlightCode("const value: number = 1", "typescript");
    expect(html).toContain("--shiki-light");
    expect(cppImported).not.toHaveBeenCalled();
    const results = await Promise.all([
      highlightCode("int main() { return 0; }", "cpp"),
      highlightCode("const int value = 1;", "cpp"),
    ]);
    expect(cppImported).toHaveBeenCalledTimes(1);
    for (const result of results) expect(result).toContain("--shiki-dark");
  });

  it("loads embedded Vue languages and escapes unknown-language text", async () => {
    const html = await highlightCode(
      '<script lang="ts">const n = 1</script>',
      "vue",
    );
    expect(html).toContain("--shiki-light");
    expect(html).toContain("const");
    const plain = await highlightCode("<unsafe>&", "unknown");
    expect(plain).not.toContain("<unsafe>");
    expect(plain).toMatch(/(?:&lt;|&#x3c;)unsafe(?:&gt;|>)/i);
    expect(languageForPath("notes.unknown")).toBe("plaintext");
    expect(languageForPath(".env.local")).toBe("dotenv");
  });
});
