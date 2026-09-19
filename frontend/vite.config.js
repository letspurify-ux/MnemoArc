import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import { mermaidMathAdapter } from "./mermaid-math-adapter.mjs";
const server = {
  host: "127.0.0.1",
  port: 5173,
  strictPort: true,
  proxy: { "/api": `http://127.0.0.1:${process.env.MNEMOARC_PORT || 3030}` },
};
export default defineConfig({
  plugins: [mermaidMathAdapter(), react()],
  server,
  preview: { ...server, port: 4173 },
  build: { target: "es2020" },
});
