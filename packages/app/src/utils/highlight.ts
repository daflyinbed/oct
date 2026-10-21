import { createHighlighterCore } from "shiki/core";
import { createOnigurumaEngine } from "shiki/engine/oniguruma";
import themeGithubDark from "shiki/themes/github-dark-default.mjs";
import themeGithubLight from "shiki/themes/github-light.mjs";
import type { HighlighterCore } from "shiki/core";

// 与 FileTreePanel 图标表对应的常用语言集合；未列出的后缀回退 plaintext。
const EXTENSION_LANGUAGES: Record<string, string> = {
  c: "c",
  h: "c",
  cc: "cpp",
  cpp: "cpp",
  cxx: "cpp",
  hpp: "cpp",
  css: "css",
  postcss: "css",
  scss: "scss",
  sass: "scss",
  less: "less",
  styl: "stylus",
  patch: "diff",
  fish: "fish",
  go: "go",
  gql: "graphql",
  graphql: "graphql",
  htm: "html",
  html: "html",
  svg: "xml",
  xml: "xml",
  ini: "ini",
  cfg: "ini",
  js: "javascript",
  mjs: "javascript",
  cjs: "javascript",
  jsx: "jsx",
  json: "json",
  jsonc: "json",
  json5: "json",
  md: "markdown",
  markdown: "markdown",
  mdx: "mdx",
  py: "python",
  pyi: "python",
  rb: "ruby",
  erb: "ruby",
  rs: "rust",
  sh: "shellscript",
  bash: "shellscript",
  zsh: "shellscript",
  sql: "sql",
  db: "sql",
  sqlite: "sql",
  sqlite3: "sql",
  swift: "swift",
  toml: "toml",
  ts: "typescript",
  mts: "typescript",
  cts: "typescript",
  tsx: "tsx",
  vue: "vue",
  wasm: "wasm",
  yaml: "yaml",
  yml: "yaml",
};

const FILENAME_LANGUAGES: Record<string, string> = {
  dockerfile: "dockerfile",
};

export function languageForPath(path: string): string {
  const name = (path.split("/").pop() ?? path).toLowerCase();
  if (name.startsWith(".env")) return "dotenv";
  const byName = FILENAME_LANGUAGES[name];
  if (byName) return byName;
  const extension = name.split(".").pop() ?? "";
  return EXTENSION_LANGUAGES[extension] ?? "plaintext";
}

// 单例只初始化引擎和主题，语言（及其内嵌依赖）按实际预览类型加载。
const LANGUAGE_LOADERS = {
  c: () => import("shiki/langs/c.mjs"),
  cpp: () => import("shiki/langs/cpp.mjs"),
  css: () => import("shiki/langs/css.mjs"),
  diff: () => import("shiki/langs/diff.mjs"),
  dockerfile: () => import("shiki/langs/dockerfile.mjs"),
  dotenv: () => import("shiki/langs/dotenv.mjs"),
  fish: () => import("shiki/langs/fish.mjs"),
  go: () => import("shiki/langs/go.mjs"),
  graphql: () => import("shiki/langs/graphql.mjs"),
  html: () => import("shiki/langs/html.mjs"),
  ini: () => import("shiki/langs/ini.mjs"),
  javascript: () => import("shiki/langs/javascript.mjs"),
  json: () => import("shiki/langs/json.mjs"),
  jsx: () => import("shiki/langs/jsx.mjs"),
  less: () => import("shiki/langs/less.mjs"),
  markdown: () => import("shiki/langs/markdown.mjs"),
  mdx: () => import("shiki/langs/mdx.mjs"),
  python: () => import("shiki/langs/python.mjs"),
  ruby: () => import("shiki/langs/ruby.mjs"),
  rust: () => import("shiki/langs/rust.mjs"),
  scss: () => import("shiki/langs/scss.mjs"),
  shellscript: () => import("shiki/langs/shellscript.mjs"),
  sql: () => import("shiki/langs/sql.mjs"),
  stylus: () => import("shiki/langs/stylus.mjs"),
  swift: () => import("shiki/langs/swift.mjs"),
  toml: () => import("shiki/langs/toml.mjs"),
  tsx: () => import("shiki/langs/tsx.mjs"),
  typescript: () => import("shiki/langs/typescript.mjs"),
  vue: () => import("shiki/langs/vue.mjs"),
  wasm: () => import("shiki/langs/wasm.mjs"),
  xml: () => import("shiki/langs/xml.mjs"),
  yaml: () => import("shiki/langs/yaml.mjs"),
};
let highlighterPromise: Promise<HighlighterCore> | null = null;
const languagePromises = new Map<string, Promise<void>>();

function getHighlighter(): Promise<HighlighterCore> {
  highlighterPromise ??= createHighlighterCore({
    themes: [themeGithubLight, themeGithubDark],
    langs: [],
    engine: createOnigurumaEngine(import("shiki/wasm")),
  }).catch((error) => {
    highlighterPromise = null;
    throw error;
  });
  return highlighterPromise;
}

async function loadLanguage(highlighter: HighlighterCore, lang: string) {
  if (!Object.keys(LANGUAGE_LOADERS).includes(lang)) return;
  if (highlighter.getLoadedLanguages().includes(lang)) return;
  let pending = languagePromises.get(lang);
  if (!pending) {
    const loader = LANGUAGE_LOADERS[lang as keyof typeof LANGUAGE_LOADERS];
    pending = loader()
      .then((module) => highlighter.loadLanguage(module.default))
      .catch((error) => {
        languagePromises.delete(lang);
        throw error;
      });
    languagePromises.set(lang, pending);
  }
  await pending;
}

// defaultColor: false 让明暗两套颜色都以 CSS 变量（--shiki-light/--shiki-dark）
// 输出，由 .code-view 的样式跟随 data-theme 切换。
export async function highlightCode(
  code: string,
  lang: string,
): Promise<string> {
  const highlighter = await getHighlighter();
  await loadLanguage(highlighter, lang);
  const resolved = highlighter.getLoadedLanguages().includes(lang)
    ? lang
    : "plaintext";
  return highlighter.codeToHtml(code, {
    lang: resolved,
    themes: { light: "github-light", dark: "github-dark-default" },
    defaultColor: false,
  });
}
