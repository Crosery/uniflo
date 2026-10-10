#!/usr/bin/env bun
// End-to-end check of the web demo (examples/web/index.html) in headless Chrome against
// throwaway daemons fed with synthetic sessions (every harness, 60 days of usage). Never touches
// real harness data: UNIFLO_HOME points at temp dirs, cleanup moves files into a temp
// UNIFLO_TRASH_DIR, the index cache is disabled, notifications and open-terminal are stubbed.
//
//   cargo build --release && CHROME=<chrome-headless-shell> bun scripts/demo-e2e.mjs
//
// Env: UNIFLO_BIN, CHROME, E2E_SHOTS (default target/demo-e2e; visual matrix in visual/), E2E_ONLY.

import { appendFileSync, existsSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { spawn } from "node:child_process";
import { writeAllHarnesses } from "./e2e-fixtures.mjs";
const ROOT = new URL("..", import.meta.url).pathname;
const BIN = process.env.UNIFLO_BIN || join(ROOT, "target/release/uniflo");
const CHROME = process.env.CHROME || "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
const SHOTS = process.env.E2E_SHOTS || join(ROOT, "target/demo-e2e");
const PORT = 20000 + Math.floor(Math.random() * 20000);
const CDP_PORT = PORT + 1;
const STATIC_PORT = PORT + 2;
const RO_PORT = PORT + 3;
const TOKEN = "e2e-not-a-secret";
const LATENCY_BUDGET_MS = 300;

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const iso = (ms) => new Date(ms).toISOString();
const line = (v) => JSON.stringify(v) + "\n";
const enc = encodeURIComponent;
const results = [];
let failed = false;
function report(name, ok, detail = "") {
  results.push({ name, ok, detail });
  if (!ok) failed = true;
  console.log(`${ok ? "✓" : "✗"} ${name}${detail ? " · " + detail : ""}`);
}

// ---------------------------------------------------------------- synthetic data
const home = mkdtempSync(join(tmpdir(), "uniflo-e2e-home-"));
const roHome = mkdtempSync(join(tmpdir(), "uniflo-e2e-ro-"));
const trash = mkdtempSync(join(tmpdir(), "uniflo-e2e-trash-"));
const claudeFile = join(home, ".claude/projects/-w-demo/sess-1.jsonl");
const ompFile = join(home, ".omp/agent/sessions/-w-demo/2026-10-02T12-00-00-000Z_omp1.jsonl");
mkdirSync(join(home, ".claude/projects/-w-demo"), { recursive: true });
mkdirSync(join(home, ".omp/agent/sessions/-w-demo"), { recursive: true });

const t0 = Date.now() - 2 * 3600_000;
const cu = (uuid, ts, content, cwd = "/w/demo") =>
  line({ type: "user", uuid, timestamp: iso(ts), cwd, origin: { kind: "human" }, message: { role: "user", content } });
const ca = (uuid, ts, content, stop, model = "demo-model", usage) =>
  line({ type: "assistant", uuid, timestamp: iso(ts), message: { id: "m-" + uuid, model, content, stop_reason: stop, ...(usage ? { usage } : {}) } });
const cuse = (u) => ({ input_tokens: u[0], output_tokens: u[1], cache_read_input_tokens: u[2], cache_creation_input_tokens: u[3] });
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
claude += line({ type: "assistant", uuid: "rich-a", timestamp: iso(tr + 21_000), message: { id: "m-rich-a", model: "claude-sonnet-4-5", usage, stop_reason: "end_turn", content: [{ type: "text", text:
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

// Every supported harness once (idle, ~400 days old, so it stays out of the usage windows).
const FIXTURES = writeAllHarnesses(home, Date.now() - 400 * 86400_000);

// Sixty days of usage across projects (cwds below /w) and models: two priced Claude models, a
// priced Codex model, and `demo-model`, which no catalog prices.
const DAY = 86400_000;
const CWDS = ["/w/alpha/api/v1", "/w/alpha/api", "/w/alpha/web", "/w/beta", "/w/gamma"];
const writeClaude = (dir, id, cwd, title, turns) => {
  mkdirSync(join(home, ".claude/projects", dir), { recursive: true });
  let s = "";
  turns.forEach((t, i) => {
    s += cu(`${id}-u${i}`, t.ts, t.prompt, cwd);
    t.steps.forEach((st, j) => (s += ca(`${id}-a${i}-${j}`, t.ts + 2_000 + j * 1_500, [{ type: "text", text: st.text || "完成了这一步。" }], j === t.steps.length - 1 ? "end_turn" : "tool_use", st.model, cuse(st.u))));
  });
  if (title) s += line({ type: "ai-title", aiTitle: title });
  writeFileSync(join(home, ".claude/projects", dir, `${id}.jsonl`), s);
};
const now0 = Date.now();
for (let d = 0; d < 60; d++) {
  if (d % 6 === 5) continue;
  const cwd = CWDS[d % CWDS.length], name = cwd.split("/").slice(2).join("/");
  const model = d % 4 === 0 ? "claude-opus-4-5" : d % 7 === 3 ? "demo-model" : "claude-sonnet-4-5";
  const ts = now0 - d * DAY - ((d * 37) % 9) * 3600_000 - 600_000;
  const turns = Array.from({ length: d % 3 === 0 ? 2 : 1 }, (_, i) => ({
    ts: ts + i * 600_000,
    prompt: `继续 ${name} 的分页逻辑重构，第 ${d}-${i} 步`,
    steps: Array.from({ length: 1 + ((d + i) % 3) }, (_, j) => ({ model, u: [1200 + d * 37 + j * 90, 240 + d * 11, 6000 + d * 101 + j * 2000, d % 3 ? 0 : 900] })),
  }));
  writeClaude("-w-proj", `proj-${d}`, cwd, `${name} 分页重构 #${d}`, turns);
  if (d % 4 === 1) {
    const id = `0199c0de-0000-7000-8000-${String(d).padStart(12, "0")}`, t = now0 - d * DAY - 3 * 3600_000;
    const dt = new Date(t), dir = join(home, ".codex/sessions", String(dt.getUTCFullYear()), String(dt.getUTCMonth() + 1).padStart(2, "0"), String(dt.getUTCDate()).padStart(2, "0"));
    mkdirSync(dir, { recursive: true });
    const cwdx = d % 8 === 1 ? "/w/beta" : "/w/gamma";
    let x = cx(t, "session_meta", { id, timestamp: iso(t), cwd: cwdx, source: "cli" }) + cx(t, "turn_context", { turn_id: "t1", cwd: cwdx, model: "gpt-5" });
    x += cx(t, "event_msg", { type: "task_started", turn_id: "t1" });
    x += cx(t + 300, "response_item", { type: "message", id: `u-${d}`, role: "user", content: [{ type: "input_text", text: `为 ${cwdx} 补分页逻辑的集成测试` }] });
    x += cx(t + 900, "token_usage_record", { response_id: `r-${d}`, usage: { input_tokens: 9000 + d * 50, cached_input_tokens: 4000, output_tokens: 700 + d * 9, reasoning_output_tokens: 200, total_tokens: 9700 + d * 59 } });
    x += cx(t + 1_200, "response_item", { type: "message", id: `a-${d}`, role: "assistant", content: [{ type: "output_text", text: "测试已补上。" }] });
    x += cx(t + 1_500, "event_msg", { type: "task_complete", turn_id: "t1" });
    writeFileSync(join(dir, `rollout-${iso(t).slice(0, 19).replace(/:/g, "-")}-${id}.jsonl`), x);
  }
}
// Full-text target: a unique phrase in the middle of a long session.
const SEARCH_WORD = "量子退火调度器";
writeClaude("-w-search", "search-target", "/w/search", "检索目标会话", Array.from({ length: 30 }, (_, i) => ({
  ts: now0 - 5 * DAY + i * 120_000,
  prompt: i === 12 ? `请把 ${SEARCH_WORD} 的超时改成可配置，并保留 frob_quux_42 的旧行为` : `第 ${i} 轮：继续整理检索目标会话`,
  steps: [{ model: "claude-sonnet-4-5", u: [800, 120, 3000, 0], text: `第 ${i} 轮已处理。` }],
})));
// Cleanup: one idle session that can be cleaned and one still waiting for its reply (working).
writeClaude("-w-clean", "cleanme", "/w/clean", "可以清理的旧会话", Array.from({ length: 12 }, (_, i) => ({
  ts: now0 - 9 * DAY + i * 60_000, prompt: `旧会话第 ${i} 轮：整理日志输出`.repeat(4), steps: [{ model: "claude-sonnet-4-5", u: [500, 80, 0, 0], text: "整理好了。".repeat(20) }],
})));
writeFileSync(join(home, ".claude/projects/-w-clean/busy.jsonl"), cu("busy-u", now0 - 15_000, "正在跑的任务：迁移数据库", "/w/clean") + line({ type: "ai-title", aiTitle: "运行中的会话" }));
// Turn-end notifications: two more idle sessions that the test flips work → idle.
for (const id of ["notify2", "notify3"]) writeClaude("-w-notify", id, "/w/notify", `通知测试 ${id}`, [{ ts: now0 - 3_600_000, prompt: "准备好了吗", steps: [{ model: "claude-sonnet-4-5", u: [100, 10, 0, 0] }] }]);
// Read-only daemon: its own small home.
mkdirSync(join(roHome, ".claude/projects/-w-ro"), { recursive: true });
writeFileSync(join(roHome, ".claude/projects/-w-ro/ro-1.jsonl"),
  cu("r1", now0 - DAY, "只读模式下的会话", "/w/ro") + ca("r2", now0 - DAY + 2000, [{ type: "text", text: "好的。" }], "end_turn", "claude-sonnet-4-5", cuse([400, 50, 0, 0])));

// ---------------------------------------------------------------- processes
const procs = [];
// Adapter location overrides must not point the test daemons at real data.
const SCRUB = ["CLAUDE_CONFIG_DIR", "CODEX_HOME", "KIMI_CODE_HOME", "COPILOT_HOME", "OPENCLAW_STATE_DIR", "CODEBUDDY_CONFIG_DIR", "XDG_DATA_HOME", "XDG_CONFIG_HOME",
  "UNIFLO_DATA_DIR", "UNIFLO_CONFIG_DIR", "UNIFLO_CACHE_DIR", "UNIFLO_TRASH_DIR"];
function start(cmd, args, env = {}) {
  const e = { ...process.env, ...env };
  if (env.UNIFLO_HOME) for (const k of SCRUB) if (!(k in env)) delete e[k];
  const p = spawn(cmd, args, { env: e, stdio: ["ignore", "ignore", "pipe"] });
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
const apiJson = async (base, path) => {
  const r = await fetch(`${base}${path}${path.includes("?") ? "&" : "?"}token=${TOKEN}`);
  return r.json();
};

// ---------------------------------------------------------------- scenarios
async function load(url) {
  cdp.errors.length = 0;
  await cdp.send("Page.navigate", { url });
  await until(() => cdp.eval("document.readyState === 'complete'"), 10_000, "page load");
}
const waitFor = (expr, ms = 8_000, what = expr) => until(() => cdp.eval(expr), ms, what);
const click = (sel) => cdp.eval(`(() => { const n = document.querySelector(${JSON.stringify(sel)}); if (!n) return false; n.click(); return true; })()`);
const shot = async (name) => writeFileSync(join(SHOTS, name), Buffer.from((await cdp.send("Page.captureScreenshot", { format: "png" })).data, "base64"));
const scheme = (value) => cdp.send("Emulation.setEmulatedMedia", { features: value ? [{ name: "prefers-color-scheme", value }] : [] });
const viewport = (width, height, mobile = false) => cdp.send("Emulation.setDeviceMetricsOverride", { width, height, deviceScaleFactor: 2, mobile });

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
    report(`[${t}] all endpoint checks pass`, true, `${await cdp.eval(`document.querySelectorAll('[data-check]').length`)} checks`);
  } catch {
    report(`[${t}] all endpoint checks pass`, false, JSON.stringify(await cdp.eval(badChecks)));
  }
  const rows = await cdp.eval(`[...document.querySelectorAll('[data-key]')].map((n) => n.dataset.key + '=' + n.dataset.status)`);
  const codexWork = rows.includes(`codex:${CODEX_ID}=work`);
  const allHarnesses = ["claude:sess-1=", "omp:omp1=", "codex:"].every((k) => rows.some((r) => r.startsWith(k)));
  const sub = rows.filter((r) => r.startsWith("claude:")).length >= 2;
  report(`[${t}] list: claude + sub-agent + omp + codex, codex working`, allHarnesses && sub && codexWork, `${rows.length} rows`);
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

  await shot(`${t}.png`);
  report(`[${t}] no page errors`, cdp.errors.length === 0, cdp.errors.slice(0, 3).join(" | "));
}

// Rich rendering paths + screenshots for the docs (synthetic data only).
async function showcase() {
  const rich = await until(() => cdp.eval(`!!(document.querySelector('#timeline .md table') && document.querySelector('#timeline .code')
    && document.querySelectorAll('#timeline .tool').length >= 3 && document.querySelector('#timeline .think'))`), 5_000, "rich").catch(() => false);
  report("[sse] markdown, code block, reasoning and paired tool cards render", !!rich);
  const paired = await cdp.eval(`[...document.querySelectorAll('#timeline .tool')].filter((n) => n.querySelector('[data-kind=tool_result]')).length`);
  report("[sse] tool results fold into their call cards", paired >= 3, `${paired} paired`);
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
  await cdp.eval(`document.querySelector('[data-key^="codex:${CODEX_ID}"]').click(); true`);
  await until(() => cdp.eval(`!!document.querySelector('#timeline .tool .spin')`), 5_000, "running tool").catch(() => {});
  await sleep(400);
  await shot("working-dark.png");
  const running = await cdp.eval(`!!document.querySelector('#timeline .tool .spin') && document.querySelector('#detail-head .badge.work') !== null`);
  report("[sse] working session shows running tool + work badge", running);
  await scheme(null);
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
  await shot("cross-origin.png");
  report("[cross-origin] no page errors", cdp.errors.length === 0, cdp.errors.slice(0, 3).join(" | "));
}

// The two copies of the page are one file, and the daemon under test serves exactly it.
async function runIdentity() {
  const page = readFileSync(join(ROOT, "examples/web/index.html"));
  const copy = readFileSync(join(ROOT, "crates/uniflo-gateway/src/index.html"));
  report("[page] examples/web/index.html equals the gateway's embedded copy", page.equals(copy), `${page.length} bytes`);
  const served = Buffer.from(await (await fetch(`${api}/demo?token=${TOKEN}`)).arrayBuffer());
  report("[page] daemon /demo serves that page", served.equals(page), served.equals(page) ? "" : "rebuild the release binary");
}

async function runIcons() {
  const hs = await apiJson(api, "/v1/harnesses");
  const claudeH = hs.find((h) => h.id === "claude");
  const svg = await fetch(`${api}${claudeH?.icon}?token=${TOKEN}`);
  const body = await svg.text();
  report("[icons] /v1/harnesses carries icon; icon.svg is image/svg+xml", claudeH?.icon === "/v1/harnesses/claude/icon.svg" && svg.headers.get("content-type") === "image/svg+xml" && body.startsWith("<svg"),
    `${hs.filter((h) => h.icon).length} with icon, ${hs.filter((h) => !h.icon).length} without`);
  const missing = Object.keys(FIXTURES).filter((id) => !hs.some((h) => h.id === id && h.sessions > 0));
  report("[icons] synthetic data covers every supported harness", hs.length >= 33 && !missing.length, missing.join(",") || `${hs.length} harnesses`);
  await load(`${api}/demo?token=${TOKEN}`);
  await waitFor(`document.querySelectorAll('.row[data-key]').length >= ${Object.keys(FIXTURES).length}`, 10_000, "rows");
  const spec = JSON.stringify(hs.map((h) => [h.id, !!h.icon]));
  const check = `(() => ${spec}.map(([id, has]) => {
    const row = document.querySelector('.row[data-key^="' + id + ':"]');
    if (!row) return id + ':missing';
    const use = row.querySelector('.ricon svg.hi use'), letters = row.querySelector('.ricon .hi.hl');
    if (has) return use && use.getAttribute('href') === '#hi-' + id ? '' : id + ':no-icon';
    return letters && /^[A-Za-z]{2}$/.test(letters.textContent) ? '' : id + ':no-letters';
  }).filter(Boolean))()`;
  const bad = await cdp.eval(check);
  report("[icons] every row shows its harness icon, letter block when there is none", bad.length === 0, bad.join(" ") || `${hs.filter((h) => h.icon).length} icons + ${hs.filter((h) => !h.icon).length} letter blocks`);
  for (const theme of ["dark", "light"]) {
    await scheme(theme);
    await sleep(150);
    const low = await cdp.eval(`[...document.querySelectorAll('.ricon .hi')].map((n) => [n.closest('.row').dataset.key, __e2e.contrastOf(n, n.classList.contains('hl') ? null : n.closest('.row'))]).filter(([, c]) => c < 3)`);
    report(`[icons] icons stay visible in ${theme} theme (≥ 3:1 against the row)`, low.length === 0, low.slice(0, 4).map(([k, c]) => `${k} ${c.toFixed(2)}`).join(" "));
    await shot(`icons-${theme}.png`);
  }
  await scheme(null);
}

const usageMatches = (domRows, apiRows, keys) => {
  const byKey = new Map(apiRows.map((r) => [r.key, r]));
  const bad = [];
  for (const [key, vals] of domRows) {
    const r = byKey.get(key);
    if (!r) { bad.push(`${key}:not-in-api`); continue; }
    for (const k of keys) {
      const want = k === "cost_usd" ? (r.cost_usd ?? "") : r[k];
      const got = k === "cost_usd" ? (vals[k] === "" ? "" : Number(vals[k])) : Number(vals[k]);
      if (k === "cost_usd" && want !== "" && Math.abs(got - want) > 1e-9) bad.push(`${key}.${k}`);
      else if (k !== "cost_usd" && got !== want) bad.push(`${key}.${k}:${got}≠${want}`);
    }
  }
  if (domRows.length !== apiRows.length) bad.push(`rows ${domRows.length}≠${apiRows.length}`);
  return bad;
};
const METRIC_KEYS = ["sessions", "prompts", "steps", "input", "output", "cache_read", "cache_write", "cost_usd", "unpriced_steps"];
const domTable = `[...document.querySelectorAll('#u-table tbody tr[data-depth="0"]')].map((tr) => [tr.dataset.key, Object.fromEntries([...tr.querySelectorAll('td[data-k]')].map((td) => [td.dataset.k, td.dataset.v]))])`;
const domKpi = `Object.fromEntries([...document.querySelectorAll('#u-kpis [data-k]')].map((n) => [n.dataset.k, n.dataset.v]))`;
async function usageAgrees(label) {
  const q = await cdp.eval(`document.querySelector('#v-usage').dataset.query`);
  const kq = await cdp.eval(`document.querySelector('#v-usage').dataset.kpiQuery`);
  const [table, kpi] = await Promise.all([apiJson(api, `/v1/usage?${q}`), apiJson(api, `/v1/usage?${kq}`)]);
  const bad = usageMatches(await cdp.eval(domTable), table.rows, METRIC_KEYS);
  const foot = await cdp.eval(`Object.fromEntries([...document.querySelectorAll('#u-table tfoot td[data-k]')].map((td) => [td.dataset.k, td.dataset.v]))`);
  bad.push(...usageMatches([["total", foot]], [table.totals], METRIC_KEYS).filter((x) => !x.startsWith("rows")));
  const k = await cdp.eval(domKpi);
  for (const f of ["input", "output", "cache_read", "cache_write", "steps", "sessions", "unpriced_steps"]) if (Number(k[f]) !== kpi.totals[f]) bad.push(`kpi.${f}`);
  if (Math.abs(Number(k.cost_usd) - kpi.totals.cost_usd) > 1e-9) bad.push("kpi.cost");
  report(`[usage] ${label}: KPI and table equal /v1/usage`, bad.length === 0, bad.slice(0, 6).join(" ") || `${table.rows.length} rows · ${q}`);
  return { q, table };
}
async function runUsage() {
  await load(`${api}/demo?token=${TOKEN}&view=usage`);
  await waitFor(`!!document.querySelector('#v-usage').dataset.query && !document.querySelector('#v-usage').hidden`, 10_000, "usage view");
  const today = new Date(), d7 = new Date(today.getFullYear(), today.getMonth(), today.getDate() - 6);
  const since7 = `${d7.getFullYear()}-${String(d7.getMonth() + 1).padStart(2, "0")}-${String(d7.getDate()).padStart(2, "0")}`;
  await click('#u-range [data-range="7d"]');
  await waitFor(`document.querySelector('#v-usage').dataset.query.includes('since=${since7}')`, 5_000, "7d");
  await click('#u-group [data-group="model"]');
  await waitFor(`document.querySelector('#v-usage').dataset.query.includes('group_by=model')`, 5_000, "model group");
  await usageAgrees("7d by model");
  const unpricedBtn = await click("#u-unpriced-btn");
  const models = unpricedBtn ? await waitFor(`!document.querySelector('#u-unpriced').hidden && [...document.querySelectorAll('#u-unpriced-list [data-model]')].map((n) => n.dataset.model)`, 3_000, "unpriced list").catch(() => []) : [];
  const kq = await cdp.eval(`document.querySelector('#v-usage').dataset.kpiQuery`);
  const want = (await apiJson(api, `/v1/usage?${kq}`)).rows.filter((r) => r.unpriced_steps > 0).map((r) => r.key).sort();
  report("[usage] unpriced steps expand into the unpriced model list", unpricedBtn && JSON.stringify([...models].sort()) === JSON.stringify(want) && want.includes("demo-model"), models.join(","));
  const note = await cdp.eval(`document.querySelector('#u-kpis [data-k=cost_usd]').textContent.includes('API 等价成本') && [...document.querySelectorAll('#u-table th')].some((th) => th.textContent.includes('API 等价'))`);
  report('[usage] costs are labelled "API 等价成本"', note);
  await click('#u-group [data-group="dir"]');
  await waitFor(`document.querySelector('#v-usage').dataset.query.includes('group_by=dir') && !!document.querySelector('#u-table [data-under="/w/alpha"]')`, 5_000, "dir group");
  await click('#u-table [data-under="/w/alpha"]');
  await waitFor(`document.querySelector('#v-usage').dataset.query.includes('under=%2Fw%2Falpha&') && !!document.querySelector('#u-table [data-under="/w/alpha/api"]')`, 5_000, "drill 1");
  await click('#u-table [data-under="/w/alpha/api"]');
  await waitFor(`document.querySelector('#v-usage').dataset.query.includes('under=%2Fw%2Falpha%2Fapi&')`, 5_000, "drill 2");
  const crumbs = await cdp.eval(`[...document.querySelectorAll('#u-crumbs button')].map((b) => b.dataset.under + '|' + (b.getAttribute('aria-current') || ''))`);
  report("[usage] directory drill-down two levels shows the breadcrumb", JSON.stringify(crumbs) === JSON.stringify(["|", "/w/alpha|", "/w/alpha/api|page"]), crumbs.join(" › "));
  await click('#u-table [data-sort="cost_usd"]');
  await waitFor(`location.search.includes('sort=cost_usd') && location.search.includes('asc=1')`, 3_000, "sort");
  const order = await cdp.eval(`[...document.querySelectorAll('#u-table tbody tr[data-depth="0"] td[data-k=cost_usd]')].map((td) => td.dataset.v === '' ? -1 : Number(td.dataset.v))`);
  report("[usage] cost column sorts ascending", order.length >= 2 && order.every((v, i) => !i || order[i - 1] <= v), order.join(" ≤ "));
  const before = { url: await cdp.eval("location.search"), keys: await cdp.eval(`[...document.querySelectorAll('#u-table tbody tr')].map((tr) => tr.dataset.key).join()`) };
  await cdp.send("Page.reload");
  await waitFor(`document.readyState === 'complete' && (document.querySelector('#v-usage').dataset.query || '').includes('group_by=dir')`, 10_000, "reload");
  const after = await cdp.eval(`({ url: location.search, keys: [...document.querySelectorAll('#u-table tbody tr')].map((tr) => tr.dataset.key).join(),
    visible: !document.querySelector('#v-usage').hidden, range: document.querySelector('#u-range [aria-pressed=true]')?.dataset.range,
    group: document.querySelector('#u-group [aria-pressed=true]')?.dataset.group, crumbs: [...document.querySelectorAll('#u-crumbs button')].map((b) => b.dataset.under).join(),
    sort: document.querySelector('#u-table th[aria-sort]')?.getAttribute('aria-sort') + ':' + document.querySelector('#u-table th[aria-sort] button')?.dataset.sort })`);
  const restored = after.url === before.url && after.keys === before.keys && after.visible && after.range === "7d" && after.group === "dir"
    && after.crumbs === ",/w/alpha,/w/alpha/api" && after.sort === "ascending:cost_usd";
  report("[usage] refresh restores view, range, grouping, drill-down and sort", restored, after.url);
  await usageAgrees("after refresh, dir /w/alpha/api");
  await shot("usage.png");
  report("[usage] no page errors", cdp.errors.length === 0, cdp.errors.slice(0, 3).join(" | "));
}

async function runDetail() {
  const key = "claude:proj-0";
  await cdp.send("Browser.grantPermissions", { permissions: ["clipboardReadWrite", "clipboardSanitizedWrite"], origin: api }).catch(() => {});
  await load(`${api}/demo?token=${TOKEN}&select=${enc(key)}`);
  await waitFor(`!!document.querySelector('#steps-toggle') && !!document.querySelector('#ctx-bar')`, 8_000, "detail head");
  await click("#steps-toggle");
  const detail = await apiJson(api, `/v1/sessions/${enc(key)}/usage`);
  await waitFor(`document.querySelectorAll('#steps-table tbody tr').length === ${detail.steps.length}`, 5_000, "steps table");
  const dom = await cdp.eval(`[...document.querySelectorAll('#steps-table tbody tr')].map((tr) => [tr.dataset.event, Object.fromEntries([...tr.querySelectorAll('td[data-k]')].map((td) => [td.dataset.k, td.dataset.v]))])`);
  const bad = [];
  dom.forEach(([ev, v], i) => {
    const s = detail.steps[i];
    if (ev !== s.event) bad.push(`#${i}.event`);
    for (const k of ["input", "output", "cache_read", "cache_write", "reasoning"]) if (Number(v[k]) !== s[k]) bad.push(`#${i}.${k}`);
    for (const k of ["cost_usd", "context_pct"]) if (Math.abs(Number(v[k]) - s[k]) > 1e-9) bad.push(`#${i}.${k}`);
  });
  report("[detail] per-step table equals /v1/sessions/{key}/usage", detail.steps.length >= 2 && bad.length === 0, bad.join(" ") || `${detail.steps.length} steps`);
  const pct = Number(await cdp.eval(`document.querySelector('#ctx-bar').dataset.pct`));
  const lastPct = detail.steps.at(-1).context_pct;
  report("[detail] context bar shows the last step's share", Math.abs(pct - lastPct) < 1e-9, `${pct.toFixed(2)}%`);
  const head = await cdp.eval(`document.querySelector('#sh-usage').textContent`);
  report('[detail] session totals show five token kinds and "API 等价成本"', ["输入", "输出", "缓存读", "缓存写", "思考", "API 等价成本"].every((t) => head.includes(t)));
  await click("#resume-btn");
  await waitFor(`!document.querySelector('#menu').hidden`, 3_000, "resume menu");
  const items = await cdp.eval(`[...document.querySelectorAll('#menu button')].map((b) => b.textContent.trim())`);
  const mac = process.platform === "darwin";
  report("[detail] resume menu offers \"在终端打开\" on macOS", mac ? items.includes("在 Terminal 中打开") : !items.some((t) => t.includes("终端")), items.join(" / "));
  await cdp.send("Emulation.setFocusEmulationEnabled", { enabled: true }).catch(() => {});
  await click('#menu [data-act="copy-resume"]');
  await sleep(300);
  const clip = await cdp.eval(`navigator.clipboard.readText()`).catch((e) => "ERR " + e.message);
  const resume = await apiJson(api, `/v1/sessions/${enc(key)}/resume`);
  report("[detail] copy resume command puts the resume command on the clipboard", clip === resume.command && !!resume.command, clip);
  // Never open a real terminal window: intercept the write request inside the page.
  await cdp.eval(`window.__term = []; const f0 = window.fetch; window.fetch = (u, o) => { if (String(u).includes('/open-terminal')) {
    window.__term.push({ url: String(u), method: o?.method, write: o?.headers?.['X-Uniflo-Write'] });
    if (window.__termFail) return Promise.resolve(new Response(JSON.stringify({ error: '无法打开 Terminal：stub', reason: '无法打开 Terminal：stub', command: 'cd /w && claude --resume s1' }), { status: 500, headers: { 'content-type': 'application/json' } }));
    return Promise.resolve(new Response(JSON.stringify({ opened: true, terminal: 'terminal' }), { status: 200, headers: { 'content-type': 'application/json' } })); }
    return f0(u, o); }; true`);
  if (mac) {
    await click("#resume-btn");
    await waitFor(`!document.querySelector('#menu').hidden`, 3_000, "resume menu again");
    await click('#menu [data-term="terminal"]');
    const term = await waitFor(`window.__term.length && window.__term[0]`, 3_000, "open-terminal call").catch(() => null);
    report("[detail] \"在 Terminal 中打开\" posts open-terminal with X-Uniflo-Write (stubbed, no window)",
      !!term && term.method === "POST" && term.write === "1" && term.url.includes(`/v1/sessions/${enc(key)}/open-terminal`) && term.url.includes("terminal=terminal"), term?.url || "");
  }
  if (mac) {
    await cdp.eval(`window.__termFail = true`);
    await click("#resume-btn");
    await waitFor(`!document.querySelector('#menu').hidden`, 3_000, "resume menu for failure");
    await click('#menu [data-term="terminal"]');
    const failed = await waitFor(`document.querySelector('#toasts .toast.sticky [data-act="copy-recover"]') && document.querySelector('#toasts .toast.sticky').textContent`, 3_000, "failure toast").catch(() => "");
    await sleep(3_600);
    const kept = await cdp.eval(`!!document.querySelector('#toasts .toast.sticky')`);
    await click('#toasts .toast.sticky [data-act="copy-recover"]');
    await sleep(300);
    const clip2 = await cdp.eval(`navigator.clipboard.readText()`).catch((e) => "ERR " + e.message);
    report("[detail] open-terminal failure keeps a toast with the reason and a working \"复制恢复命令\"",
      failed.includes("无法打开 Terminal：stub") && kept && clip2 === "cd /w && claude --resume s1", `${failed} | kept=${kept} | ${clip2}`);
    await cdp.eval(`window.__termFail = false`);
  }
  await shot("detail.png");
  report("[detail] no page errors", cdp.errors.length === 0, cdp.errors.slice(0, 3).join(" | "));
}

async function runFts() {
  await until(async () => { const s = await apiJson(api, "/v1/stats"); return s.fts && !s.fts.indexing; }, 30_000, "fts index").catch(() => {});
  await load(`${api}/demo?token=${TOKEN}&view=search`);
  await waitFor(`!document.querySelector('#v-search').hidden`, 5_000, "search view");
  await cdp.eval(`document.querySelector('#s-q').focus(); true`);
  await cdp.send("Input.insertText", { text: SEARCH_WORD });
  await cdp.eval(`document.querySelector('#s-q').form.requestSubmit(); true`);
  const hit = await waitFor(`(() => { const h = document.querySelector('.hit[data-event]'); return h && { s: h.dataset.session, e: h.dataset.event, mark: h.querySelector('mark')?.textContent }; })()`, 8_000, "search hit").catch(() => null);
  const api_ = await apiJson(api, `/v1/search?q=${enc(SEARCH_WORD)}`);
  const want = api_.results[0];
  report("[search] hit shows a highlighted snippet in its session group",
    !!hit && hit.s === "claude:search-target" && hit.e === want?.hits[0]?.event && hit.mark === SEARCH_WORD, JSON.stringify(hit));
  report("[search] query is in the URL", (await cdp.eval("location.search")).includes(`sq=${enc(SEARCH_WORD).replace(/%20/g, "+")}`));
  await click(".hit[data-event]");
  const at = await waitFor(`(() => { const n = document.querySelector('#timeline .hit-target[data-id="${hit?.e}"]'); if (!n) return null;
    const r = n.getBoundingClientRect(), t = document.querySelector('#timeline').getBoundingClientRect();
    return { inView: r.top >= t.top - 1 && r.bottom <= t.bottom + 1, before: !!n.previousElementSibling?.dataset?.id, after: !!n.nextElementSibling?.dataset?.id,
      selected: document.querySelector('.row.sel')?.dataset.key, view: !document.querySelector('#app').hidden, url: location.search }; })()`, 8_000, "jump").catch(() => null);
  report("[search] clicking the hit opens the session, scrolls to the event and highlights it",
    !!at && at.inView && at.before && at.after && at.selected === "claude:search-target" && at.view && at.url.includes(`at=${hit?.e}`), JSON.stringify(at));
  await shot("search-jump.png");
  report("[search] no page errors", cdp.errors.length === 0, cdp.errors.slice(0, 3).join(" | "));
}

const METRICS = { sessions: (r) => r.sessions, prompts: (r) => r.prompts, tokens: (r) => r.input + r.output + r.cache_read + r.cache_write, cost: (r) => r.cost_usd || 0, steps: (r) => r.steps };
async function runInsights() {
  await load(`${api}/demo?token=${TOKEN}&view=insights`);
  await waitFor(`document.querySelectorAll('#i-heat rect[data-day]').length > 300 && document.querySelectorAll('#i-ranks li').length > 0`, 10_000, "insights");
  const since = await cdp.eval(`document.querySelector('#v-insights').dataset.since`);
  const tz = await cdp.eval(`Intl.DateTimeFormat().resolvedOptions().timeZone`);
  const q = (g, extra = "") => apiJson(api, `/v1/usage?group_by=${g}&since=${since}&tz=${enc(tz)}${extra}`);
  const [day, wh, wd, hr] = await Promise.all([q("day"), q("weekday_hour"), q("weekday"), q("hour")]);
  const cells = (sel, attr) => cdp.eval(`[...document.querySelectorAll('${sel}')].map((n) => [n.dataset.${attr}, Number(n.dataset.v)])`);
  const compare = (dom, rows, f) => { const m = new Map(rows.map((r) => [r.key, f(r)])); return dom.filter(([k, v]) => Math.abs((m.get(k) || 0) - v) > 1e-9).map(([k]) => k); };
  const nonzero = day.rows.filter((r) => r.key).length;
  for (const metric of ["sessions", "tokens"]) {
    if (metric !== "sessions") {
      await click(`#i-metric [data-metric="${metric}"]`);
      await waitFor(`location.search.includes('metric=${metric}')`, 3_000, metric);
      await sleep(200);
    }
    const heat = await cells("#i-heat rect[data-day]", "day");
    const badHeat = compare(heat, day.rows, METRICS[metric]);
    const activeDom = heat.filter(([, v]) => v > 0).length;
    report(`[insights] heatmap (${metric}) equals /v1/usage?group_by=day`, badHeat.length === 0 && activeDom === day.rows.filter((r) => METRICS[metric](r) > 0).length && nonzero >= 40,
      badHeat.slice(0, 5).join(" ") || `${activeDom} active days of ${heat.length}`);
    const badCells = compare(await cells("#i-punch rect[data-cell]", "cell"), wh.rows, METRICS[metric]);
    const badWd = compare(await cells("#i-punch rect[data-weekday]", "weekday"), wd.rows, METRICS[metric]);
    const badHr = compare(await cells("#i-punch rect[data-hour]", "hour"), hr.rows, METRICS[metric]);
    report(`[insights] weekday × hour (${metric}) equals weekday_hour, weekday and hour groups`, !badCells.length && !badWd.length && !badHr.length,
      [...badCells, ...badWd, ...badHr].slice(0, 5).join(" ") || `${wh.rows.length} cells`);
  }
  for (const rank of ["cost", "tokens"]) {
    if (rank !== "cost") {
      await click(`#i-rank [data-rank="${rank}"]`);
      await waitFor(`location.search.includes('rank=${rank}')`, 3_000, rank);
      await sleep(400);
    }
    const bad = [];
    for (const g of ["project", "model", "harness"]) {
      const r = await q(g, `&sort=${rank}&limit=5`);
      const want = r.rows.filter((x) => x.key !== "(other)").map((x) => `${x.key}=${METRICS[rank](x)}`);
      const got = await cdp.eval(`[...document.querySelectorAll('#i-ranks [data-dim="${g}"] li')].map((li) => li.dataset.key + '=' + Number(li.dataset.v))`);
      if (JSON.stringify(got) !== JSON.stringify(want)) bad.push(`${g}: ${got.join(",")} ≠ ${want.join(",")}`);
    }
    report(`[insights] top-5 rankings by ${rank} equal the grouped /v1/usage`, bad.length === 0, bad.join(" | "));
  }
  await shot("insights.png");
  report("[insights] no page errors", cdp.errors.length === 0, cdp.errors.slice(0, 3).join(" | "));
}

async function runNotify() {
  await load(`${api}/demo?token=${TOKEN}`);
  await waitFor(`!!document.querySelector('.row[data-key="claude:notify2"]') && document.querySelector('#conn').dataset.state === 'live'`, 8_000, "list");
  await cdp.eval(`window.__notes = [];
    window.Notification = class { constructor(title, opts) { this.title = title; this.opts = opts; window.__notes.push(this); } close() {}
      static requestPermission() { window.Notification.permission = 'granted'; return Promise.resolve('granted'); } };
    window.Notification.permission = 'default';
    Object.defineProperty(document, 'visibilityState', { configurable: true, get: () => 'hidden' }); true`);
  const notes = () => cdp.eval(`window.__notes.map((n) => n.title)`);
  const flip = async (file, key, id) => {
    appendFileSync(file, cu(`${id}-u`, Date.now(), "再跑一轮", "/w/notify"));
    await waitFor(`document.querySelector('.row[data-key="${key}"]')?.dataset.status === 'work'`, 4_000, `${key} work`);
    appendFileSync(file, ca(`${id}-a`, Date.now(), [{ type: "text", text: "好了。" }], "end_turn", "claude-sonnet-4-5"));
    await waitFor(`document.querySelector('.row[data-key="${key}"]')?.dataset.status === 'idle'`, 4_000, `${key} idle`);
    await sleep(400);
  };
  const n2 = join(home, ".claude/projects/-w-notify/notify2.jsonl"), n3 = join(home, ".claude/projects/-w-notify/notify3.jsonl");
  await flip(claudeFile, "claude:sess-1", "nt-a");
  report("[notify] off by default: no notification", (await notes()).length === 0);
  await click("#notify");
  await waitFor(`document.querySelector('#notify').getAttribute('aria-pressed') === 'true'`, 3_000, "notify on");
  await flip(claudeFile, "claude:sess-1", "nt-b");
  await flip(claudeFile, "claude:sess-1", "nt-c");
  await flip(n2, "claude:notify2", "nt-d");
  const got = await notes();
  report("[notify] one per session within 30 s, titled with the session title", JSON.stringify(got) === JSON.stringify(["网关新增 /demo 演示页", "通知测试 notify2"]), got.join(" | "));
  const icon = await cdp.eval(`window.__notes[0]?.opts.icon.startsWith('data:image/svg+xml') && window.__notes[0].opts.body.includes('用时')`);
  report("[notify] carries the harness icon and the turn duration", icon);
  await cdp.eval(`window.__notes[1].onclick(); true`);
  const opened = await waitFor(`document.querySelector('.row.sel')?.dataset.key === 'claude:notify2' && location.search.includes('select=claude:notify2')`, 3_000, "click opens").catch(() => false);
  report("[notify] clicking the notification opens that session", !!opened);
  await click("#notify");
  await flip(n3, "claude:notify3", "nt-e");
  report("[notify] switched off: no further notifications", (await notes()).length === 2);
  report("[notify] no page errors", cdp.errors.length === 0, cdp.errors.slice(0, 3).join(" | "));
}

async function runManage() {
  // Keep `busy` inside its working window however long the earlier steps took.
  appendFileSync(join(home, ".claude/projects/-w-clean/busy.jsonl"), cu("busy-u2", Date.now(), "还在迁移", "/w/clean"));
  await load(`${api}/demo?token=${TOKEN}&view=manage&mq=${enc("in:/w/clean")}`);
  await waitFor(`document.querySelector('#m-table tr[data-key="claude:cleanme"]')?.dataset.eligible === 'true' && document.querySelector('#m-table tr[data-key="claude:busy"]')?.dataset.eligible === 'false'`, 10_000, "manage list");
  const row = await cdp.eval(`(() => { const r = document.querySelector('#m-table tr[data-key="claude:cleanme"]'); const b = document.querySelector('#m-table tr[data-key="claude:busy"]');
    return { size: Number(r.querySelector('[data-k=bytes]').dataset.v), sizeText: r.querySelector('[data-k=bytes]').textContent, busy: b.querySelector('[data-k=eligible]').textContent.trim() }; })()`);
  report("[manage] list shows each session's size and whether it can be cleaned", row.size > 0 && row.busy === "会话运行中", `${row.sizeText} · busy: ${row.busy}`);
  await click('[data-pick="claude:cleanme"]');
  await click('[data-pick="claude:busy"]');
  await click("#m-plan");
  await sleep(300);
  const toasts = await cdp.eval(`[...document.querySelectorAll('#toasts > *')].map((n) => n.textContent).join(' | ')`);
  if (toasts) console.log("  toasts:", toasts);
  const plan = await waitFor(`(() => { if (document.querySelector('#m-review').hidden || !document.querySelector('#m-plan-kpis')) return null;
    return { freed: Number(document.querySelector('#m-plan-kpis [data-k=freed_bytes]').dataset.v), archive: Number(document.querySelector('#m-plan-kpis [data-k=archive_bytes]').dataset.v),
      reason: document.querySelector('#m-no [data-key="claude:busy"] [data-reason]')?.textContent, ok: [...document.querySelectorAll('#m-ok [data-key]')].map((n) => n.dataset.key) }; })()`, 5_000, "review").catch(() => null);
  report("[manage] review shows space to free, archive size and why the running session is excluded",
    !!plan && plan.freed === row.size && plan.archive > 0 && plan.reason === "会话运行中" && JSON.stringify(plan.ok) === '["claude:cleanme"]', JSON.stringify(plan));
  await shot("manage-review.png");
  await click("#m-exec");
  const res = await waitFor(`(() => { const a = [...document.querySelectorAll('#m-ok [data-status]')].map((n) => n.closest('[data-key]').dataset.key + '=' + n.dataset.status); return a.length && a; })()`, 10_000, "execute").catch(() => []);
  const moved = readdirSync(trash).length > 0 && !existsSync(join(home, ".claude/projects/-w-clean/cleanme.jsonl"));
  report("[manage] execution reports each session; source moved into the injected trash", JSON.stringify(res) === '["claude:cleanme=archived"]' && moved, res.join(" "));
  await shot("manage-result.png");
  await click("#m-done");
  const badge = await waitFor(`document.querySelector('#m-table tr[data-key="claude:cleanme"] .tag.acc')?.textContent`, 8_000, "archived badge").catch(() => "");
  await click('#nav [data-view="sessions"]');
  const listBadge = await waitFor(`document.querySelector('.row[data-key="claude:cleanme"] .tag')?.textContent`, 8_000, "list badge").catch(() => "");
  report("[manage] cleaned session carries the archived badge in both lists", badge === "已归档" && listBadge === "已归档", `${badge} / ${listBadge}`);
  await click('#nav [data-view="manage"]');
  await click('[data-mtab="archive"]');
  const inArchive = await waitFor(`!!document.querySelector('#m-arch-table tr[data-key="claude:cleanme"]')`, 5_000, "archive tab").catch(() => false);
  report("[manage] archive tab lists the archived session", !!inArchive);
  await shot("manage-archive.png");
  await click('[data-del="claude:cleanme"]');
  await click('[data-del="claude:cleanme"]');
  const gone = await waitFor(`!document.querySelector('#m-arch-table tr[data-key="claude:cleanme"]')`, 5_000, "delete").catch(() => false);
  const arch = await apiJson(api, "/v1/archive");
  report("[manage] deleting an archive (confirmed twice) removes it", !!gone && !arch.archives.some((a) => a.key === "claude:cleanme"));
  report("[manage] no page errors", cdp.errors.length === 0, cdp.errors.slice(0, 3).join(" | "));
}

async function runReadOnly() {
  const health = await apiJson(roApi, "/v1/health");
  await load(`${roApi}/demo?token=${TOKEN}&view=manage`);
  const ro = await waitFor(`(() => { if (document.querySelector('#m-ro').hidden || !document.querySelector('#m-table tr[data-key]')) return null;
    return { checkboxes: document.querySelectorAll('#m-table input[type=checkbox]').length, plan: !!document.querySelector('#m-plan')?.offsetParent, selall: !!document.querySelector('#m-selall')?.offsetParent,
      note: document.querySelector('#m-ro').textContent.includes('--read-only') }; })()`, 10_000, "read-only notice").catch(() => null);
  await shot("readonly-manage.png");
  await click('[data-mtab="archive"]');
  await sleep(500);
  const del = await cdp.eval(`document.querySelectorAll('[data-del]').length`);
  await load(`${roApi}/demo?token=${TOKEN}&select=${enc("claude:ro-1")}`);
  await waitFor(`!!document.querySelector('#resume-btn')`, 8_000, "ro detail");
  await click("#resume-btn");
  await waitFor(`!document.querySelector('#menu').hidden`, 3_000, "ro menu").catch(() => {});
  const head = await cdp.eval(`({ clean: !!document.querySelector('#clean-btn'), term: document.querySelectorAll('#menu [data-act="open-term"]').length })`);
  report("[read-only] write controls are hidden with an explanation", health.read_only === true && !!ro && ro.checkboxes === 0 && !ro.plan && !ro.selall && ro.note && del === 0 && !head.clean && head.term === 0,
    JSON.stringify({ ...ro, del, ...head }));
  report("[read-only] no page errors", cdp.errors.length === 0, cdp.errors.slice(0, 3).join(" | "));
}

// Every view, wide and narrow, dark and light: screenshots plus overflow / overlap / contrast audits.
async function runVisual() {
  const dir = join(SHOTS, "visual");
  mkdirSync(dir, { recursive: true });
  const ready = {
    sessions: `document.querySelectorAll('#timeline [data-id]').length > 3 && !!document.querySelector('#sh-usage .us-cost')`,
    usage: `!!document.querySelector('#u-chart svg') && document.querySelectorAll('#u-table tbody tr').length > 1`,
    search: `document.querySelectorAll('.hit').length > 2`,
    manage: `document.querySelectorAll('#m-table tbody tr[data-eligible="true"], #m-table tbody tr[data-eligible="false"]').length > 3`,
    insights: `document.querySelectorAll('#i-heat rect').length > 300 && document.querySelectorAll('#i-ranks li').length > 2`,
  };
  const extra = { sessions: `&select=${enc("claude:proj-0")}`, search: `&sq=${enc("分页逻辑")}`, manage: `&mq=${enc("in:/w")}` };
  const issues = { overflow: [], overlap: [], contrast: [] };
  let n = 0;
  for (const [w, h, mobile] of [[1480, 920, false], [390, 844, true]]) {
    await viewport(w, h, mobile);
    for (const theme of ["dark", "light"]) {
      await scheme(theme);
      for (const v of Object.keys(ready)) {
        await load(`${api}/demo?token=${TOKEN}&view=${v}${extra[v] || ""}`);
        await waitFor(ready[v], 10_000, `${v} ready`).catch(() => {});
        await sleep(500);
        await shot(join("visual", `${v}-${w}-${theme}.png`));
        n++;
        const audit = await cdp.eval(`__e2e.audit('${v === "sessions" ? "#app" : "#v-" + v}')`);
        for (const k of Object.keys(issues)) for (const x of audit[k]) issues[k].push(`${v}@${w}/${theme}: ${x}`);
      }
    }
  }
  await viewport(1480, 920);
  await scheme(null);
  report("[visual] screenshots: 5 views × 1480/390 px × dark/light", n === 20, dir);
  report("[visual] no horizontal overflow", issues.overflow.length === 0, issues.overflow.slice(0, 5).join(" | "));
  report("[visual] no overlapping elements in bars, toolbars and tiles", issues.overlap.length === 0, issues.overlap.slice(0, 5).join(" | "));
  report("[visual] text contrast meets WCAG AA", issues.contrast.length === 0, issues.contrast.slice(0, 6).join(" | "));
}

// In-page audit helpers, installed before the page's own scripts on every navigation.
const AUDIT = `window.__e2e = (() => {
  const cv = document.createElement('canvas'); cv.width = cv.height = 1;
  const cx = cv.getContext('2d', { willReadFrequently: true });
  const rgba = (c) => { cx.clearRect(0, 0, 1, 1); cx.fillStyle = '#000'; cx.fillStyle = c; cx.fillRect(0, 0, 1, 1); const d = cx.getImageData(0, 0, 1, 1).data; return [d[0], d[1], d[2], d[3] / 255]; };
  const over = (top, bot) => [0, 1, 2].map((i) => top[i] * top[3] + bot[i] * (1 - top[3])).concat(1);
  const lum = (c) => { const f = (v) => { v /= 255; return v <= 0.03928 ? v / 12.92 : ((v + 0.055) / 1.055) ** 2.4; }; return 0.2126 * f(c[0]) + 0.7152 * f(c[1]) + 0.0722 * f(c[2]); };
  const ratio = (a, b) => { const x = lum(a), y = lum(b); return (Math.max(x, y) + 0.05) / (Math.min(x, y) + 0.05); };
  const bgOf = (el) => { const chain = []; for (let n = el; n && n.nodeType === 1; n = n.parentElement) chain.push(getComputedStyle(n).backgroundColor);
    let c = rgba(getComputedStyle(document.documentElement).backgroundColor); if (c[3] < 1) c = over(c, [255, 255, 255, 1]);
    for (const b of chain.reverse()) { const x = rgba(b); if (x[3] > 0) c = over(x, c); } return c; };
  const fgOf = (el, prop = 'color') => { const s = getComputedStyle(el); let c = rgba(s[prop]); let o = 1; for (let n = el; n && n.nodeType === 1; n = n.parentElement) o *= Number(getComputedStyle(n).opacity); return [c[0], c[1], c[2], c[3] * o]; };
  const contrastOf = (el, against) => { const bg = bgOf(against || el); return ratio(over(fgOf(el), bg), bg); };
  const desc = (el) => el.tagName.toLowerCase() + (el.id ? '#' + el.id : '') + (el.className && typeof el.className === 'string' ? '.' + el.className.trim().split(/\\s+/).slice(0, 2).join('.') : '');
  function audit(rootSel) {
    const root = document.querySelector(rootSel), vw = document.documentElement.clientWidth, out = { overflow: [], overlap: [], contrast: [] };
    if (document.scrollingElement.scrollWidth > vw + 1) out.overflow.push('document ' + document.scrollingElement.scrollWidth);
    const scope = [document.querySelector('.top'), root];
    for (const r0 of scope) for (const el of r0.querySelectorAll('*')) {
      const r = el.getBoundingClientRect();
      if (!r.width || !r.height) continue;
      if (r.right > vw + 1) { let clip = false; for (let p = el.parentElement; p && p !== r0.parentElement; p = p.parentElement) { if (p === root) break; if (getComputedStyle(p).overflowX !== 'visible') { clip = true; break; } } if (!clip) out.overflow.push(desc(el) + ' right=' + Math.round(r.right)); }
      if (r.bottom < 0 || r.top > innerHeight || el.closest('svg, [aria-hidden=true], .sk, :disabled, .shimmer')) continue;
      const own = [...el.childNodes].some((n) => n.nodeType === 3 && n.textContent.trim());
      if (!own || getComputedStyle(el).visibility === 'hidden') continue;
      const fg = fgOf(el);
      if (fg[3] < 0.05) continue;
      const bg = bgOf(el), c = ratio(over(fg, bg), bg), fs = parseFloat(getComputedStyle(el).fontSize), bold = Number(getComputedStyle(el).fontWeight) >= 700;
      const need = fs >= 24 || (fs >= 18.66 && bold) ? 3 : 4.5;
      if (c < need - 0.005) out.contrast.push(desc(el) + ' ' + c.toFixed(2) + ' "' + el.textContent.trim().slice(0, 16) + '"');
    }
    for (const box of document.querySelectorAll('.top, ' + ['.toolbar', '.page-h', '.kpis', '.sh-actions', '.sh-top', '.selbar', '.r1', '.r2', '.legend', '.sform'].map((s) => rootSel + ' ' + s).join(', '))) {
      const kids = [...box.children].map((k) => [k, k.getBoundingClientRect()]).filter(([k, r]) => r.width && r.height && getComputedStyle(k).position !== 'absolute');
      for (let i = 0; i < kids.length; i++) for (let j = i + 1; j < kids.length; j++) {
        const [a, ra] = kids[i], [b, rb] = kids[j];
        const w = Math.min(ra.right, rb.right) - Math.max(ra.left, rb.left), h = Math.min(ra.bottom, rb.bottom) - Math.max(ra.top, rb.top);
        if (w > 1 && h > 1) out.overlap.push(desc(a) + ' × ' + desc(b));
      }
    }
    for (const k of Object.keys(out)) out[k] = [...new Set(out[k])].slice(0, 12);
    return out;
  }
  return { contrastOf, audit };
})();`;

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
    if (r.exceptionDetails) throw new Error(r.exceptionDetails.exception?.description || r.exceptionDetails.text);
    return r.result.value;
  };
  return { send, eval: evaluate, errors, close: () => ws.close() };
}

// ---------------------------------------------------------------- run
let daemon;
let cdp;
const api = `http://127.0.0.1:${PORT}`;
const roApi = `http://127.0.0.1:${RO_PORT}`;

async function main() {
daemon = start(BIN, ["daemon", "--bind", `127.0.0.1:${PORT}`, "--no-cache", "--no-price-sync", "--token", TOKEN], { UNIFLO_HOME: home, UNIFLO_TRASH_DIR: trash });
start(BIN, ["daemon", "--bind", `127.0.0.1:${RO_PORT}`, "--no-cache", "--no-price-sync", "--read-only", "--token", TOKEN], { UNIFLO_HOME: roHome });
const profile = mkdtempSync(join(tmpdir(), "uniflo-e2e-chrome-"));
try {
  await until(async () => (await fetch(`${api}/v1/health?token=${TOKEN}`)).ok, 15_000, "daemon health");
  await until(async () => (await fetch(`${roApi}/v1/health?token=${TOKEN}`)).ok, 15_000, "read-only daemon health");
  report("daemon without token answers 401", (await fetch(`${api}/v1/health`)).status === 401);

  // Same page served from a different loopback origin: the third-party integration path (CORS).
  Bun.serve({ port: STATIC_PORT, hostname: "127.0.0.1", fetch: () => new Response(readFileSync(join(ROOT, "examples/web/index.html")), { headers: { "content-type": "text/html; charset=utf-8" } }) });

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
  await cdp.send("Page.addScriptToEvaluateOnNewDocument", { source: AUDIT });
  await viewport(1480, 920);

  mkdirSync(SHOTS, { recursive: true });
  await until(async () => (await apiJson(api, "/v1/usage")).indexing?.ready, 20_000, "usage index").catch(() => {});
  // E2E_ONLY=runManage,runVisual runs a subset while iterating; the full run is the gate.
  const only = (process.env.E2E_ONLY || "").split(",").filter(Boolean);
  if (!only.length) {
    for (const transport of ["sse", "ws", "ndjson"]) await runTransport(transport);
    await runCrossOrigin();
  }
  for (const step of [runIdentity, runIcons, runUsage, runDetail, runFts, runInsights, runNotify, runVisual, runManage, runReadOnly]) {
    if (only.length && !only.includes(step.name)) continue;
    try { await step(); } catch (e) { report(`${step.name}`, false, String(e.message || e)); }
  }
} catch (e) {
  report("harness", false, String(e.message || e) + (daemon.lastErr() ? ` · daemon: ${daemon.lastErr().trim().slice(-300)}` : ""));
} finally {
  try { cdp?.close(); } catch {}
  for (const p of procs) p.kill("SIGTERM");
  await sleep(300);
  for (const d of [home, roHome, trash, profile]) rmSync(d, { recursive: true, force: true });
  console.log(JSON.stringify({ ok: !failed, passed: results.filter((r) => r.ok).length, failed: results.filter((r) => !r.ok).length }));
  process.exit(failed ? 1 : 0);
}
}

await main();
