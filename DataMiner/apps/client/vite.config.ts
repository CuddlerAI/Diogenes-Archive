import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  server: { port: 1421, strictPort: true },
  define: {
    // 构建风味注入前端。默认 unsigned-dev——忘了设的构建只会低估自己，不会
    // 声称自己是生产版。只有 tools/build-windows-installer.ps1 会设成 production，
    // 而它在打包后要断言产物真的带有效签名，见 src/build-flavor.ts。
    __DATAMINER_BUILD_FLAVOR__: JSON.stringify(
      ["production", "unsigned-delivered"].includes(process.env.DATAMINER_BUILD_FLAVOR ?? "")
        ? process.env.DATAMINER_BUILD_FLAVOR
        : "unsigned-dev",
    ),
  },
});
