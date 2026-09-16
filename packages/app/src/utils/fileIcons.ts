import type { Component } from "vue";

import IconTypeScript from "~icons/vscode-icons/file-type-typescript";
import IconJs from "~icons/vscode-icons/file-type-js";
import IconJson from "~icons/vscode-icons/file-type-json";
import IconVue from "~icons/vscode-icons/file-type-vue";
import IconCss from "~icons/vscode-icons/file-type-css";
import IconSass from "~icons/vscode-icons/file-type-sass";
import IconHtml from "~icons/vscode-icons/file-type-html";
import IconMarkdown from "~icons/vscode-icons/file-type-markdown";
import IconRust from "~icons/vscode-icons/file-type-rust";
import IconPython from "~icons/vscode-icons/file-type-python";
import IconGo from "~icons/vscode-icons/file-type-go";
import IconRuby from "~icons/vscode-icons/file-type-ruby";
import IconShell from "~icons/vscode-icons/file-type-shell";
import IconYaml from "~icons/vscode-icons/file-type-yaml";
import IconToml from "~icons/vscode-icons/file-type-toml";
import IconSql from "~icons/vscode-icons/file-type-sql";
import IconSqlite from "~icons/vscode-icons/file-type-sqlite";
import IconSvg from "~icons/vscode-icons/file-type-svg";
import IconImage from "~icons/vscode-icons/file-type-image";
import IconZip from "~icons/vscode-icons/file-type-zip";
import IconText from "~icons/vscode-icons/file-type-text";
import IconC from "~icons/vscode-icons/file-type-c";
import IconCpp from "~icons/vscode-icons/file-type-cpp";
import IconSwift from "~icons/vscode-icons/file-type-swift";
import IconGraphql from "~icons/vscode-icons/file-type-graphql";
import IconWasm from "~icons/vscode-icons/file-type-wasm";
import IconDocker from "~icons/vscode-icons/file-type-docker";
import IconGit from "~icons/vscode-icons/file-type-git";
import IconNpm from "~icons/vscode-icons/file-type-npm";
import IconVite from "~icons/vscode-icons/file-type-vite";
import IconFile from "~icons/lucide/file";

export interface FileIconTarget {
  name: string;
  kind: "dir" | "file";
}

const FILENAME_ICONS: Record<string, Component> = {
  "package.json": IconNpm,
  "package-lock.json": IconNpm,
  "pnpm-lock.yaml": IconNpm,
  dockerfile: IconDocker,
  "docker-compose.yml": IconDocker,
  "docker-compose.yaml": IconDocker,
  "compose.yml": IconDocker,
  "compose.yaml": IconDocker,
  ".gitignore": IconGit,
  ".gitattributes": IconGit,
  ".gitmodules": IconGit,
  ".gitkeep": IconGit,
};

const EXTENSION_ICONS: Record<string, Component> = {
  ts: IconTypeScript,
  mts: IconTypeScript,
  cts: IconTypeScript,
  tsx: IconTypeScript,
  js: IconJs,
  mjs: IconJs,
  cjs: IconJs,
  jsx: IconJs,
  json: IconJson,
  jsonc: IconJson,
  json5: IconJson,
  vue: IconVue,
  css: IconCss,
  postcss: IconCss,
  scss: IconSass,
  sass: IconSass,
  less: IconSass,
  styl: IconSass,
  html: IconHtml,
  htm: IconHtml,
  md: IconMarkdown,
  mdx: IconMarkdown,
  markdown: IconMarkdown,
  rs: IconRust,
  py: IconPython,
  pyi: IconPython,
  go: IconGo,
  rb: IconRuby,
  erb: IconRuby,
  sh: IconShell,
  bash: IconShell,
  zsh: IconShell,
  fish: IconShell,
  yaml: IconYaml,
  yml: IconYaml,
  toml: IconToml,
  sql: IconSql,
  db: IconSqlite,
  sqlite: IconSqlite,
  sqlite3: IconSqlite,
  svg: IconSvg,
  png: IconImage,
  jpg: IconImage,
  jpeg: IconImage,
  gif: IconImage,
  webp: IconImage,
  bmp: IconImage,
  ico: IconImage,
  avif: IconImage,
  zip: IconZip,
  tar: IconZip,
  gz: IconZip,
  tgz: IconZip,
  "7z": IconZip,
  txt: IconText,
  log: IconText,
  ini: IconText,
  cfg: IconText,
  c: IconC,
  h: IconC,
  cc: IconCpp,
  cpp: IconCpp,
  cxx: IconCpp,
  hpp: IconCpp,
  swift: IconSwift,
  gql: IconGraphql,
  graphql: IconGraphql,
  wasm: IconWasm,
};

// Icon resolution follows pierre's rule order: exact filename first, then
// extension, then the generic file glyph.
export function resolveFileIcon(target: FileIconTarget): Component {
  if (target.kind === "dir") return IconFile;

  const lowerName = target.name.toLowerCase();
  const exact = FILENAME_ICONS[lowerName];
  if (exact) return exact;
  if (lowerName.startsWith("vite.config")) return IconVite;
  if (lowerName.startsWith(".env")) return IconText;

  const extension = lowerName.split(".").pop();
  return (extension && EXTENSION_ICONS[extension]) || IconFile;
}
