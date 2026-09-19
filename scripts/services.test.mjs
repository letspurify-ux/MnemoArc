import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import {
  copyFile,
  mkdir,
  mkdtemp,
  readFile,
  rm,
  writeFile,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { promisify } from "node:util";
import { createServer } from "node:http";
import test from "node:test";

const exec = promisify(execFile);
const source = new URL("./services.mjs", import.meta.url);
async function fixture(t, dev) {
  const root = await mkdtemp(join(tmpdir(), "mnemoarc launcher test "));
  await mkdir(join(root, "scripts"));
  await copyFile(source, join(root, "scripts/services.mjs"));
  await writeFile(join(root, "scripts/dev.mjs"), dev);
  const run = (command, options = {}) =>
    exec(process.execPath, [join(root, "scripts/services.mjs"), command], {
      cwd: tmpdir(),
      timeout: 15000,
      ...options,
    });
  t.after(async () => {
    await run("stop").catch(() => {});
    await rm(root, { recursive: true, force: true });
  });
  return { root, run };
}
const fakeDev = `
import { createServer } from 'node:http';
import { appendFileSync } from 'node:fs';
appendFileSync('events', 'start\\n');
const server = createServer((req,res) => res.end('ready'));
server.listen(0, '127.0.0.1', () => process.send({ ready: true, url: 'http://127.0.0.1:' + server.address().port }));
function stop() { server.close(() => { appendFileSync('events', 'stop\\n'); process.exit(0); }); }
process.on('message', message => { if (message === 'stop') stop(); });
process.on('disconnect', stop);
`;

test("background lifecycle handles spaces, repeat start/stop, and checkout ownership", async (t) => {
  const first = await fixture(t, fakeDev);
  const second = await fixture(t, fakeDev);
  const started = await first.run("start");
  const url = started.stdout.match(/http:\/\/127\.0\.0\.1:\d+/)?.[0];
  assert.ok(url, started.stdout);
  assert.equal(await (await fetch(url)).text(), "ready");
  assert.match((await first.run("start")).stdout, /already running/);
  assert.match((await second.run("stop")).stdout, /not running/);
  assert.equal(await (await fetch(url)).text(), "ready");
  assert.equal(await readFile(join(first.root, "events"), "utf8"), "start\n");
  assert.match((await first.run("stop")).stdout, /MnemoArc stopped/);
  assert.equal(
    await readFile(join(first.root, "events"), "utf8"),
    "start\nstop\n",
  );
  await assert.rejects(fetch(url));
  assert.match((await first.run("stop")).stdout, /not running/);
  assert.match((await first.run("start")).stdout, /is running/);
  await first.run("stop");
});

test("startup errors are reported and release the launcher for a retry", async (t) => {
  const app = await fixture(
    t,
    "console.error('fixture startup failure'); process.exit(1);",
  );
  await assert.rejects(app.run("start"), (error) => {
    assert.match(error.stderr, /fixture startup failure/);
    return true;
  });
  assert.match((await app.run("stop")).stdout, /not running/);
  await writeFile(join(app.root, "scripts/dev.mjs"), fakeDev);
  assert.match((await app.run("start")).stdout, /is running/);
});

test("an occupied port is reported without terminating its owner", async (t) => {
  const server = createServer((req, res) => res.end("unrelated server"));
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  t.after(() => new Promise((resolve) => server.close(resolve)));
  const port = server.address().port;
  const dev = await readFile(new URL("./dev.mjs", import.meta.url), "utf8");
  const app = await fixture(t, dev);
  await assert.rejects(
    app.run("start", {
      env: {
        ...process.env,
        MNEMOARC_PORT: String(port),
        MNEMOARC_FRONTEND_PORT: String(port === 5173 ? 5174 : 5173),
      },
    }),
    (error) => {
      assert.match(error.stderr, /already in use/);
      return true;
    },
  );
  assert.equal(
    await (await fetch(`http://127.0.0.1:${port}`)).text(),
    "unrelated server",
  );
  assert.match((await app.run("stop")).stdout, /not running/);
});
