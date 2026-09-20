import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

export default defineConfig({
  plugins: [react()],
  // Tauri serves the dev build from a fixed port and fails if it moves.
  server: { port: 5173, strictPort: true },
  build: { target: "safari15", outDir: "dist" },
  clearScreen: false,
});
