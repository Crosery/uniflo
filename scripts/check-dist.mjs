#!/usr/bin/env node
// Release packaging stays consistent (ADR-0009):
// - .github/workflows/release.yml builds exactly the five targets, packs them with scripts/package.sh,
//   writes SHA256SUMS, uploads to the tag's release and marks `-` tags as prereleases;
// - the cargo-binstall templates of the `uniflo` package (read with `cargo metadata`) render the
//   package and executable names that scripts/package.sh produces (checked on a real package for
//   the host target) and that `uniflo update` downloads (`uniflo_core::install::asset_name`).
import { execFileSync } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const TARGETS = [
  "aarch64-apple-darwin",
  "x86_64-apple-darwin",
  "x86_64-unknown-linux-musl",
  "aarch64-unknown-linux-musl",
  "x86_64-pc-windows-msvc",
];
const failures = [];
const expect = (ok, what) => ok || failures.push(what);

const wf = readFileSync(join(root, ".github/workflows/release.yml"), "utf8");
const targets = [...wf.matchAll(/^\s*- target: (\S+)\s*$/gm)].map((m) => m[1]);
expect(JSON.stringify([...targets].sort()) === JSON.stringify([...TARGETS].sort()), `workflow targets ${targets}`);
expect(/scripts\/package\.sh "\$\{\{ needs\.publish\.outputs\.version \}\}" "\$TARGET"/.test(wf), "package step");
expect(/sha256sum uniflo-\* > SHA256SUMS/.test(wf), "SHA256SUMS step");
expect(/gh release create "\$TAG" dist\/\*/.test(wf), "upload step");
expect(/cp scripts\/install\.sh scripts\/install\.ps1 dist\//.test(wf), "install scripts attached");
expect(/\[\[ "\$TAG" == \*-\* \]\][\s\S]*prerelease_flag=--prerelease/.test(wf), "prerelease detection");
expect(/\$PRERELEASE_FLAG/.test(wf.slice(wf.indexOf("gh release create"))), "prerelease flag passed");
expect(/scripts\/publish-crates\.sh/.test(wf), "crates.io publish kept");

const meta = JSON.parse(execFileSync("cargo", ["metadata", "--format-version", "1", "--no-deps"], { cwd: root }));
const pkg = meta.packages.find((p) => p.name === "uniflo");
const bs = pkg.metadata?.binstall ?? {};
const render = (tpl, target) =>
  tpl
    .replaceAll("{ repo }", pkg.repository)
    .replaceAll("{ name }", pkg.name)
    .replaceAll("{ version }", pkg.version)
    .replaceAll("{ target }", target)
    .replaceAll("{ bin }", "uniflo")
    .replaceAll("{ binary-ext }", target.includes("windows") ? ".exe" : "");
const assetName = (t) => `uniflo-${pkg.version}-${t}.${t.includes("windows") ? "zip" : "tar.gz"}`;
const binPath = (t) => `uniflo-${pkg.version}-${t}/uniflo${t.includes("windows") ? ".exe" : ""}`;
const rendered = {};
for (const t of TARGETS) {
  const o = bs.overrides?.[t] ?? {};
  const url = render(o["pkg-url"] ?? bs["pkg-url"] ?? "", t);
  const bin = render(o["bin-dir"] ?? bs["bin-dir"] ?? "", t);
  const fmt = o["pkg-fmt"] ?? bs["pkg-fmt"];
  rendered[t] = { url, bin };
  expect(url === `${pkg.repository}/releases/download/v${pkg.version}/${assetName(t)}`, `binstall pkg-url ${t}: ${url}`);
  expect(bin === binPath(t), `binstall bin-dir ${t}: ${bin}`);
  expect(fmt === (t.includes("windows") ? "zip" : "tgz"), `binstall pkg-fmt ${t}: ${fmt}`);
}

const host = { "darwin-arm64": "aarch64-apple-darwin", "darwin-x64": "x86_64-apple-darwin", "linux-x64": "x86_64-unknown-linux-musl", "linux-arm64": "aarch64-unknown-linux-musl" }[`${process.platform}-${process.arch}`];
if (host) {
  const tmp = mkdtempSync(join(tmpdir(), "uniflo-check-dist-"));
  try {
    writeFileSync(join(tmp, "bin"), "#!/bin/sh\n");
    const out = execFileSync("bash", ["scripts/package.sh", pkg.version, host, join(tmp, "bin"), join(tmp, "dist")], { cwd: root })
      .toString()
      .trim();
    expect(rendered[host].url.endsWith(`/${out.split(/[\\/]/).pop()}`), `package.sh produced ${out}`);
    const entries = execFileSync("tar", ["-tzf", out]).toString().split("\n").filter(Boolean).sort();
    const dir = `uniflo-${pkg.version}-${host}`;
    expect(
      JSON.stringify(entries) ===
        JSON.stringify([`${dir}/`, `${dir}/LICENSE`, `${dir}/THIRD_PARTY_NOTICES.md`, `${dir}/uniflo`].sort()),
      `package layout ${entries}`,
    );
    expect(entries.includes(rendered[host].bin), `binstall bin-dir not in package: ${rendered[host].bin}`);
  } finally {
    rmSync(tmp, { recursive: true, force: true });
  }
}

if (failures.length) {
  console.error(`release packaging check failed:\n  ${failures.join("\n  ")}`);
  process.exit(1);
}
console.log(`release packaging: ${TARGETS.length} targets, binstall templates match${host ? `, ${host} package layout ok` : ""}`);
