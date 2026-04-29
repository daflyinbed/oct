import path from "node:path";
import { fileURLToPath } from "node:url";
import { defineConfig } from "rolldown";
import { dts } from "rolldown-plugin-dts";

const dirname =
  typeof __dirname !== "undefined"
    ? __dirname
    : path.dirname(fileURLToPath(import.meta.url));

export default defineConfig({
  input: {
    index: path.resolve(dirname, "src/index.ts"),
    resolver: path.resolve(dirname, "src/resolver.ts"),
  },
  external: [/node_modules/, "vue", "unplugin-vue-components"],
  plugins: [
    dts({
      tsconfig: path.resolve(dirname, "tsconfig.app.json"),
      vue: true,
      emitDtsOnly: true,
      compilerOptions: {
        declaration: true,
        emitDeclarationOnly: true,
      },
    }),
  ],
  output: {
    dir: path.resolve(dirname, "dist"),
    entryFileNames: "[name].d.ts",
  },
});
