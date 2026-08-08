import xwbx from "@xwbx/eslint-config";
import storybook from "eslint-plugin-storybook";

export default xwbx(
  {
    ignores: ["packages/app/src/components/ui"],
  },
  ...storybook.configs["flat/recommended"],
);
