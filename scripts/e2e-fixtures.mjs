// Minimal hand-written synthetic sessions (one idle human→assistant exchange) for every supported
// harness, laid out the way each adapter reads them under a throwaway UNIFLO_HOME.
// Synthetic data only; nothing outside `home` is touched.
//
//   import { writeAllHarnesses } from "./e2e-fixtures.mjs";
//   const keys = writeAllHarnesses(home, Date.now() - 400 * 86400_000); // { harnessId: "harness:id" }

import { Database } from "bun:sqlite";
import { mkdirSync, rmSync, utimesSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";

const line = (v) => JSON.stringify(v) + "\n";
const jsonl = (...vs) => vs.map(line).join("");

export const HARNESSES = [
  ["claude", "Claude Code"], ["qoder", "Qoder"], ["qwen", "Qwen Work"], ["codex", "Codex"], ["pi", "Pi"],
  ["omp", "oh-my-pi"], ["crosery", "Crosery Agent"], ["commandcode", "Command Code"], ["prime", "Prime Agent"],
  ["cline", "Cline"], ["roo", "Roo Code"], ["kodu", "Kodu"], ["gemini", "Gemini CLI"], ["antigravity", "Antigravity"],
  ["opencode", "OpenCode"], ["kilo", "Kilo Code"], ["zcode", "ZCode"], ["mimocode", "MiMo Code"],
  ["workbuddy", "WorkBuddy"], ["minimax", "MiniMax Code"], ["hermes", "Hermes"], ["factory", "Factory Droid"],
  ["reasonix", "Reasonix"], ["cursor", "Cursor Agent"], ["dsh", "DeepSeek Harness"], ["grok", "Grok CLI"],
  ["kiro", "Kiro CLI"], ["kimi", "Kimi Code"], ["codebuddy", "CodeBuddy"], ["copilot", "GitHub Copilot CLI"],
  ["devin", "Devin CLI"], ["craft", "Craft Agents"], ["openclaw", "OpenClaw"],
];
const NAME = Object.fromEntries(HARNESSES);

export function writeAllHarnesses(home, ts) {
  const keys = {};
  const put = (rel, data) => {
    const p = join(home, rel);
    mkdirSync(dirname(p), { recursive: true });
    writeFileSync(p, data);
    utimesSync(p, ts / 1000, ts / 1000); // formats without in-file times fall back to mtime
  };
  const iso = new Date(ts).toISOString();
  const fileTs = iso.replace(/[:.]/g, "-"); // 2025-01-01T00-00-00-000Z
  const sec = Math.floor(ts / 1000);
  const q = (id) => `${NAME[id]} 示例会话`; // title / first prompt
  const a = (id) => `${NAME[id]} 的示例回复`;
  const cwd = (id) => `/w/fixtures/${id}`;
  const slug = (id) => `-w-fixtures-${id}`;
  const db = (rel, ddl, fill) => {
    const p = join(home, rel);
    mkdirSync(dirname(p), { recursive: true });
    rmSync(p, { force: true });
    const d = new Database(p);
    d.exec(ddl);
    fill(d);
    d.close();
    utimesSync(p, ts / 1000, ts / 1000);
  };
  // Deterministic valid UUIDs for formats that need them.
  const uuid = (n) => `0199f000-0000-7000-8000-${String(n).padStart(12, "0")}`;

  // Claude family: <root>/<slug>/<id>.jsonl
  for (const [id, root] of [["claude", ".claude/projects"], ["qoder", ".qoder/projects"], ["qwen", ".qwenworkcn/projects"]]) {
    const key = `fx-${id}`;
    put(`${root}/${slug(id)}/${key}.jsonl`, jsonl(
      { type: "user", uuid: "u1", timestamp: iso, cwd: cwd(id), sessionId: key, origin: { kind: "human" }, message: { role: "user", content: q(id) } },
      { type: "assistant", uuid: "a1", timestamp: iso, sessionId: key, message: { id: "m1", model: "fx-model", content: [{ type: "text", text: a(id) }], stop_reason: "end_turn" } },
    ));
    keys[id] = key;
  }

  // Pi family: <root>/<slug>/<ts>_<id>.jsonl, header + Pi messages (Command Code: plain Anthropic blocks, bare <id>.jsonl)
  for (const [id, root] of [["pi", ".pi/agent/sessions"], ["omp", ".omp/agent/sessions"], ["crosery", ".crosery/agent-sessions"], ["commandcode", ".commandcode/projects"]]) {
    const key = `fx-${id}`;
    const cc = id === "commandcode";
    put(`${root}/${slug(id)}/${cc ? "" : fileTs + "_"}${key}.jsonl`, jsonl(
      { type: "session", version: 3, id: key, timestamp: iso, cwd: cwd(id), ...(cc ? {} : { title: q(id) }) },
      { type: "message", id: "u1", timestamp: iso, message: { role: "user", content: [{ type: "text", text: q(id) }] } },
      { type: "message", id: "a1", timestamp: iso, message: { role: "assistant", content: [{ type: "text", text: a(id) }], ...(cc ? {} : { stopReason: "stop" }) } },
    ));
    keys[id] = key;
  }

  // Codex: sessions/Y/M/D/rollout-<ts>-<uuid>.jsonl (id = trailing 36-char uuid)
  {
    const key = uuid(1);
    const cx = (type, payload) => line({ timestamp: iso, type, payload });
    put(`.codex/sessions/${iso.slice(0, 4)}/${iso.slice(5, 7)}/${iso.slice(8, 10)}/rollout-${fileTs.slice(0, 19)}-${key}.jsonl`,
      cx("session_meta", { id: key, timestamp: iso, cwd: cwd("codex"), source: "cli" }) +
      cx("event_msg", { type: "task_started", turn_id: "t1" }) +
      cx("response_item", { type: "message", role: "user", content: [{ type: "input_text", text: q("codex") }] }) +
      cx("response_item", { type: "message", role: "assistant", content: [{ type: "output_text", text: a("codex") }] }) +
      cx("event_msg", { type: "task_complete", turn_id: "t1" }));
    keys.codex = key;
  }

  // Prime: sessions/<uuid>.jsonl (Pi-shaped records)
  {
    const key = uuid(2);
    put(`.prime/agent/sessions/${key}.jsonl`, jsonl(
      { type: "session", version: 3, id: key, timestamp: iso, cwd: cwd("prime"), rlmDepth: 0 },
      { type: "message", id: "u1", message: { role: "user", content: [{ type: "text", text: q("prime") }] } },
      { type: "message", id: "a1", message: { role: "assistant", content: [{ type: "text", text: a("prime") }], stopReason: "stop" } },
    ));
    keys.prime = key;
  }

  // Cline family: <VS Code globalStorage>/<extension>/tasks/<task>/ui_messages.json (a whole JSON array, no cwd)
  const gs = process.platform === "darwin" ? "Library/Application Support/Code/User/globalStorage" : ".config/Code/User/globalStorage";
  for (const [id, ext] of [["cline", "saoudrizwan.claude-dev"], ["roo", "rooveterinaryinc.roo-cline"], ["kodu", "kodu-ai.kodu"]]) {
    const key = `fx-${id}`;
    put(`${gs}/${ext}/tasks/${key}/ui_messages.json`, JSON.stringify([
      { ts, type: "say", say: "task", text: q(id) },
      { ts: ts + 1, type: "say", say: "text", text: a(id) },
      { ts: ts + 2, type: "say", say: "completion_result", text: a(id) },
    ]));
    keys[id] = key;
  }

  // Gemini: tmp/<project>/chats/session-*.jsonl; <project> → cwd via projects.json
  {
    const key = "session-fx-gemini";
    put(".gemini/projects.json", JSON.stringify({ projects: { [cwd("gemini")]: "fxgemini" } }));
    put(`.gemini/tmp/fxgemini/chats/${key}.jsonl`, jsonl(
      { sessionId: "fx-gemini", projectHash: "h", startTime: iso, lastUpdated: iso, kind: "main" },
      { id: "u1", timestamp: iso, type: "user", content: [{ text: q("gemini") }] },
      { id: "g1", timestamp: iso, type: "gemini", content: a("gemini"), model: "fx-model" },
    ));
    keys.gemini = key;
  }

  // Antigravity: brain/<conversation>/.system_generated/logs/transcript.jsonl (steps, no cwd)
  {
    const key = "fx-antigravity";
    put(`.gemini/antigravity/brain/${key}/.system_generated/logs/transcript.jsonl`, jsonl(
      { step_index: 0, source: "USER_EXPLICIT", type: "USER_INPUT", status: "DONE", created_at: iso, content: q("antigravity") },
      { step_index: 1, source: "MODEL", type: "PLANNER_RESPONSE", status: "DONE", created_at: iso, content: a("antigravity") },
    ));
    keys.antigravity = key;
  }

  // OpenCode family: one SQLite db with session/message/part (JSON `data`, epoch-ms times)
  for (const [id, rel] of [["opencode", ".local/share/opencode/opencode.db"], ["kilo", ".local/share/kilo/kilo.db"], ["zcode", ".zcode/cli/db/db.sqlite"], ["mimocode", ".local/share/mimocode/mimocode.db"]]) {
    const key = `ses_fx_${id}`;
    db(rel, `CREATE TABLE session (id TEXT PRIMARY KEY, project_id TEXT, parent_id TEXT, slug TEXT, directory TEXT, title TEXT, version TEXT, time_created INTEGER, time_updated INTEGER);
      CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, time_updated INTEGER, data TEXT);
      CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT, session_id TEXT, time_created INTEGER, time_updated INTEGER, data TEXT);`, (d) => {
      d.run("INSERT INTO session VALUES (?,?,?,?,?,?,?,?,?)", [key, "p", null, "s", cwd(id), q(id), "1", ts, ts]);
      const msg = (mid, data) => d.run("INSERT INTO message VALUES (?,?,?,?,?)", [mid, key, ts, ts, JSON.stringify(data)]);
      const part = (pid, mid, data) => d.run("INSERT INTO part VALUES (?,?,?,?,?,?)", [pid, mid, key, ts, ts, JSON.stringify(data)]);
      msg(`msg_u_${id}`, { role: "user", time: { created: ts } });
      part(`prt_u_${id}`, `msg_u_${id}`, { type: "text", text: q(id) });
      msg(`msg_a_${id}`, { role: "assistant", modelID: "fx-model", finish: "stop", time: { created: ts, completed: ts } });
      part(`prt_a_${id}`, `msg_a_${id}`, { type: "text", text: a(id) });
    });
    keys[id] = key;
  }

  // WorkBuddy kernel (also CodeBuddy): <base>/projects/<slug>/<id>.jsonl, one record per message
  for (const [id, base] of [["workbuddy", ".workbuddy"], ["codebuddy", ".codebuddy"]]) {
    const key = `fx-${id}`;
    const r = (rid, extra) => ({ id: rid, timestamp: ts, cwd: cwd(id), sessionId: key, ...extra });
    put(`${base}/projects/${slug(id)}/${key}.jsonl`, jsonl(
      r("u1", { type: "message", role: "user", content: [{ type: "input_text", text: q(id) }] }),
      r("a1", { type: "message", role: "assistant", status: "completed", content: [{ type: "output_text", text: a(id) }], providerData: { model: "fx-model" } }),
      r("t1", { type: "ai-title", aiTitle: q(id) }),
    ));
    keys[id] = key;
  }

  // MiniMax: runtime-state.sqlite with explicit turn_ingress rows
  {
    const key = "fx-minimax";
    db(".minimax/v2/sqlite/runtime-state.sqlite", `
      CREATE TABLE local_runtime_sessions (session_id TEXT PRIMARY KEY, record_json TEXT NOT NULL, updated_at_ms INTEGER NOT NULL, agent_name TEXT, status TEXT, archived INTEGER NOT NULL DEFAULT 0, parent_session_id TEXT, workspace_dir TEXT, title TEXT, created_at_ms INTEGER);
      CREATE TABLE local_runtime_message_rows (id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT NOT NULL, msg_id TEXT NOT NULL, role TEXT, turn_id TEXT, created_at_ms INTEGER NOT NULL, data_json TEXT NOT NULL, UNIQUE(session_id, msg_id));
      CREATE TABLE local_runtime_turn_ingress (turn_id TEXT PRIMARY KEY, session_id TEXT NOT NULL, status TEXT NOT NULL, accepted_at_ms INTEGER NOT NULL, completed_at_ms INTEGER);`, (d) => {
      d.run("INSERT INTO local_runtime_sessions(session_id,record_json,updated_at_ms,workspace_dir,title,created_at_ms) VALUES (?,?,?,?,?,?)", [key, "{}", ts, cwd("minimax"), q("minimax"), ts]);
      d.run("INSERT INTO local_runtime_turn_ingress VALUES (?,?,?,?,?)", ["t1", key, "completed", ts, ts + 1]);
      const row = (msg, role, data) => d.run("INSERT INTO local_runtime_message_rows(session_id,msg_id,role,created_at_ms,data_json) VALUES (?,?,?,?,?)", [key, msg, role, ts, JSON.stringify({ msg_type: 1, role, ...data })]);
      row("u1", "user", { msg_content: q("minimax") });
      row("a1", "assistant", { msg_content: a("minimax"), finish_reason: "stop" });
    });
    keys.minimax = key;
  }

  // Hermes: state.db (sessions + messages, epoch seconds as REAL)
  {
    const key = "fx-hermes";
    db(".hermes/state.db", `
      CREATE TABLE sessions (id TEXT PRIMARY KEY, source TEXT NOT NULL, model TEXT, parent_session_id TEXT, started_at REAL NOT NULL, ended_at REAL, end_reason TEXT, title TEXT, cwd TEXT, title_source TEXT);
      CREATE TABLE messages (id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT NOT NULL, role TEXT NOT NULL, content TEXT, tool_call_id TEXT, tool_calls TEXT, tool_name TEXT, timestamp REAL NOT NULL, token_count INTEGER, finish_reason TEXT, reasoning TEXT, reasoning_content TEXT, active INTEGER NOT NULL DEFAULT 1, compacted INTEGER NOT NULL DEFAULT 0);`, (d) => {
      d.run("INSERT INTO sessions(id,source,model,started_at,title,cwd) VALUES (?,?,?,?,?,?)", [key, "cli", "fx-model", sec, q("hermes"), cwd("hermes")]);
      d.run("INSERT INTO messages(session_id,role,content,timestamp) VALUES (?,?,?,?)", [key, "user", q("hermes"), sec]);
      d.run("INSERT INTO messages(session_id,role,content,timestamp,finish_reason) VALUES (?,?,?,?,?)", [key, "assistant", a("hermes"), sec + 1, "stop"]);
    });
    keys.hermes = key;
  }

  // Factory Droid: sessions/<slug>/<id>.jsonl, Anthropic-style blocks, text-only reply ends the turn
  {
    const key = "fx-factory";
    const m = (id, role, text) => ({ type: "message", id, timestamp: iso, message: { role, content: [{ type: "text", text }] } });
    put(`.factory/sessions/${slug("factory")}/${key}.jsonl`, jsonl(
      { type: "session_start", id: key, title: q("factory"), sessionTitle: q("factory"), cwd: cwd("factory") },
      m("m1", "user", q("factory")), m("m2", "assistant", a("factory")),
    ));
    keys.factory = key;
  }

  // Reasonix: projects/<slug>/sessions/<name>.events.jsonl of append rows; title from the .jsonl.meta sidecar
  {
    const key = "fx-reasonix";
    const row = (idx, messages) => ({ type: "append", message_index: idx, created_at: iso, messages });
    put(`.reasonix/projects/${slug("reasonix")}/sessions/${key}.events.jsonl`, jsonl(
      row(0, [{ role: "user", content: q("reasonix"), createdAt: ts }]),
      row(1, [{ role: "assistant", content: a("reasonix") }]),
    ));
    put(`.reasonix/projects/${slug("reasonix")}/sessions/${key}.jsonl.meta`, JSON.stringify({ model: "fx-model", topic_title: q("reasonix") }));
    keys.reasonix = key;
  }

  // Cursor Agent: agent-transcripts/<chat>/<chat>.jsonl has no ids, times or cwd
  {
    const key = "fx-cursor";
    const f = `.cursor/projects/${slug("cursor")}/agent-transcripts/${key}/${key}.jsonl`;
    put(f, jsonl(
      { role: "user", message: { content: [{ type: "text", text: `<user_query>\n${q("cursor")}\n</user_query>` }] } },
      { role: "assistant", message: { content: [{ type: "text", text: a("cursor") }] } },
    ));
    keys.cursor = key;
  }

  // dsh: sessions/<slug>/<id>/session.jsonl.zstd. A zstd frame with one raw (uncompressed) block, so no compressor is needed.
  {
    const key = "fx-dsh";
    const rec = (type, data) => ({ type, seq: 1, time: ts, data });
    const body = Buffer.from(jsonl(
      { type: "session", version: 4, id: key, createdAt: ts, cwd: cwd("dsh") },
      rec("turn/start", { turn: 1 }),
      rec("user/message", { id: "u1", role: "user", source: { kind: "user" }, content: [{ type: "text", text: q("dsh") }] }),
      rec("assistant/message", { turn: 1, step: 1, message: { role: "assistant", id: "a1", content: [{ type: "text", text: a("dsh") }] } }),
      rec("session/title", { title: q("dsh"), source: { kind: "user" } }),
      rec("turn/end", { turn: 1, reason: { kind: "completed" } }),
    ));
    const head = Buffer.alloc(9); // magic, single-segment + 4-byte content size
    head.set([0x28, 0xb5, 0x2f, 0xfd, 0xa0]);
    head.writeUInt32LE(body.length, 5);
    const blk = Buffer.alloc(3);
    blk.writeUIntLE(1 | (body.length << 3), 0, 3); // last block, type raw
    put(`.dsh/sessions/${slug("dsh")}/${key}/session.jsonl.zstd`, Buffer.concat([head, blk, body]));
    keys.dsh = key;
  }

  // Grok CLI: sessions/<percent-encoded cwd>/<uuid>/updates.jsonl (ACP stream, unix seconds) + summary.json
  {
    const key = uuid(3);
    const upd = (update) => line({ timestamp: sec, method: "_x.ai/session/update", params: { sessionId: key, _meta: { agentTimestampMs: ts }, update } });
    const dir = `.grok/sessions/${encodeURIComponent(cwd("grok"))}/${key}`;
    put(`${dir}/updates.jsonl`,
      upd({ sessionUpdate: "user_message_chunk", content: { type: "text", text: q("grok") }, _meta: { modelId: "fx-model", promptIndex: 0 } }) +
      upd({ sessionUpdate: "agent_message_chunk", content: { type: "text", text: a("grok") } }) +
      upd({ sessionUpdate: "turn_completed", stop_reason: "end_turn" }));
    put(`${dir}/summary.json`, JSON.stringify({ generated_title: q("grok"), current_model_id: "fx-model", info: { cwd: cwd("grok"), id: key }, created_at: iso, updated_at: iso, last_active_at: iso }));
    keys.grok = key;
  }

  // Kiro CLI: sessions/cli/<uuid>.jsonl (position ids, unix seconds) + <uuid>.json sidecar
  {
    const key = uuid(4);
    const m = (kind, text) => ({ kind, data: { content: [{ kind: "text", data: text }], meta: { timestamp: sec } } });
    put(`.kiro/sessions/cli/${key}.jsonl`, jsonl(m("Prompt", q("kiro")), m("AssistantMessage", a("kiro"))));
    put(`.kiro/sessions/cli/${key}.json`, JSON.stringify({ session_id: key, cwd: cwd("kiro"), title: q("kiro"), created_at: iso, updated_at: iso }));
    keys.kiro = key;
  }

  // Kimi Code: sessions/wd_<name>_<hash>/session_<uuid>/agents/main/wire.jsonl (unix seconds) + state.json + shared session_index.jsonl
  {
    const key = `session_${uuid(5)}`;
    const dir = `.kimi-code/sessions/wd_kimi_fx0001/${key}`;
    const loop = (event) => ({ type: "context.append_loop_event", time: sec + 2, event });
    put(`${dir}/agents/main/wire.jsonl`, jsonl(
      { type: "metadata", protocol_version: "1.3", created_at: ts },
      { type: "turn.prompt", time: sec, input: [{ type: "text", text: q("kimi") }], origin: { kind: "user" } },
      loop({ type: "step.begin", uuid: "s1" }),
      loop({ type: "content.part", part: { type: "text", text: a("kimi") } }),
      loop({ type: "step.end" }),
    ));
    put(`${dir}/state.json`, JSON.stringify({ createdAt: iso, title: q("kimi"), isCustomTitle: true, workDir: cwd("kimi") }));
    put(".kimi-code/session_index.jsonl", line({ sessionId: key, workDir: cwd("kimi") }));
    keys.kimi = key;
  }

  // Copilot CLI: session-store.db, one `turns` row per finished exchange, UTC text times
  {
    const key = "fx-copilot";
    const utc = iso.slice(0, 19).replace("T", " ");
    db(".copilot/session-store.db", `
      CREATE TABLE sessions (id TEXT PRIMARY KEY, cwd TEXT, repository TEXT, branch TEXT, summary TEXT, created_at TEXT, updated_at TEXT);
      CREATE TABLE turns (id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT NOT NULL, turn_index INTEGER NOT NULL, user_message TEXT, assistant_response TEXT, timestamp TEXT);`, (d) => {
      d.run("INSERT INTO sessions VALUES (?,?,?,?,?,?,?)", [key, cwd("copilot"), null, "main", q("copilot"), utc, utc]);
      d.run("INSERT INTO turns(session_id,turn_index,user_message,assistant_response,timestamp) VALUES (?,?,?,?,?)", [key, 0, q("copilot"), a("copilot"), utc]);
    });
    keys.copilot = key;
  }

  // Devin CLI: cli/sessions.db, message tree walked from main_chain_id (unix seconds)
  {
    const key = "fx-devin";
    db(".local/share/devin/cli/sessions.db", `
      CREATE TABLE sessions (id TEXT PRIMARY KEY, title TEXT, working_directory TEXT, model TEXT, agent_mode TEXT, created_at INTEGER, last_activity_at INTEGER, hidden INTEGER DEFAULT 0, main_chain_id INTEGER);
      CREATE TABLE message_nodes (row_id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT NOT NULL, node_id INTEGER NOT NULL, parent_node_id INTEGER, chat_message TEXT NOT NULL, created_at INTEGER NOT NULL);`, (d) => {
      d.run("INSERT INTO sessions(id,title,working_directory,model,created_at,last_activity_at,hidden,main_chain_id) VALUES (?,?,?,?,?,?,0,2)", [key, q("devin"), cwd("devin"), "fx-model", sec, sec]);
      const node = (n, parent, msg) => d.run("INSERT INTO message_nodes(session_id,node_id,parent_node_id,chat_message,created_at) VALUES (?,?,?,?,?)", [key, n, parent, JSON.stringify(msg), sec]);
      node(1, null, { role: "user", content: q("devin"), metadata: { is_user_input: true, telemetry: { source: "user" } } });
      node(2, 1, { role: "assistant", content: a("devin"), metadata: {} });
    });
    keys.devin = key;
  }

  // OpenClaw: agents/<agent>/agent/openclaw-agent.sqlite holding Pi session-tree entries
  {
    const key = "fx-openclaw";
    db(".openclaw/agents/main/agent/openclaw-agent.sqlite", `
      CREATE TABLE session_windows (session_id TEXT PRIMARY KEY, session_key TEXT, spawned_by TEXT, created_at INTEGER, updated_at INTEGER, transcript_updated_at INTEGER, started_at INTEGER, model TEXT, channel TEXT, display_name TEXT);
      CREATE TABLE session_nodes (session_key TEXT PRIMARY KEY, label TEXT, display_name TEXT, entry_json TEXT);
      CREATE TABLE transcript_events (session_id TEXT, seq INTEGER, event_json TEXT, created_at INTEGER, PRIMARY KEY (session_id, seq));
      CREATE TABLE session_transcript_active_events (session_id TEXT, event_seq INTEGER, active_position INTEGER);`, (d) => {
      d.run("INSERT INTO session_windows VALUES (?,?,?,?,?,?,?,?,?,?)", [key, `agent:main:${key}`, null, ts, ts, ts, ts, "fx-model", "cli", q("openclaw")]);
      const ev = (seq, v) => d.run("INSERT INTO transcript_events VALUES (?,?,?,?)", [key, seq, JSON.stringify(v), ts]);
      ev(1, { type: "session", version: 3, id: key, timestamp: iso, cwd: cwd("openclaw") });
      ev(2, { type: "message", id: "u1", parentId: null, timestamp: iso, message: { role: "user", content: [{ type: "text", text: q("openclaw") }] } });
      ev(3, { type: "message", id: "a1", parentId: "u1", timestamp: iso, message: { role: "assistant", content: [{ type: "text", text: a("openclaw") }], stopReason: "stop" } });
    });
    keys.openclaw = key;
  }

  // Craft Agents: workspaces/<workspace>/sessions/<session>/session.jsonl, id = <workspace>/<session>; line 1 is the header
  {
    const key = "fx-ws/fx-craft";
    put(`.craft-agent/workspaces/${key.split("/")[0]}/sessions/fx-craft/session.jsonl`, jsonl(
      { id: "fx-craft", name: q("craft"), workingDirectory: cwd("craft"), model: "fx-model", createdAt: ts, lastMessageAt: ts },
      { id: "m1", type: "user", content: q("craft"), timestamp: ts },
      { id: "m2", type: "assistant", content: a("craft"), timestamp: ts },
    ));
    keys.craft = key;
  }

  // Gateway session keys (`<harness>:<id>`); craft ids contain `/`, so percent-encode when used in a URL path.
  return Object.fromEntries(Object.entries(keys).map(([h, id]) => [h, `${h}:${id}`]));
}
