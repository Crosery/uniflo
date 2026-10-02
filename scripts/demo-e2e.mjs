#!/usr/bin/env bun
// End-to-end check of the web demo (examples/web/index.html) in headless Chrome against a
// throwaway daemon fed with synthetic sessions. Never touches real harness data:
// UNIFLO_HOME points at a temp dir and the index cache is disabled.
//
//   cargo build --release && bun scripts/demo-e2e.mjs
//
// Env: UNIFLO_BIN (default target/release/uniflo), CHROME (Chrome/Chromium binary),
// E2E_SHOTS (dir for screenshots, default target/demo-e2e).

import { appendFileSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { spawn } from "node:child_process";

const ROOT = new URL("..", import.meta.url).pathname;
const BIN = process.env.UNIFLO_BIN || join(ROOT, "target/release/uniflo");
const CHROME = process.env.CHROME || "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
const SHOTS = process.env.E2E_SHOTS || join(ROOT, "target/demo-e2e");
const PORT = 20000 + Math.floor(Math.random() * 20000);
const CDP_PORT = PORT + 1;
const STATIC_PORT = PORT + 2;
const TOKEN = "e2e-not-a-secret";
const LATENCY_BUDGET_MS = 300;

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const iso = (ms) => new Date(ms).toISOString();
const line = (v) => JSON.stringify(v) + "\n";
const results = [];
let failed = false;
function report(name, ok, detail = "") {
  results.push({ name, ok, detail });
  if (!ok) failed = true;
  console.log(`${ok ? "✓" : "✗"} ${name}${detail ? " · " + detail : ""}`);
}

// ---------------------------------------------------------------- synthetic data
const home = mkdtempSync(join(tmpdir(), "uniflo-e2e-home-"));
const claudeFile = join(home, ".claude/projects/-w-demo/sess-1.jsonl");
const ompFile = join(home, ".omp/agent/sessions/-w-demo/2026-10-02T12-00-00-000Z_omp1.jsonl");
mkdirSync(join(home, ".claude/projects/-w-demo"), { recursive: true });
mkdirSync(join(home, ".omp/agent/sessions/-w-demo"), { recursive: true });

const t0 = Date.now() - 2 * 3600_000;
const cu = (uuid, ts, content) =>
  line({ type: "user", uuid, timestamp: iso(ts), cwd: "/w/demo", origin: { kind: "human" }, message: { role: "user", content } });
const ca = (uuid, ts, content, stop) =>
  line({ type: "assistant", uuid, timestamp: iso(ts), message: { id: "m-" + uuid, model: "demo-model", content, stop_reason: stop } });
let claude = "";
for (let i = 0; i < 80; i++) {
  claude += cu(`u${i}`, t0 + i * 60_000, `synthetic question ${i}`);
  claude += ca(`a${i}`, t0 + i * 60_000 + 5_000, [{ type: "text", text: `synthetic answer ${i}\n\n\`\`\`rust\nfn main() {}\n\`\`\`` }], "end_turn");
}
// One rich recent turn: reasoning, several tool kinds, Markdown answer, usage.
const tr = Date.now() - 3 * 60_000;
const usage = { input_tokens: 18234, output_tokens: 912, cache_read_input_tokens: 15400, cache_creation_input_tokens: 1200 };
claude += cu("rich-u", tr, "给网关加一个 /demo 路由，直接返回内置的网页演示，并补测试。");
claude += ca("rich-t", tr + 2_000, [{ type: "thinking", thinking: "先看路由表在哪里注册，再决定用 include_str! 内嵌页面，避免运行时读文件。" }], "tool_use");
claude += ca("rich-r", tr + 3_000, [{ type: "tool_use", id: "toolu_read", name: "Read", input: { file_path: "/w/demo/crates/gateway/src/lib.rs" } }], "tool_use");
claude += cu("rich-rr", tr + 3_400, [{ type: "tool_result", tool_use_id: "toolu_read", content: "pub fn router(engine: Arc<Engine>) -> Router {\n    Router::new()\n        .route(\"/v1/health\", get(health))\n}" }]);
claude += ca("rich-e", tr + 6_000, [{ type: "tool_use", id: "toolu_edit", name: "Edit", input: { file_path: "/w/demo/crates/gateway/src/lib.rs", old_string: ".route(\"/v1/health\"", new_string: ".route(\"/demo\", get(demo))\n        .route(\"/v1/health\"" } }], "tool_use");
claude += cu("rich-er", tr + 6_300, [{ type: "tool_result", tool_use_id: "toolu_edit", content: "The file has been updated." }]);
claude += ca("rich-b", tr + 8_000, [{ type: "tool_use", id: "toolu_bash", name: "Bash", input: { command: "cargo test -p gateway", description: "Run gateway tests" } }], "tool_use");
claude += cu("rich-br", tr + 19_500, [{ type: "tool_result", tool_use_id: "toolu_bash", content: "running 6 tests\ntest demo_page_is_served ... ok\ntest result: ok. 6 passed; 0 failed" }]);
claude += line({ type: "assistant", uuid: "rich-a", timestamp: iso(tr + 21_000), message: { id: "m-rich-a", model: "demo-model", usage, stop_reason: "end_turn", content: [{ type: "text", text:
  "已加上 `/demo` 路由，页面用 `include_str!` 编译进二进制。\n\n### 改动\n\n- `router()` 注册 **GET /demo**，返回 `text/html`\n- 新增测试 `demo_page_is_served`，校验页面引用了全部 `/v1/*` 接口\n\n```rust\nasync fn demo() -> Html<&'static str> {\n    Html(include_str!(\"../../../examples/web/index.html\"))\n}\n```\n\n| 检查 | 结果 |\n|---|---|\n| cargo test | 6 passed |\n| clippy | 0 warnings |\n\n> 打开 http://127.0.0.1:7311/demo 即可查看。" }] } });
claude += line({ type: "ai-title", aiTitle: "网关新增 /demo 演示页" });
writeFileSync(claudeFile, claude);

// A sub-agent of the Claude session.
mkdirSync(join(home, ".claude/projects/-w-demo/sess-1/subagents"), { recursive: true });
writeFileSync(
  join(home, ".claude/projects/-w-demo/sess-1/subagents/agent-sub1.jsonl"),
  line({ type: "user", uuid: "s1", isSidechain: true, timestamp: iso(tr + 9_000), cwd: "/w/demo", message: { role: "user", content: "Review the gateway diff" } }) +
    line({ type: "assistant", uuid: "s2", isSidechain: true, timestamp: iso(tr + 15_000), message: { id: "m-s2", model: "demo-model", content: [{ type: "text", text: "Diff looks good." }], stop_reason: "end_turn" } }),
);

// A Codex session that is still working (turn started, not completed).
const codexDir = join(home, ".codex/sessions/2026/10/02");
mkdirSync(codexDir, { recursive: true });
const CODEX_ID = "0199a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b";
const cx = (ts, type, payload) => line({ timestamp: iso(ts), type, payload });
const tc = Date.now() - 20_000;
writeFileSync(
  join(codexDir, "rollout-2026-10-02T21-40-00-0199a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b.jsonl"),
  cx(tc, "session_meta", { id: "0199a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b", timestamp: iso(tc), cwd: "/w/web", source: "cli" }) +
    cx(tc, "turn_context", { turn_id: "t1", cwd: "/w/web", model: "gpt-demo" }) +
    cx(tc, "event_msg", { type: "task_started", turn_id: "t1" }) +
    cx(tc + 500, "response_item", { type: "message", id: "cu1", role: "user", content: [{ type: "input_text", text: "把首页的加载骨架屏改成渐变动画" }] }) +
    cx(tc + 2_000, "response_item", { type: "reasoning", id: "cr1", summary: [{ type: "summary_text", text: "Find the skeleton component first." }] }) +
    cx(tc + 3_000, "response_item", { type: "function_call", id: "cf1", name: "exec_command", arguments: JSON.stringify({ cmd: "rg -n skeleton src" }), call_id: "call_rg" }) +
    cx(tc + 3_600, "response_item", { type: "function_call_output", call_id: "call_rg", output: "src/ui/Skeleton.tsx:12: export function Skeleton()" }) +
    cx(tc + 5_000, "response_item", { type: "function_call", id: "cf2", name: "exec_command", arguments: JSON.stringify({ cmd: "pnpm test --watch=false" }), call_id: "call_test" }),
);
writeFileSync(
  ompFile,
  line({ type: "title", v: 1, title: "omp demo session", source: "user" }) +
    line({ type: "session", version: 3, id: "omp1", timestamp: iso(t0), cwd: "/w/demo" }) +
    line({ type: "message", id: "o1", timestamp: iso(t0 + 1000), message: { role: "user", content: [{ type: "text", text: "list files" }] } }) +
    line({ type: "message", id: "o2", timestamp: iso(t0 + 2000), message: { role: "assistant", content: [{ type: "toolCall", id: "c1", name: "bash", arguments: { command: "ls" } }], stopReason: "toolUse" } }) +
    line({ type: "message", id: "o3", timestamp: iso(t0 + 3000), message: { role: "toolResult", toolCallId: "c1", toolName: "bash", content: [{ type: "text", text: "a\nb" }] } }) +
    line({ type: "message", id: "o4", timestamp: iso(t0 + 4000), message: { role: "assistant", content: [{ type: "text", text: "done" }], stopReason: "stop" } }),
);

// ---------------------------------------------------------------- processes
const procs = [];
function start(cmd, args, env = {}) {
  const p = spawn(cmd, args, { env: { ...process.env, ...env }, stdio: ["ignore", "ignore", "pipe"] });
  let err = "";
  p.stderr.on("data", (d) => (err = (err + d).slice(-4000)));
  p.lastErr = () => err;
  procs.push(p);
  return p;
}
async function until(fn, ms, what) {
  const end = Date.now() + ms;
  for (;;) {
    try {
      const v = await fn();
      if (v) return v;
    } catch {}
    if (Date.now() > end) throw new Error(`timeout: ${what}`);
    await sleep(50);
  }
}

// ---------------------------------------------------------------- scenarios
async function load(url) {
  cdp.errors.length = 0;
  await cdp.send("Page.navigate", { url });
  await until(() => cdp.eval("document.readyState === 'complete'"), 10_000, "page load");
}

const allChecksOk = `(() => { const r = [...document.querySelectorAll('[data-check]')];
  return r.length >= 10 && r.every((x) => x.dataset.ok === 'true'); })()`;
const badChecks = `[...document.querySelectorAll('[data-check]')].filter((x) => x.dataset.ok !== 'true').map((x) => x.dataset.check + ':' + (x.title || 'pending'))`;
const evCount = `document.querySelectorAll('#timeline [data-id]').length`;
const rowStatus = (key) => `document.querySelector('[data-key="${key}"]')?.dataset.status`;
const hasText = (t) => `[...document.querySelectorAll('#timeline [data-id]')].some((n) => n.textContent.includes(${JSON.stringify(t)}))`;

async function runTransport(t) {
  await load(`${api}/demo?token=${TOKEN}&transport=${t}&select=claude:sess-1`);
  try {
    await until(() => cdp.eval(allChecksOk), 10_000, "self-check");
    report(`[${t}] all 10 endpoint checks pass`, true);
  } catch {
    report(`[${t}] all 10 endpoint checks pass`, false, JSON.stringify(await cdp.eval(badChecks)));
  }
  const rows = await cdp.eval(`[...document.querySelectorAll('[data-key]')].map((n) => n.dataset.key + '=' + n.dataset.status)`);
  const codexWork = rows.includes(`codex:${CODEX_ID}=work`);
  const allHarnesses = ["claude:sess-1=", "omp:omp1=", "codex:"].every((k) => rows.some((r) => r.startsWith(k)));
  const sub = rows.filter((r) => r.startsWith("claude:")).length >= 2;
  report(`[${t}] list: claude + sub-agent + omp + codex, codex working`, allHarnesses && sub && codexWork, rows.join(" "));
  if (t === "sse") await showcase();
  await until(() => cdp.eval(`${evCount} >= 150`), 5_000, "first page").catch(() => {});
  const first = await cdp.eval(evCount);
  report(`[${t}] transcript first page rendered`, first >= 150, `${first} events`);
  await cdp.eval(`document.querySelector('#more')?.click()`);
  await until(() => cdp.eval(`${evCount} > ${first}`), 5_000, "older page").catch(() => {});
  const all = await cdp.eval(evCount);
  report(`[${t}] "load earlier" pages history`, all > first, `${first} → ${all}`);

  // Live append → DOM, timed inside the page.
  const tag = `e2e-live-${t}-${Date.now()}`;
  await cdp.eval(`window.__seen = 0; new MutationObserver(() => { if (!window.__seen && ${hasText(tag)}) window.__seen = Date.now(); })
    .observe(document.querySelector('#timeline'), { childList: true, subtree: true, characterData: true }); true`);
  const tAppend = Date.now();
  appendFileSync(claudeFile, cu(`live-${t}`, tAppend, tag));
  const seen = await until(() => cdp.eval("window.__seen"), 5_000, "live event").catch(() => 0);
  const latency = seen ? seen - tAppend : -1;
  report(`[${t}] appended message appears live`, seen > 0 && latency < LATENCY_BUDGET_MS, `${latency} ms`);
  const work = await until(() => cdp.eval(`${rowStatus("claude:sess-1")} === 'work'`), 3_000, "work").catch(() => false);
  report(`[${t}] session flips to work`, !!work);

  appendFileSync(
    claudeFile,
    ca(`tool-${t}`, Date.now(), [{ type: "tool_use", id: `toolu_${t}`, name: "e2e_tool", input: { path: "/w/demo" } }], "tool_use") +
      cu(`res-${t}`, Date.now(), [{ type: "tool_result", tool_use_id: `toolu_${t}`, content: "tool output ok" }]),
  );
  const tool = await until(() => cdp.eval(`${hasText("e2e_tool")} && ${hasText("tool output ok")}`), 3_000, "tool").catch(() => false);
  report(`[${t}] tool call + result stream in`, !!tool);

  appendFileSync(claudeFile, ca(`end-${t}`, Date.now(), [{ type: "text", text: "all done" }], "end_turn"));
  const idle = await until(() => cdp.eval(`${rowStatus("claude:sess-1")} === 'idle'`), 3_000, "idle").catch(() => false);
  report(`[${t}] turn end flips to idle`, !!idle);

  const shot = await cdp.send("Page.captureScreenshot", { format: "png" });
  writeFileSync(join(SHOTS, `${t}.png`), Buffer.from(shot.data, "base64"));
  report(`[${t}] no page errors`, cdp.errors.length === 0, cdp.errors.slice(0, 3).join(" | "));
}

// Rich rendering paths + screenshots for the docs (synthetic data only).
async function showcase() {
  const rich = await until(() => cdp.eval(`!!(document.querySelector('#timeline .md table') && document.querySelector('#timeline .code')
    && document.querySelectorAll('#timeline .tool').length >= 3 && document.querySelector('#timeline .think'))`), 5_000, "rich").catch(() => false);
  report("[sse] markdown, code block, reasoning and paired tool cards render", !!rich);
  const paired = await cdp.eval(`[...document.querySelectorAll('#timeline .tool')].filter((n) => n.querySelector('[data-kind=tool_result]')).length`);
  report("[sse] tool results fold into their call cards", paired >= 3, `${paired} paired`);
  const shot = async (name) => writeFileSync(join(SHOTS, name), Buffer.from((await cdp.send("Page.captureScreenshot", { format: "png" })).data, "base64"));
  const scheme = (value) => cdp.send("Emulation.setEmulatedMedia", { features: [{ name: "prefers-color-scheme", value }] });
  await scheme("dark");
  await cdp.eval(`[...document.querySelectorAll('.tool > summary')].find((s) => s.textContent.includes('cargo test'))?.click();
    [...document.querySelectorAll('#timeline [data-kind=user_message]')].find((n) => n.textContent.includes('/demo'))?.scrollIntoView({ block: 'start' });
    document.querySelector('#timeline').scrollTop -= 14; true`);
  await sleep(400);
  await shot("showcase-dark.png");
  await scheme("light");
  await sleep(300);
  await shot("showcase-light.png");
  await scheme("dark");
  await cdp.eval(`document.querySelector('[data-key^="codex:"]').click(); true`);
  await until(() => cdp.eval(`!!document.querySelector('#timeline .tool .spin')`), 5_000, "running tool").catch(() => {});
  await sleep(400);
  await shot("working-dark.png");
  const running = await cdp.eval(`!!document.querySelector('#timeline .tool .spin') && document.querySelector('#detail-head .badge.work') !== null`);
  report("[sse] working session shows running tool + work badge", running);
  await cdp.send("Emulation.setEmulatedMedia", { features: [] });
  await cdp.eval(`document.querySelector('[data-key="claude:sess-1"]').click(); true`);
}

async function runCrossOrigin() {
  await load(`http://127.0.0.1:${STATIC_PORT}/?api=${encodeURIComponent(api)}&token=${TOKEN}&transport=ws&select=omp:omp1`);
  try {
    await until(() => cdp.eval(allChecksOk), 10_000, "cross-origin self-check");
    report("[cross-origin] page on another loopback origin passes all checks", true);
  } catch {
    report("[cross-origin] page on another loopback origin passes all checks", false, JSON.stringify(await cdp.eval(badChecks)));
  }
  const omp = await until(() => cdp.eval(`${hasText("list files")} && ${hasText("bash")}`), 3_000, "omp transcript").catch(() => false);
  report("[cross-origin] omp transcript rendered", !!omp);
  const shot = await cdp.send("Page.captureScreenshot", { format: "png" });
  writeFileSync(join(SHOTS, "cross-origin.png"), Buffer.from(shot.data, "base64"));
  report("[cross-origin] no page errors", cdp.errors.length === 0, cdp.errors.slice(0, 3).join(" | "));
}

// ---------------------------------------------------------------- minimal CDP client
async function connectCdp(url) {
  const ws = new WebSocket(url);
  await new Promise((ok, bad) => { ws.onopen = ok; ws.onerror = bad; });
  let id = 0;
  const pending = new Map();
  const errors = [];
  ws.onmessage = (m) => {
    const msg = JSON.parse(m.data);
    if (msg.id && pending.has(msg.id)) {
      const { ok, bad } = pending.get(msg.id);
      pending.delete(msg.id);
      msg.error ? bad(new Error(msg.error.message)) : ok(msg.result);
    } else if (msg.method === "Runtime.exceptionThrown") {
      errors.push(msg.params.exceptionDetails.exception?.description || msg.params.exceptionDetails.text);
    } else if (msg.method === "Runtime.consoleAPICalled" && msg.params.type === "error") {
      errors.push(msg.params.args.map((a) => a.value ?? a.description).join(" "));
    } else if (msg.method === "Log.entryAdded" && msg.params.entry.level === "error") {
      errors.push(`${msg.params.entry.source}: ${msg.params.entry.text} ${msg.params.entry.url || ""}`);
    }
  };
  const send = (method, params = {}) =>
    new Promise((ok, bad) => {
      pending.set(++id, { ok, bad });
      ws.send(JSON.stringify({ id, method, params }));
    });
  const evaluate = async (expression) => {
    const r = await send("Runtime.evaluate", { expression, returnByValue: true, awaitPromise: true });
    if (r.exceptionDetails) throw new Error(r.exceptionDetails.text);
    return r.result.value;
  };
  return { send, eval: evaluate, errors, close: () => ws.close() };
}

// ---------------------------------------------------------------- run
let daemon;
let cdp;
const api = `http://127.0.0.1:${PORT}`;

async function main() {
daemon = start(BIN, ["daemon", "--bind", `127.0.0.1:${PORT}`, "--no-cache", "--token", TOKEN], { UNIFLO_HOME: home });
const profile = mkdtempSync(join(tmpdir(), "uniflo-e2e-chrome-"));
try {
  await until(async () => (await fetch(`${api}/v1/health?token=${TOKEN}`)).ok, 15_000, "daemon health");
  report("daemon without token answers 401", (await fetch(`${api}/v1/health`)).status === 401);

  // Same page served from a different loopback origin: the third-party integration path (CORS).
  const html = readFileSync(join(ROOT, "examples/web/index.html"));
  Bun.serve({ port: STATIC_PORT, hostname: "127.0.0.1", fetch: () => new Response(html, { headers: { "content-type": "text/html; charset=utf-8" } }) });

  start(CHROME, [
    "--headless=new", `--remote-debugging-port=${CDP_PORT}`, `--user-data-dir=${profile}`, "--no-first-run",
    "--no-default-browser-check", "--disable-gpu", "--window-size=1480,920", "about:blank",
  ]);
  const wsUrl = await until(async () => {
    const list = await (await fetch(`http://127.0.0.1:${CDP_PORT}/json/list`)).json();
    return list.find((t) => t.type === "page")?.webSocketDebuggerUrl;
  }, 15_000, "chrome devtools");
  cdp = await connectCdp(wsUrl);
  await cdp.send("Runtime.enable");
  await cdp.send("Log.enable");
  await cdp.send("Page.enable");
  await cdp.send("Emulation.setDeviceMetricsOverride", { width: 1480, height: 920, deviceScaleFactor: 2, mobile: false });

  mkdirSync(SHOTS, { recursive: true });
  for (const transport of ["sse", "ws", "ndjson"]) await runTransport(transport);
  await runCrossOrigin();
} catch (e) {
  report("harness", false, String(e.message || e) + (daemon.lastErr() ? ` · daemon: ${daemon.lastErr().trim().slice(-300)}` : ""));
} finally {
  try { cdp?.close(); } catch {}
  for (const p of procs) p.kill("SIGTERM");
  await sleep(300);
  rmSync(home, { recursive: true, force: true });
  rmSync(profile, { recursive: true, force: true });
  console.log(JSON.stringify({ ok: !failed, passed: results.filter((r) => r.ok).length, failed: results.filter((r) => !r.ok).length }));
  process.exit(failed ? 1 : 0);
}
}

await main();
