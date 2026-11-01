// The console is one HTML file, scripts and styles inlined, so sandstormd can
// embed it with include_str! and serve it at / with no other routes.
import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import { viteSingleFile } from "vite-plugin-singlefile";

export default defineConfig({
  plugins: [react(), viteSingleFile()],
  build: { outDir: "dist", emptyOutDir: true, target: "es2022" },
  // `npm run dev` against a local master: the API is proxied, so the page and
  // the API share an origin exactly as they do when sandstormd serves it.
  server: {
    proxy: Object.fromEntries(
      ["/v1", "/metrics", "/install.sh"].map((p) => [p, { target: process.env.SANDSTORM_MASTER ?? "http://127.0.0.1:8898", changeOrigin: false }]),
    ),
  },
});
