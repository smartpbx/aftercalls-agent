import { defineConfig } from "vite";
import { sveltekit } from "@sveltejs/kit/vite";
import { readFileSync as read } from "node:fs";
import { execSync as exec } from "node:child_process";

const host = process.env.TAURI_DEV_HOST;

function agentVersion() {
  try {
    const toml = read(new URL("src-tauri/Cargo.toml", import.meta.url), "utf8");
    const m = toml.match(/^\s*version\s*=\s*"([^"]+)"/m);
    return m ? `v${m[1]}` : "";
  } catch {
    return "";
  }
}

function gitSha() {
  try {
    return exec("git rev-parse --short HEAD", {
      stdio: ["ignore", "pipe", "ignore"],
    })
      .toString()
      .trim();
  } catch {
    return "dev";
  }
}

const git = { sha: gitSha(), tag: agentVersion() };

// https://vite.dev/config/
export default defineConfig(async () => ({
  plugins: [sveltekit()],

  // Vite options tailored for Tauri development and only applied in `tauri dev` or `tauri build`
  //
  // 1. prevent Vite from obscuring rust errors
  clearScreen: false,
  // 2. tauri expects a fixed port, fail if that port is not available
  server: {
    port: 1420,
    strictPort: true,
    host: host || false,
    hmr: host
      ? {
          protocol: "ws",
          host,
          port: 1421,
        }
      : undefined,
    watch: {
      // 3. tell Vite to ignore watching `src-tauri`
      ignored: ["**/src-tauri/**"],
    },
  },
  define: {
    "import.meta.env.VITE_BUILD_SHA": JSON.stringify(git.sha),
    "import.meta.env.VITE_BUILD_TAG": JSON.stringify(git.tag),
  },
}));
