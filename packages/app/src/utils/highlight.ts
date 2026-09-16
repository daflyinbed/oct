import { createHighlighterCore, type HighlighterCore } from "shiki/core";
import { createOnigurumaEngine } from "shiki/engine/oniguruma";

import langC from "shiki/langs/c.mjs";
import langCpp from "shiki/langs/cpp.mjs";
import langCss from "shiki/langs/css.mjs";
import langDiff from "shiki/langs/diff.mjs";
import langDockerfile from "shiki/langs/dockerfile.mjs";
import langDotenv from "shiki/langs/dotenv.mjs";
import langFish from "shiki/langs/fish.mjs";
import langGo from "shiki/langs/go.mjs";
import langGraphql from "shiki/langs/graphql.mjs";
import langHtml from "shiki/langs/html.mjs";
import langIni from "shiki/langs/ini.mjs";
import langJavascript from "shiki/langs/javascript.mjs";
import langJson from "shiki/langs/json.mjs";
import langJsx from "shiki/langs/jsx.mjs";
import langLess from "shiki/langs/less.mjs";
import langMarkdown from "shiki/langs/markdown.mjs";
import langMdx from "shiki/langs/mdx.mjs";
import langPython from "shiki/langs/python.mjs";
import langRuby from "shiki/langs/ruby.mjs";
import langRust from "shiki/langs/rust.mjs";
import langScss from "shiki/langs/scss.mjs";
import langShellscript from "shiki/langs/shellscript.mjs";
import langSql from "shiki/langs/sql.mjs";
import langStylus from "shiki/langs/stylus.mjs";
import langSwift from "shiki/langs/swift.mjs";
import langToml from "shiki/langs/toml.mjs";
import langTsx from "shiki/langs/tsx.mjs";
import langTypescript from "shiki/langs/typescript.mjs";
import langVue from "shiki/langs/vue.mjs";
import langWasm from "shiki/langs/wasm.mjs";
import langXml from "shiki/langs/xml.mjs";
import langYaml from "shiki/langs/yaml.mjs";
import themeGithubDark from "shiki/themes/github-dark-default.mjs";
import themeGithubLight from "shiki/themes/github-light.mjs";

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

// 单例懒初始化：只加载上面声明的语言/主题，避免 shiki 全量 bundle。
let highlighterPromise: Promise<HighlighterCore> | null = null;

function getHighlighter(): Promise<HighlighterCore> {
  highlighterPromise ??= createHighlighterCore({
    themes: [themeGithubLight, themeGithubDark],
    langs: [
      langC,
      langCpp,
      langCss,
      langDiff,
      langDockerfile,
      langDotenv,
      langFish,
      langGo,
      langGraphql,
      langHtml,
      langIni,
      langJavascript,
      langJson,
      langJsx,
      langLess,
      langMarkdown,
      langMdx,
      langPython,
      langRuby,
      langRust,
      langScss,
      langShellscript,
      langSql,
      langStylus,
      langSwift,
      langToml,
      langTsx,
      langTypescript,
      langVue,
      langWasm,
      langXml,
      langYaml,
    ],
    engine: createOnigurumaEngine(import("shiki/wasm")),
  });
  return highlighterPromise;
}

// defaultColor: false 让明暗两套颜色都以 CSS 变量（--shiki-light/--shiki-dark）
// 输出，由 .code-view 的样式跟随 data-theme 切换。
export async function highlightCode(code: string, lang: string): Promise<string> {
  const highlighter = await getHighlighter();
  const resolved = highlighter.getLoadedLanguages().includes(lang) ? lang : "plaintext";
  return highlighter.codeToHtml(code, {
    lang: resolved,
    themes: { light: "github-light", dark: "github-dark-default" },
    defaultColor: false,
  });
}
