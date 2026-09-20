import { spawn } from "node:child_process";
import { createRequire } from "node:module";
import { createServer } from "node:net";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { setTimeout as delay } from "node:timers/promises";

const root = fileURLToPath(new URL("..", import.meta.url));
const require = createRequire(import.meta.url);
const children = new Set();
let closing = false;
let backend;

function launch(command, args, options = {}) {
  const child = spawn(command, args, {
    cwd: root,
    windowsHide: true,
    stdio: "inherit",
    ...options,
  });
  children.add(child);
  child.finished = new Promise((resolve) => {
    child.once("error", (error) => {
      console.error(error.message);
      children.delete(child);
      resolve(1);
    });
    child.once("exit", (code) => {
      children.delete(child);
      resolve(code ?? 1);
    });
  });
  return child;
}
async function stop(code = 0) {
  if (closing) return;
  closing = true;
  await Promise.all(
    [...children].map(async (child) => {
      if (child === backend) {
        // EOF requests an orderly shutdown on Windows as well as Unix.
        child.stdin.end();
      } else if (process.platform === "win32" && child.pid) {
        const killer = spawn(
          "taskkill",
          ["/PID", String(child.pid), "/T", "/F"],
          { stdio: "ignore" },
        );
        killer.on("error", () => child.kill());
      } else {
        child.kill("SIGINT");
      }
      await child.finished;
    }),
  );
  process.exit(code);
}
process.on("SIGINT", () => void stop());
process.on("SIGTERM", () => void stop());
process.on("message", (message) => {
  if (message === "stop") void stop();
});
if (process.send) process.on("disconnect", () => void stop());

function port(value, fallback) {
  const number = Number(value ?? fallback);
  if (!Number.isInteger(number) || number < 1 || number > 65535)
    throw new Error(`Invalid port: ${value}`);
  return number;
}
async function checkPort(number) {
  const server = createServer();
  await new Promise((resolve, reject) => {
    server.once("error", () =>
      reject(
        new Error(
          `Port ${number} is already in use. Stop its owner or choose another port.`,
        ),
      ),
    );
    server.listen(number, "127.0.0.1", resolve);
  });
  await new Promise((resolve) => server.close(resolve));
}
async function ready(url) {
  for (let attempt = 0; attempt < 300 && !closing; attempt++) {
    try {
      const response = await fetch(url, { signal: AbortSignal.timeout(1000) });
      await response.body?.cancel();
      if (response.ok) return;
    } catch {
      /* Server is still starting. */
    }
    await delay(200);
  }
  throw new Error(`Server did not become ready: ${url}`);
}

try {
  const backendPort = port(process.env.MNEMOARC_PORT, 3030);
  const frontendPort = port(process.env.MNEMOARC_FRONTEND_PORT, 5173);
  if (backendPort === frontendPort)
    throw new Error("Backend and frontend ports must differ.");
  await Promise.all([checkPort(backendPort), checkPort(frontendPort)]);
  try {
    require.resolve("vite/package.json");
  } catch {
    console.log("Installing frontend dependencies with npm ci...");
    const install =
      process.platform === "win32"
        ? launch("cmd.exe", ["/d", "/s", "/c", "npm ci"])
        : launch("npm", ["ci"]);
    if ((await install.finished) !== 0) throw new Error("npm ci failed.");
  }
  if (closing) process.exit(1);
  const build = launch("cargo", ["build"]);
  if ((await build.finished) !== 0)
    throw new Error(
      "Rust build failed. Install the Rust toolchain and try again.",
    );
  if (closing) process.exit(1);
  const target = process.env.CARGO_TARGET_DIR
    ? resolve(root, process.env.CARGO_TARGET_DIR)
    : join(root, "target");
  backend = launch(
    join(
      target,
      "debug",
      process.platform === "win32" ? "mnemoarc.exe" : "mnemoarc",
    ),
    [
      "--config",
      "config.toml",
      "web",
      "--no-open",
      "--port",
      String(backendPort),
      "--shutdown-on-stdin",
    ],
    { stdio: ["pipe", "inherit", "inherit"] },
  );
  backend.stdin.on("error", () => {});
  const vite = launch(
    process.execPath,
    [
      join(dirname(require.resolve("vite/package.json")), "bin/vite.js"),
      "--port",
      String(frontendPort),
    ],
    {
      cwd: join(root, "frontend"),
      env: { ...process.env, MNEMOARC_PORT: String(backendPort) },
    },
  );
  for (const child of [backend, vite]) {
    void child.finished.then((code) => {
      if (!closing) void stop(code || 1);
    });
  }
  const url = `http://127.0.0.1:${frontendPort}`;
  await Promise.all([
    ready(`http://127.0.0.1:${backendPort}/api/state`),
    ready(`${url}/api/state`),
  ]);
  console.log(`MnemoArc is ready: ${url}`);
  process.send?.({ ready: true, url });
} catch (error) {
  console.error(error.message);
  await stop(1);
}
