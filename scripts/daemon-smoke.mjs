#!/usr/bin/env node
// Cross-platform smoke test: launches daemon, asserts HTTP API and gracefully shuts down.
// Runs natively on macOS, Linux, and Windows.

import { spawn } from "node:child_process";
import { existsSync, mkdtempSync, mkdirSync, writeFileSync, rmSync } from "node:fs";
import { resolve, join } from "node:path";
import { tmpdir } from "node:os";

const isWin = process.platform === "win32";
const binName = isWin ? "uniflo.exe" : "uniflo";
const binPath = resolve(process.env.UNIFLO_BIN || join(process.env.CARGO_TARGET_DIR || "target", "release", binName));

const port = process.env.UNIFLO_TEST_PORT || "7399";
const bind = `127.0.0.1:${port}`;
const base = `http://${bind}`;

if (!existsSync(binPath)) {
  console.error(`Binary not found at ${binPath}. Run cargo build --release first.`);
  process.exit(1);
}

console.log(`==> Starting ${binName} daemon smoke test on ${process.platform} (${process.arch}) at ${base}...`);

const home = mkdtempSync(join(tmpdir(), "uniflo-smoke-"));
const sessionsDir = join(home, ".claude", "projects", "smoke");
mkdirSync(sessionsDir, { recursive: true });
writeFileSync(join(sessionsDir, "smoke.jsonl"), JSON.stringify({
  type: "user", uuid: "smoke-user", timestamp: Date.now(), cwd: home,
  message: { role: "user", content: "synthetic daemon smoke" },
}) + "\n");
const env = { ...process.env, HOME: home, USERPROFILE: home, UNIFLO_HOME: home,
  UNIFLO_DATA_DIR: join(home, "data"), UNIFLO_CACHE_DIR: join(home, "cache"), UNIFLO_CONFIG_DIR: join(home, "config"),
  UNIFLO_LANG: "en" };
delete env.UNIFLO_TOKEN;
delete env.CLAUDE_CONFIG_DIR;
delete env.CODEX_HOME;
const daemon = spawn(binPath, ["daemon", "--bind", bind, "--no-cache", "--no-fts", "--no-price-sync", "--no-update-check"], {
  stdio: ["ignore", "pipe", "pipe"], env,
});

let stderr = "";
daemon.stderr.on("data", (chunk) => {
  stderr += chunk.toString();
});

let stdout = "";
daemon.stdout.on("data", (chunk) => {
  stdout += chunk.toString();
});

async function waitReady(maxMs = 10000) {
  const start = Date.now();
  while (Date.now() - start < maxMs) {
    try {
      const res = await fetch(`${base}/v1/health`);
      if (res.ok) {
        const json = await res.json();
        return json;
      }
    } catch {
      // not ready yet
    }
    await new Promise((r) => setTimeout(r, 200));
  }
  throw new Error(`Daemon failed to respond within ${maxMs}ms. Stderr: ${stderr}`);
}

async function run() {
  try {
    const health = await waitReady();
    console.log(`✓ Daemon listening on ${base}`);
    console.log(`  Health response: ok=${health.ok}, version=${health.version}, schema=${health.schema}`);

    // Check harnesses endpoint
    const hRes = await fetch(`${base}/v1/harnesses`);
    if (!hRes.ok) throw new Error(`/v1/harnesses returned status ${hRes.status}`);
    const harnesses = await hRes.json();
    console.log(`✓ /v1/harnesses returned ${harnesses.length} registered harnesses`);
    if (harnesses.length < 24) {
      throw new Error(`Expected at least 24 harnesses, got ${harnesses.length}`);
    }

    // Check demo web page
    const demoRes = await fetch(`${base}/demo`);
    if (!demoRes.ok) throw new Error(`/demo returned status ${demoRes.status}`);
    const html = await demoRes.text();
    if (!html.includes("Uniflo")) throw new Error(`/demo does not contain 'Uniflo' title`);
    console.log(`✓ /demo served ${html.length} bytes HTML correctly`);

    // Check sessions query
    const sRes = await fetch(`${base}/v1/sessions?limit=5`);
    if (!sRes.ok) throw new Error(`/v1/sessions returned status ${sRes.status}`);
    const sessions = await sRes.json();
    if (!Array.isArray(sessions) || sessions.length !== 1 || sessions[0].key !== "claude:smoke") {
      throw new Error("/v1/sessions did not return the isolated synthetic session");
    }
    console.log(`✓ /v1/sessions returned the synthetic session`);

    const ftsRes = await fetch(`${base}/v1/search?q=smoke`);
    if (ftsRes.status !== 503) throw new Error(`/v1/search with --no-fts returned ${ftsRes.status}, want 503`);
    console.log(`✓ /v1/search answers 503 with --no-fts`);

    console.log(`==> Daemon smoke test passed 100% on ${process.platform}!\n`);
  } finally {
    daemon.kill("SIGINT");
    await new Promise((r) => setTimeout(r, 500));
    daemon.kill("SIGKILL");
    await new Promise((r) => setTimeout(r, 100));
    rmSync(home, { recursive: true, force: true });
  }
}

run().catch((err) => {
  console.error("FAIL:", err.message);
  process.exit(1);
});
