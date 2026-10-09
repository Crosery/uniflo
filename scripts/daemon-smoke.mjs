#!/usr/bin/env node
// Cross-platform smoke test: launches daemon, asserts HTTP API and gracefully shuts down.
// Runs natively on macOS, Linux, and Windows.

import { spawn } from "node:child_process";
import { existsSync } from "node:fs";
import { resolve } from "node:path";

const isWin = process.platform === "win32";
const binName = isWin ? "uniflo.exe" : "uniflo";
const binPath = resolve(process.cwd(), "target", "release", binName);

const port = process.env.UNIFLO_TEST_PORT || "7399";
const bind = `127.0.0.1:${port}`;
const base = `http://${bind}`;

if (!existsSync(binPath)) {
  console.error(`Binary not found at ${binPath}. Run cargo build --release first.`);
  process.exit(1);
}

console.log(`==> Starting ${binName} daemon smoke test on ${process.platform} (${process.arch}) at ${base}...`);

const daemon = spawn(binPath, ["daemon", "--bind", bind, "--no-cache", "--no-price-sync"], {
  stdio: ["ignore", "pipe", "pipe"],
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
    console.log(`✓ /v1/sessions returned response with ${sessions.items ? sessions.items.length : 0} items`);

    console.log(`==> Daemon smoke test passed 100% on ${process.platform}!\n`);
  } finally {
    daemon.kill("SIGINT");
    await new Promise((r) => setTimeout(r, 500));
    daemon.kill("SIGKILL");
  }
}

run().catch((err) => {
  console.error("FAIL:", err.message);
  process.exit(1);
});
