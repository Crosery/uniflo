#!/usr/bin/env node
// Builds the developer site into target/site from the repo's own docs (single source of truth):
// every page also ships as Markdown, plus llms.txt / llms-full.txt for agents. No dependencies.
//   node site/build.mjs [outDir]
import { mkdirSync, readFileSync, readdirSync, writeFileSync, copyFileSync, rmSync, existsSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const out = process.argv[2] || join(root, "target/site");
const REPO = "https://github.com/Crosery/uniflo";
const SITE = "https://crosery.github.io/uniflo/";
const version = readFileSync(join(root, "Cargo.toml"), "utf8").match(/^version\s*=\s*"([^"]+)"/m)[1];

// slug, title, source (repo path), one-line summary for llms.txt
const PAGES = [
  ["quickstart", "快速开始", "README.md", "安装、启动守护进程、接入 agent、常用命令"],
  ["sdk", "Rust SDK", "docs/sdk.md", "用 crates.io 上的 uniflo-* crate 二次开发：客户端、嵌入引擎、wire 类型"],
  ["api", "网关 API", "docs/api.md", "REST、SSE、NDJSON、WebSocket 接口，鉴权与写接口"],
  ["schema", "Wire schema", "docs/schema.md", "Session、Event、Envelope 与用量、检索响应类型"],
  ["search", "搜索语法", "docs/search.md", "会话过滤与模糊匹配、全文检索语法与排序"],
  ["agents", "Agent 接入", "docs/agents.md", "MCP 服务器、Skill、uniflo setup、恢复会话、记忆与接力上下文"],
  ["console", "网页控制台", "docs/console.md", "/demo 的六个视图、命令面板、主题、URL 参数与验收"],
  ["architecture", "架构", "docs/architecture.md", "crate 分层、数据流、状态机、扩展点"],
  ["adapters", "Harness 适配", "docs/adapters.md", "33 个 harness 的存储位置、回合信号与用量口径"],
];
const bySource = new Map(PAGES.map((p) => [p[2], p[0]]));

const esc = (s) => s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;");
// GitHub's heading anchors: lower-case, drop punctuation, spaces to hyphens (CJK kept)
const slug = (s) => s.toLowerCase().replace(/<[^>]+>/g, "").replace(/[^\p{L}\p{N}\s_-]/gu, "").trim().replace(/\s/g, "-");

function link(href, from) {
  if (/^(https?:|mailto:|#)/.test(href)) return href;
  const [path, hash = ""] = href.split("#");
  const base = from.includes("/") ? from.slice(0, from.lastIndexOf("/") + 1) : "";
  const parts = [];
  for (const seg of (base + path).split("/")) seg === ".." ? parts.pop() : seg && seg !== "." && parts.push(seg);
  const target = parts.join("/");
  if (bySource.has(target)) return `${bySource.get(target)}.html${hash ? "#" + hash : ""}`;
  // docs/assets/* ship with the site (copied below); other repo files link to GitHub
  if (target.startsWith("docs/assets/")) return `assets/${target.slice("docs/assets/".length)}`;
  if (!path) return "#" + hash;
  return `${REPO}/blob/main/${target}${hash ? "#" + hash : ""}`;
}

function inline(s, from) {
  const codes = [];
  s = s.replace(/`([^`]+)`/g, (_, c) => `\u0000${codes.push(c) - 1}\u0000`);
  s = esc(s)
    .replace(/!\[([^\]]*)\]\(([^)]+)\)/g, (_, alt, src) => `<img alt="${alt}" src="${link(src, from)}" loading="lazy">`)
    .replace(/\[([^\]]+)\]\(([^)\s]+)\)/g, (_, t, h) => `<a href="${esc(link(h.replace(/&amp;/g, "&"), from))}">${t}</a>`)
    .replace(/\*\*([^*]+)\*\*/g, "<strong>$1</strong>")
    .replace(/(^|[^*\w])\*([^*\s][^*]*)\*(?=[^*\w]|$)/g, "$1<em>$2</em>");
  return s.replace(/\u0000(\d+)\u0000/g, (_, i) => `<code>${esc(codes[+i])}</code>`);
}

// Markdown subset the docs use: headings, paragraphs, nested lists, tables, fences, quotes, rules.
function render(md, from) {
  const lines = md.replace(/\r/g, "").split("\n"), html = [], toc = [];
  let i = 0, title = "";
  const para = [];
  const flush = () => { if (para.length) { html.push(`<p>${inline(para.join(" "), from)}</p>`); para.length = 0; } };
  while (i < lines.length) {
    const l = lines[i];
    let m;
    if ((m = l.match(/^```\s*(\S*)/))) {
      flush();
      const body = [];
      for (i++; i < lines.length && !/^```/.test(lines[i]); i++) body.push(lines[i]);
      i++;
      html.push(`<div class="code"><div class="code-h"><span>${esc(m[1] || "text")}</span><button class="copy" type="button" aria-label="复制">复制</button></div><pre><code class="lang-${esc(m[1] || "text")}">${esc(body.join("\n"))}</code></pre></div>`);
      continue;
    }
    if ((m = l.match(/^(#{1,4})\s+(.*)$/))) {
      flush();
      const n = m[1].length, text = m[2].trim(), id = slug(text.replace(/`/g, ""));
      if (n === 1 && !title) { title = text; i++; continue; }
      if (n <= 3) toc.push([n, id, text]);
      html.push(`<h${n} id="${esc(id)}"><a class="anchor" href="#${esc(id)}" aria-hidden="true">#</a>${inline(text, from)}</h${n}>`);
      i++;
      continue;
    }
    if (/^\|/.test(l) && /^\|?\s*:?-{2,}/.test(lines[i + 1] || "")) {
      flush();
      const row = (r) => r.trim().replace(/^\||\|$/g, "").split(/(?<!\\)\|/).map((c) => c.trim().replace(/\\\|/g, "|"));
      const head = row(l);
      i += 2;
      const body = [];
      while (i < lines.length && /^\|/.test(lines[i])) body.push(row(lines[i++]));
      html.push(`<div class="table"><table><thead><tr>${head.map((c) => `<th>${inline(c, from)}</th>`).join("")}</tr></thead><tbody>${body.map((r) => `<tr>${r.map((c) => `<td>${inline(c, from)}</td>`).join("")}</tr>`).join("")}</tbody></table></div>`);
      continue;
    }
    if (/^>\s?/.test(l)) {
      flush();
      const q = [];
      while (i < lines.length && /^>\s?/.test(lines[i])) q.push(lines[i++].replace(/^>\s?/, ""));
      html.push(`<blockquote>${render(q.join("\n"), from).html}</blockquote>`);
      continue;
    }
    if (/^\s*([-*]|\d+\.)\s+/.test(l)) {
      flush();
      const block = [];
      while (i < lines.length && (/^\s*([-*]|\d+\.)\s+/.test(lines[i]) || (/^\s{2,}\S/.test(lines[i]) && block.length))) block.push(lines[i++]);
      html.push(list(block, from));
      continue;
    }
    if (/^(-{3,}|\*{3,})\s*$/.test(l)) { flush(); html.push("<hr>"); i++; continue; }
    if (!l.trim()) { flush(); i++; continue; }
    para.push(l.trim());
    i++;
  }
  flush();
  return { title, toc, html: html.join("\n") };
}

function list(block, from) {
  const indent = (s) => s.match(/^\s*/)[0].length;
  const base = indent(block[0]), ordered = /^\s*\d+\./.test(block[0]);
  const items = [];
  for (const l of block) {
    if (indent(l) === base && /^\s*([-*]|\d+\.)\s+/.test(l)) items.push([l.replace(/^\s*([-*]|\d+\.)\s+/, "")]);
    else items.at(-1).push(l);
  }
  const li = items.map(([first, ...rest]) => {
    const sub = rest.filter((r) => /^\s*([-*]|\d+\.)\s+/.test(r) || indent(r) > base);
    const nested = sub.length && /^\s*([-*]|\d+\.)\s+/.test(sub[0]) ? list(sub, from) : sub.length ? ` ${inline(sub.map((s) => s.trim()).join(" "), from)}` : "";
    return `<li>${inline(first, from)}${nested}</li>`;
  });
  return ordered ? `<ol>${li.join("")}</ol>` : `<ul>${li.join("")}</ul>`;
}

const nav = (active) => PAGES.map(([s, t]) => `<a href="${s}.html"${s === active ? ' aria-current="page"' : ""}>${esc(t)}</a>`).join("");
const head = (title, desc, md) => `<!doctype html>
<html lang="zh-CN">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>${esc(title)}</title>
<meta name="description" content="${esc(desc)}">
<meta name="color-scheme" content="dark light">
<link rel="icon" href="assets/logo.svg" type="image/svg+xml">
<link rel="stylesheet" href="assets/site.css">
${md ? `<link rel="alternate" type="text/markdown" href="${md}" title="Markdown">` : ""}
<link rel="alternate" type="text/plain" href="llms.txt" title="llms.txt">
<script>document.documentElement.dataset.scheme = localStorage.getItem("uf-site-theme") || (matchMedia("(prefers-color-scheme: light)").matches ? "light" : "dark")</script>
</head>`;
const topbar = (active) => `<header class="top">
  <a class="brand" href="index.html"><img src="assets/logo.svg" alt="" width="22" height="22"><b>Uniflo</b><span class="ver">v${esc(version)}</span></a>
  <nav class="top-nav"><a href="quickstart.html"${active === "quickstart" ? ' aria-current="page"' : ""}>文档</a><a href="sdk.html"${active === "sdk" ? ' aria-current="page"' : ""}>SDK</a><a href="api.html"${active === "api" ? ' aria-current="page"' : ""}>API</a><a href="agents.html"${active === "agents" ? ' aria-current="page"' : ""}>Agent</a></nav>
  <span class="grow"></span>
  <a class="pill" href="llms.txt" title="给 agent 读的索引">llms.txt</a>
  <button class="icon" id="theme" type="button" aria-label="切换深浅色"><svg viewBox="0 0 24 24" width="17" height="17"><path d="M12 3a9 9 0 1 0 9 9 7 7 0 0 1-9-9Z" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linejoin="round"/></svg></button>
  <a class="icon" href="${REPO}" aria-label="GitHub"><svg viewBox="0 0 24 24" width="18" height="18"><path fill="currentColor" d="M12 2a10 10 0 0 0-3.16 19.49c.5.09.68-.22.68-.48v-1.7c-2.78.6-3.37-1.34-3.37-1.34-.45-1.16-1.11-1.47-1.11-1.47-.9-.62.07-.6.07-.6 1 .07 1.53 1.03 1.53 1.03.9 1.52 2.34 1.08 2.91.83.09-.65.35-1.09.63-1.34-2.22-.25-4.55-1.11-4.55-4.94 0-1.09.39-1.98 1.03-2.68-.1-.25-.45-1.27.1-2.64 0 0 .84-.27 2.75 1.02a9.56 9.56 0 0 1 5 0c1.91-1.29 2.75-1.02 2.75-1.02.55 1.37.2 2.39.1 2.64.64.7 1.03 1.59 1.03 2.68 0 3.84-2.34 4.68-4.57 4.93.36.31.68.92.68 1.85v2.75c0 .27.18.58.69.48A10 10 0 0 0 12 2Z"/></svg></a>
</header>`;

function docPage([s, t, src, desc], body) {
  const toc = body.toc.filter(([n]) => n === 2 || n === 3).map(([n, id, text]) => `<a class="l${n}" href="#${esc(id)}">${inline(text.replace(/`/g, ""), src).replace(/<[^>]+>/g, "")}</a>`).join("");
  return `${head(`${t} · Uniflo`, desc, `${s}.md`)}
<body class="doc">
${topbar(s)}
<div class="doc-wrap">
  <aside class="side" aria-label="文档目录"><div class="side-in"><p class="side-h">文档</p>${nav(s)}<p class="side-h">给 agent</p><a href="llms.txt">llms.txt</a><a href="llms-full.txt">llms-full.txt</a><a href="${s}.md">本页 Markdown</a></div></aside>
  <main class="prose" id="main">
    <div class="crumbs"><span>${esc(t)}</span><a href="${s}.md" class="md-link">Markdown</a><a href="${REPO}/blob/main/${src}" class="md-link">在 GitHub 编辑</a></div>
    <h1>${inline(body.title || t, src)}</h1>
    ${body.html}
    <footer class="doc-f"><span>源文件 <a href="${REPO}/blob/main/${src}"><code>${esc(src)}</code></a></span><span>Uniflo v${esc(version)} · MIT</span></footer>
  </main>
  <nav class="toc" aria-label="本页目录"><div class="toc-in"><p class="side-h">本页</p>${toc}</div></nav>
</div>
<script src="assets/site.js" defer></script>
</body>
</html>
`;
}

rmSync(out, { recursive: true, force: true });
mkdirSync(join(out, "assets"), { recursive: true });
for (const f of ["site.css", "site.js", "logo.svg"]) copyFileSync(join(root, "site/assets", f), join(out, "assets", f));
// harness marks: cut from the demo page's sprite, the same single source the gateway serves them from
const demo = readFileSync(join(root, "examples/web/index.html"), "utf8");
const sprite = demo.slice(demo.indexOf('<svg id="harness-icons"'), demo.indexOf("</svg>", demo.indexOf('<svg id="harness-icons"')));
const HERO = ["claude", "codex", "gemini", "opencode", "omp", "cursor", "copilot", "kimi", "qwen", "factory", "grok", "cline", "kiro", "minimax"];
const symbols = HERO.map((id) => (sprite.match(new RegExp(`<symbol id="hi-${id}"[\\s\\S]*?</symbol>`)) || [""])[0]);
if (symbols.some((x) => !x)) throw new Error("harness icon missing from examples/web/index.html: " + HERO.filter((_, i) => !symbols[i]).join(","));
const iconSprite = `<svg width="0" height="0" style="position:absolute" aria-hidden="true">${symbols.join("")}</svg>`;
for (const f of readdirSync(join(root, "docs/assets"))) copyFileSync(join(root, "docs/assets", f), join(out, "assets", f));

const full = [`# Uniflo v${version} — full documentation\n\nSource: ${REPO} · Site: ${SITE}\n`];
for (const p of PAGES) {
  const md = readFileSync(join(root, p[2]), "utf8");
  writeFileSync(join(out, `${p[0]}.html`), docPage(p, render(md, p[2])));
  writeFileSync(join(out, `${p[0]}.md`), md);
  full.push(`\n\n<!-- ${p[2]} -->\n\n${md}`);
}
const home = readFileSync(join(root, "site/home.html"), "utf8").replaceAll("{{version}}", esc(version)).replace("{{icons}}", iconSprite);
writeFileSync(join(out, "index.html"), `${head("Uniflo · 所有 agent 会话，一条实时流", "本机守护进程：把 Claude Code、Codex、Gemini CLI 等 33 个 agent harness 的会话归一成一套 schema，经本地网关与 Rust SDK 供应用和 agent 实时使用。", "quickstart.md")}\n${home.replace("{{topbar}}", topbar("home"))}`);
writeFileSync(join(out, "404.html"), `${head("找不到页面 · Uniflo", "", "")}\n<body class="doc">${topbar("")}<main class="prose" style="margin:12vh auto;text-align:center"><h1>找不到这个页面</h1><p><a href="index.html">回到首页</a> · <a href="llms.txt">llms.txt</a></p></main></body></html>\n`);
writeFileSync(join(out, "llms.txt"), `# Uniflo

> Local daemon that normalizes every AI coding agent's sessions (Claude Code, Codex, Gemini CLI, OpenCode, omp and 28 more) into one wire schema and serves them on 127.0.0.1:7311 over REST, SSE, NDJSON and WebSocket, plus an MCP server and Rust crates for building on top. Version ${version}. Docs are in Chinese; code, field names and commands are verbatim.

Read-only by default, loopback only. Start with Quickstart, then API or SDK. Every page below is plain Markdown; llms-full.txt concatenates all of them.

## Docs

${PAGES.map(([s, t, , d]) => `- [${t}](${SITE}${s}.md): ${d}`).join("\n")}

## Optional

- [llms-full.txt](${SITE}llms-full.txt): all pages above in one file
- [Source](${REPO}): Rust workspace, ADRs in docs/decisions
- [crates.io](https://crates.io/crates/uniflo-client): uniflo-client (daemon client), uniflo-schema (wire types), uniflo-core, uniflo-search, uniflo-gateway, uniflo-adapters (embedding)
`);
writeFileSync(join(out, "llms-full.txt"), full.join(""));
writeFileSync(join(out, "sitemap.xml"), `<?xml version="1.0" encoding="UTF-8"?>\n<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">${["index", ...PAGES.map((p) => p[0])].map((s) => `<url><loc>${SITE}${s === "index" ? "" : s + ".html"}</loc></url>`).join("")}</urlset>\n`);
writeFileSync(join(out, ".nojekyll"), "");
if (!existsSync(join(out, "index.html"))) process.exit(1);
console.log(`site → ${out} (${PAGES.length + 1} pages, v${version})`);
