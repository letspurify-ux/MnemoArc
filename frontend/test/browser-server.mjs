// Real Rust API + deterministic local SSE provider. No external API calls or user configuration.
import { createServer } from "node:http";
import { mkdtemp, writeFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { spawn } from "node:child_process";
const dir = await mkdtemp(join(tmpdir(), "mnemoarc-browser-"));
await writeFile(join(dir, "sample.rs"), 'fn main() { println!("hello"); }\n');
const answer =
  "# 조사 결과\n\n프로젝트의 주요 흐름을 확인했습니다.\n\n| 항목 | 결과 |\n| --- | --- |\n| 진입점 | main.rs |\n\n```rust\nfn main() {}\n```\n\n수식: \\(x^2 + y^2\\)\n\n```mermaid\nflowchart LR\n A[요청] --> B[기억]\n```";
const provider = createServer(async (req, res) => {
  let body = "";
  for await (const chunk of req) body += chunk;
  const data = JSON.parse(body);
  if (data.stream === false) {
    res.setHeader("Content-Type", "application/json");
    res.end(JSON.stringify({ choices: [{ message: { content: "OK" } }] }));
    return;
  }
  res.writeHead(200, {
    "Content-Type": "text/event-stream",
    "Cache-Control": "no-cache",
  });
  const event = (value) => res.write(`data: ${JSON.stringify(value)}\n\n`);
  if (data.tool_choice) {
    event({
      choices: [
        {
          delta: {
            tool_calls: [
              {
                index: 0,
                id: "probe",
                function: {
                  name: "connection_echo",
                  arguments: '{"text":"OK"}',
                },
              },
            ],
          },
          finish_reason: "tool_calls",
        },
      ],
    });
    res.end("data: [DONE]\n\n");
    return;
  }
  const planTest = data.messages.some(
    (m) => m.role === "user" && m.content === "할 일 목록 테스트",
  );
  if (planTest) {
    const stateMessage = data.messages.at(-1).content;
    const state = JSON.parse(
      stateMessage.slice(stateMessage.indexOf("\n") + 1),
    );
    const revision = state.task.plan_revision;
    const operations = [
      [{ op: "insert", texts: ["본문 작성", "결과 검증", "불필요 작업"] }],
      [
        { op: "insert", texts: ["선행 근거 확인"], before: "T1" },
        { op: "remove", id: "T3", reason: "요청 범위 밖" },
      ],
      [{ op: "complete", id: "T4", result: "필요한 선언을 확인했습니다." }],
      [
        {
          op: "split",
          id: "T1",
          texts: ["본문 초안 작성", "본문 내용 검증"],
        },
      ],
    ][revision];
    if (!operations) {
      const keep = setInterval(() => res.write(": keepalive\n\n"), 1000);
      res.on("close", () => clearInterval(keep));
      return;
    }
    event({
      choices: [
        {
          delta: {
            tool_calls: [
              {
                index: 0,
                id: `plan-${revision}`,
                function: {
                  name: "task_plan",
                  arguments: JSON.stringify({
                    action: "apply",
                    expected_revision: revision,
                    operations,
                  }),
                },
              },
            ],
          },
          finish_reason: "tool_calls",
        },
      ],
    });
    res.end("data: [DONE]\n\n");
    return;
  }
  const continuation = data.messages.some(
    (m) => m.role === "user" && m.content === "길이 이어받기 테스트",
  );
  if (continuation) {
    const resumed = data.messages.some(
      (m) =>
        m.role === "assistant" &&
        m.content === "```mermaid\nflowchart LR\n A -->",
    );
    event({
      choices: [
        {
          delta: {
            content: resumed ? " B\n```" : "```mermaid\nflowchart LR\n A -->",
          },
          finish_reason: resumed ? "stop" : "length",
        },
      ],
    });
    event({
      choices: [],
      usage: { prompt_tokens: 100, completion_tokens: 20 },
    });
    res.end("data: [DONE]\n\n");
    return;
  }
  const slow = data.messages.some(
    (m) => m.role === "user" && m.content === "느린 요청 테스트",
  );
  if (slow) {
    await new Promise((resolve) => setTimeout(resolve, 1800));
    if (res.destroyed) return;
    event({
      choices: [
        {
          delta: { content: "천천히 조사하고 있습니다…" },
          finish_reason: null,
        },
      ],
    });
    const keep = setInterval(() => res.write(": keepalive\n\n"), 1000);
    res.on("close", () => clearInterval(keep));
    return;
  }
  for (const part of answer.match(/.{1,25}|\n/g)) {
    event({ choices: [{ delta: { content: part }, finish_reason: null }] });
    await new Promise((r) => setTimeout(r, 40));
  }
  event({ choices: [{ delta: {}, finish_reason: "stop" }] });
  event({ choices: [], usage: { prompt_tokens: 800, completion_tokens: 100 } });
  res.end("data: [DONE]\n\n");
});
await new Promise((resolve) => provider.listen(0, "127.0.0.1", resolve));
await writeFile(
  join(dir, "config.toml"),
  `base_url = "http://127.0.0.1:${provider.address().port}"\nmodel = ""\n[[projects]]\nname = "Browser fixture"\nroot = ${JSON.stringify(dir)}\noutput = "summary.md"\n`,
);
const child = spawn(
  resolve("../target/debug/mnemoarc"),
  ["--config", join(dir, "config.toml"), "web", "--no-open", "--port", "3099"],
  { stdio: "inherit", cwd: dir },
);
let stopping = false;
async function close() {
  if (stopping) return;
  stopping = true;
  child.kill("SIGINT");
  provider.closeAllConnections();
  provider.close();
  await new Promise((r) =>
    child.exitCode !== null ? r() : child.once("exit", r),
  );
  await rm(dir, { recursive: true, force: true });
  process.exit(0);
}
process.on("SIGINT", close);
process.on("SIGTERM", close);
child.on("exit", () => {
  if (!stopping) void close();
});
