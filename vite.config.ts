import { defineConfig } from "vite";

export default defineConfig({
  clearScreen: false,
  server: { port: 1420, strictPort: true },
  build: {
    rollupOptions: {
      // The main window and the on-screen dictation pill.
      input: { main: "index.html", overlay: "overlay.html" },
    },
  },
});
