import xwbx from "@xwbx/eslint-config";

export default xwbx({
  ignores: [
    "packages/app/src/components/ui",
    // openapi-typescript 自动生成，重新生成会覆盖手工格式化
    "packages/app/src/api/schema.d.ts",
  ],
});
