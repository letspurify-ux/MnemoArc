// Shared background launcher for start_all/stop_all on Windows, macOS and Linux.
// Only the supervisor holding this checkout's local socket can stop its children.
import { fork } from "node:child_process";
import { createHash } from "node:crypto";
import { mkdir, readFile, unlink } from "node:fs/promises";
import { createConnection, createServer } from "node:net";
import { homedir, tmpdir } from "node:os";
import { join } from "node:path";
import { finished } from "node:stream/promises";
import { fileURLToPath } from "node:url";
import {
  DailyLogSink,
  dailyLogPath,
  pruneLogs,
  retentionDays,
} from "./service-log.mjs";

const root = fileURLToPath(new URL("..", import.meta.url));
const id = createHash("sha256")
  .update(`${homedir()}:${root}`)
  .digest("hex")
  .slice(0, 24);
const socketDir = join(tmpdir(), `mnemoarc-${id}`);
const endpoint =
  process.platform === "win32"
    ? `\\\\.\\pipe\\mnemoarc-${id}`
    : join(socketDir, "control.sock");
const logDirectory = join(root, ".mnemoarc");

function request(command) {
  return new Promise((resolve, reject) => {
    const socket = createConnection(endpoint);
    let text = "";
    socket.setTimeout(10000, () =>
      socket.destroy(
        new Error("Launcher is not responding; no processes were killed."),
      ),
    );
    socket.once("connect", () => socket.end(`${command}\n`));
    socket.on("data", (chunk) => {
      text += chunk;
    });
    socket.once("error", reject);
    socket.once("end", () => {
      try {
        resolve(JSON.parse(text));
      } catch {
        reject(new Error("Invalid launcher response."));
      }
    });
  });
}
async function status() {
  try {
    return await request("status");
  } catch (error) {
    if (["ENOENT", "ECONNREFUSED"].includes(error.code)) return null;
    throw error;
  }
}

async function supervise() {
  await mkdir(socketDir, { recursive: true, mode: 0o700 });
  let current = { state: "starting" };
  let child;
  let stopping = false;
  const server = createServer({ allowHalfOpen: true }, (socket) => {
    socket.setTimeout(1000, () => socket.destroy());
    socket.on("error", () => {});
    let input = "";
    socket.on("data", (chunk) => {
      input += chunk;
      if (input.length > 32) return socket.destroy();
      if (!input.includes("\n")) return;
      const command = input.trim();
      socket.end(JSON.stringify(current));
      if (command === "stop") void stop();
    });
  });
  async function listen() {
    await new Promise((resolve, reject) => {
      server.once("error", reject);
      server.listen(endpoint, () => {
        server.removeListener("error", reject);
        resolve();
      });
    });
  }
  try {
    await listen();
  } catch (error) {
    if (error.code !== "EADDRINUSE") throw error;
    const existing = await status();
    if (existing) {
      process.send?.({ ready: true, ...existing });
      process.disconnect?.();
      return;
    }
    if (process.platform === "win32") throw error;
    // A crashed supervisor can leave a Unix socket behind; never kill by PID.
    await unlink(endpoint);
    await listen();
  }
  async function stop() {
    if (stopping) return;
    stopping = true;
    current = { ...current, state: "stopping" };
    if (child?.connected) child.send("stop");
    else if (!child) server.close(() => process.exit(0));
  }
  process.on("SIGINT", () => void stop());
  process.on("SIGTERM", () => void stop());
  // The startup caller can cancel, but its normal disconnect leaves us running.
  process.on("message", (message) => {
    if (message === "stop") void stop();
  });
  const logs = new DailyLogSink(logDirectory);
  logs.on("error", (error) => {
    process.send?.({ logError: error.message });
    void stop();
  });
  const days = retentionDays();
  await pruneLogs(logDirectory, days);
  if (stopping) return;
  // Cleanup still runs when the server stays up through midnight without
  // producing output. The timer does not keep a stopped supervisor alive.
  function schedulePrune() {
    const now = new Date();
    const midnight = new Date(
      now.getFullYear(),
      now.getMonth(),
      now.getDate() + 1,
    );
    const timer = setTimeout(
      () => {
        void pruneLogs(logDirectory, days)
          .catch((error) => logs.destroy(error))
          .finally(schedulePrune);
      },
      Math.max(1, midnight.getTime() - now.getTime()),
    );
    timer.unref();
  }
  schedulePrune();
  child = fork(join(root, "scripts/dev.mjs"), [], {
    cwd: root,
    windowsHide: true,
    stdio: ["ignore", "pipe", "pipe", "ipc"],
  });
  child.stdout.pipe(logs, { end: false });
  child.stderr.pipe(logs, { end: false });
  child.on("message", (message) => {
    if (message.ready && !stopping) {
      current = { state: "running", url: message.url };
      process.send?.({ ready: true, ...current });
      process.disconnect?.();
    }
  });
  child.once("close", async (code) => {
    logs.end();
    await finished(logs).catch(() => {});
    server.close(() => process.exit(code ?? 1));
  });
  child.once("error", (error) => {
    logs.write(`${error.message}\n`);
  });
}

async function start() {
  const existing = await status();
  if (existing) {
    console.log(
      `MnemoArc is already ${existing.state}.${existing.url ? ` ${existing.url}` : ""}`,
    );
    return;
  }
  const days = retentionDays();
  await mkdir(logDirectory, { recursive: true, mode: 0o700 });
  await pruneLogs(logDirectory, days);
  const logPath = dailyLogPath(logDirectory);
  console.log(
    `Starting MnemoArc (first build may take several minutes).\nDaily log: ${logPath} (keep ${days} days)`,
  );
  const child = fork(fileURLToPath(import.meta.url), ["supervise"], {
    cwd: root,
    detached: true,
    windowsHide: true,
    stdio: ["ignore", "ignore", "ignore", "ipc"],
  });
  const cancel = () => {
    if (child.connected) child.send("stop");
  };
  process.on("SIGINT", cancel);
  process.on("SIGTERM", cancel);
  await new Promise((resolve, reject) => {
    child.once("error", reject);
    child.once("exit", (code) =>
      reject(new Error(`Startup failed (exit ${code}).`)),
    );
    child.once("message", (message) => {
      if (message.logError)
        return reject(new Error(`Log write failed: ${message.logError}`));
      if (!message.ready) return;
      console.log(
        `MnemoArc is ${message.state}. ${message.url ?? ""}\nUse stop_all.sh or stop_all.bat to stop it.`,
      );
      child.unref();
      resolve();
    });
  }).catch(async (error) => {
    console.error(
      (await readFile(logPath, "utf8").catch(() => "")).slice(-6000),
    );
    throw error;
  });
  process.removeListener("SIGINT", cancel);
  process.removeListener("SIGTERM", cancel);
}

async function stop() {
  if (!(await status())) {
    console.log("MnemoArc is not running through start_all.");
    return;
  }
  await request("stop");
  console.log("Stopping MnemoArc; waiting for active work to finish...");
  for (let attempt = 0; attempt < 240; attempt++) {
    await new Promise((resolve) => setTimeout(resolve, 250));
    if (!(await status())) {
      console.log("MnemoArc stopped.");
      return;
    }
  }
  throw new Error(
    "Shutdown is still in progress. Run stop_all again to check; no forced termination was performed.",
  );
}

try {
  const [major, minor] = process.versions.node.split(".").map(Number);
  if (major < 22 || (major === 22 && minor < 12))
    throw new Error("Node.js 22.12 or newer is required.");
  const action = process.argv[2];
  if (action === "start") await start();
  else if (action === "stop") await stop();
  else if (action === "supervise") await supervise();
  else throw new Error("Usage: node scripts/services.mjs start|stop");
} catch (error) {
  console.error(error.message);
  process.exit(1);
}
