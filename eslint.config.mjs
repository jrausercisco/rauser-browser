import js from "@eslint/js";
import { defineConfig } from "eslint/config";
import tseslint from "typescript-eslint";

export default defineConfig({
  files: ["protocol/ts/**/*.ts", "extension/**/*.{ts,tsx}"],
  extends: [js.configs.recommended, tseslint.configs.recommended],
});
