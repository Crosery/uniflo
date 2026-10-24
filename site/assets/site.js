// Uniflo developer site: theme, copy buttons, tabs, TOC scrollspy, a tiny highlighter, and the
// home page's flow canvas and event stream. No dependencies; animation pauses offscreen, in
// hidden tabs and under prefers-reduced-motion.
(() => {
  const $ = (s, el = document) => el.querySelector(s);
  const $$ = (s, el = document) => [...el.querySelectorAll(s)];
  const calm = matchMedia("(prefers-reduced-motion: reduce)");
  const root = document.documentElement;

  $("#theme")?.addEventListener("click", () => {
    const next = root.dataset.scheme === "light" ? "dark" : "light";
    root.dataset.scheme = next;
    localStorage.setItem("uf-site-theme", next);
  });

  // copy: a button inside .code copies its block; data-copy names the element to copy
  document.addEventListener("click", async (e) => {
    const b = e.target.closest(".copy");
    if (!b) return;
    const src = b.dataset.copy ? $(b.dataset.copy) : b.closest(".code")?.querySelector("pre");
    if (!src) return;
    try {
      await navigator.clipboard.writeText(src.innerText.replace(/\n$/, ""));
      const was = b.textContent;
      b.textContent = "已复制";
      b.classList.add("done");
      setTimeout(() => { b.textContent = was; b.classList.remove("done"); }, 1300);
    } catch {}
  });

  const select = (group, btn) => $$('[role="tab"]', group).forEach((t) => t.setAttribute("aria-selected", String(t === btn)));
  $$(".install .seg button").forEach((b) => b.addEventListener("click", () => {
    select(b.parentElement, b);
    $("#cmd").textContent = b.dataset.cmd;
  }));
  $$("[data-tabs]").forEach((box) => $$(".seg button", box).forEach((b) => b.addEventListener("click", () => {
    select(b.parentElement, b);
    $$(".seg button", box).forEach((t) => { $("#" + t.dataset.tab).hidden = t !== b; });
  })));

  // highlight: comments, strings, numbers, keywords; enough for sh / rust / js / json / toml
  const KW = /\b(let|mut|fn|use|for|in|if|else|match|return|const|async|await|new|pub|impl|struct|true|false|null|None|Some|Ok|Err)\b/g;
  const esc = (s) => s.replace(/&/g, "&amp;").replace(/</g, "&lt;");
  const paint = (code) => {
    const lang = (code.className.match(/lang-(\S+)/) || [, "sh"])[1];
    if (lang === "text") return;
    const comment = /^(sh|bash|shell|toml|text|ps1|powershell|yaml)$/.test(lang) ? /#[^\n]*/ : /\/\/[^\n]*/;
    const re = new RegExp(`(${comment.source})|("(?:\\\\.|[^"\\\\\\n])*"|'(?:\\\\.|[^'\\\\\\n])*')|\\b(\\d[\\d_.]*)\\b`, "g");
    let out = "", last = 0;
    for (const m of code.textContent.matchAll(re)) {
      out += esc(code.textContent.slice(last, m.index)).replace(KW, '<span class="t-k">$1</span>');
      const cls = m[1] ? "t-c" : m[2] ? "t-s" : "t-n";
      out += `<span class="${cls}">${esc(m[0])}</span>`;
      last = m.index + m[0].length;
    }
    code.innerHTML = out + esc(code.textContent.slice(last)).replace(KW, '<span class="t-k">$1</span>');
  };
  $$(".prose pre code, .tabs pre code").forEach(paint);

  // TOC scrollspy
  const toc = $$(".toc a");
  if (toc.length) {
    const byId = new Map(toc.map((a) => [decodeURIComponent(a.hash.slice(1)), a]));
    const heads = $$(".prose h2[id], .prose h3[id]").filter((h) => byId.has(h.id));
    let current;
    const spy = () => {
      let hit = heads[0];
      for (const h of heads) if (h.getBoundingClientRect().top < 120) hit = h;
      if (hit && hit !== current) {
        current?.classList.remove("on");
        current = byId.get(hit.id);
        current.classList.add("on");
      }
    };
    addEventListener("scroll", () => requestAnimationFrame(spy), { passive: true });
    spy();
  }

  // card hover light follows the pointer
  $$(".card").forEach((c) => c.addEventListener("pointermove", (e) => {
    const r = c.getBoundingClientRect();
    c.style.setProperty("--mx", `${e.clientX - r.left}px`);
    c.style.setProperty("--my", `${e.clientY - r.top}px`);
  }));

  // runs `step(now)` each frame while `el` is on screen and the tab is visible
  const loop = (el, step) => {
    let raf = 0, seen = false;
    const tick = (t) => { step(t); raf = requestAnimationFrame(tick); };
    const sync = () => {
      const go = seen && !document.hidden && !calm.matches;
      if (go && !raf) raf = requestAnimationFrame(tick);
      if (!go && raf) { cancelAnimationFrame(raf); raf = 0; }
    };
    new IntersectionObserver(([e]) => { seen = e.isIntersecting; sync(); }).observe(el);
    document.addEventListener("visibilitychange", sync);
    calm.addEventListener("change", sync);
  };

  // flow: curves from each harness to the core and from the core to each consumer, with
  // particles riding them. Layout is read from the DOM, so it follows the CSS breakpoints.
  const flow = $("#flow");
  if (flow) {
    const cv = $("#flow-canvas"), ctx = cv.getContext("2d");
    const core = $("#core .core-ring");
    const srcs = $$(".src li:not(.more)", flow), dsts = $$(".dst li", flow);
    const COLORS = ["#9aa4ff", "#3fcf8e", "#f17cc0", "#f5b452", "#5cc8e8"];
    let paths = [], dots = [], w = 0, h = 0, last = 0;
    const layout = () => {
      const dpr = Math.min(devicePixelRatio || 1, 2), box = flow.getBoundingClientRect();
      w = box.width; h = box.height;
      cv.width = w * dpr; cv.height = h * dpr;
      ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
      const rel = (el) => { const r = el.getBoundingClientRect(); return { x: r.left - box.left, y: r.top - box.top, w: r.width, h: r.height }; };
      const c = rel(core), cx = c.x + c.w / 2, cy = c.y + c.h / 2, rad = c.w / 2;
      const vertical = getComputedStyle(flow).gridTemplateColumns.split(" ").length < 3;
      const link = (el, into) => {
        const r = rel(el);
        let a, b;
        if (vertical) {
          a = into ? { x: r.x + r.w / 2, y: r.y + r.h } : { x: cx, y: cy + rad };
          b = into ? { x: cx, y: cy - rad } : { x: r.x + r.w / 2, y: r.y };
          const m = (b.y - a.y) / 2;
          return { el, pts: [a, { x: a.x, y: a.y + m }, { x: b.x, y: b.y - m }, b] };
        }
        a = into ? { x: r.x + r.w, y: r.y + r.h / 2 } : { x: cx + rad, y: cy };
        b = into ? { x: cx - rad, y: cy } : { x: r.x, y: r.y + r.h / 2 };
        const m = (b.x - a.x) / 2;
        return { el, pts: [a, { x: a.x + m, y: a.y }, { x: b.x - m, y: b.y }, b] };
      };
      paths = [...srcs.map((el) => ({ ...link(el, true), into: true })), ...dsts.map((el) => ({ ...link(el, false), into: false }))];
      draw(0);
    };
    const at = (p, t) => {
      const [a, b, c, d] = p.pts, u = 1 - t;
      return { x: u * u * u * a.x + 3 * u * u * t * b.x + 3 * u * t * t * c.x + t * t * t * d.x, y: u * u * u * a.y + 3 * u * u * t * b.y + 3 * u * t * t * c.y + t * t * t * d.y };
    };
    const line = getComputedStyle(root);
    const draw = (now) => {
      ctx.clearRect(0, 0, w, h);
      ctx.lineWidth = 1;
      ctx.strokeStyle = line.getPropertyValue("--line2").trim() || "#30303c";
      for (const p of paths) {
        const [a, b, c, d] = p.pts;
        ctx.beginPath(); ctx.moveTo(a.x, a.y); ctx.bezierCurveTo(b.x, b.y, c.x, c.y, d.x, d.y); ctx.stroke();
      }
      const dt = Math.min(now - last, 50) / 1000; last = now;
      if (now && Math.random() < 0.18) {
        const ins = paths.filter((p) => p.into), p = ins[(Math.random() * ins.length) | 0];
        if (p) dots.push({ p, t: 0, c: COLORS[(Math.random() * COLORS.length) | 0], v: 0.55 + Math.random() * 0.5 });
      }
      const next = [];
      for (const d of dots) {
        d.t += d.v * dt;
        if (d.t >= 1) {
          d.p.el.classList.remove("hot");
          if (d.p.into) {
            // arriving at the core fans out to one consumer
            const outs = paths.filter((p) => !p.into), o = outs[(Math.random() * outs.length) | 0];
            if (o) next.push({ p: o, t: 0, c: d.c, v: d.v });
          }
          continue;
        }
        if (d.t < 0.15) d.p.el.classList.add("hot");
        else d.p.el.classList.remove("hot");
        const q = at(d.p, d.t), tail = at(d.p, Math.max(0, d.t - 0.06));
        const g = ctx.createLinearGradient(tail.x, tail.y, q.x, q.y);
        g.addColorStop(0, "transparent"); g.addColorStop(1, d.c);
        ctx.strokeStyle = g; ctx.lineWidth = 2;
        ctx.beginPath(); ctx.moveTo(tail.x, tail.y); ctx.lineTo(q.x, q.y); ctx.stroke();
        ctx.fillStyle = d.c; ctx.shadowColor = d.c; ctx.shadowBlur = 10;
        ctx.beginPath(); ctx.arc(q.x, q.y, 2.4, 0, 7); ctx.fill();
        ctx.shadowBlur = 0;
        next.push(d);
      }
      dots = next.slice(-90);
    };
    new ResizeObserver(layout).observe(flow);
    layout();
    loop(flow, draw);
  }

  // stream: synthetic envelopes in the shape of /v1/stream.ndjson, one every ~0.9 s
  const pre = $("#stream");
  if (pre) {
    const H = ["claude", "codex", "gemini", "opencode", "omp", "cursor"];
    const SAMPLE = [
      (h, s) => ({ type: "session", seq: s, session: { key: `${h}:9f3a…`, status: "work", status_reason: "user_message" } }),
      (h, s) => ({ type: "event", seq: s, event: { session: `${h}:9f3a…`, kind: "user_message", text: "修复登录页的缓存击穿" } }),
      (h, s) => ({ type: "event", seq: s, event: { session: `${h}:9f3a…`, kind: "tool_call", name: "Bash", call_id: "c7" } }),
      (h, s) => ({ type: "event", seq: s, event: { session: `${h}:9f3a…`, kind: "tool_result", call_id: "c7", is_error: false } }),
      (h, s) => ({ type: "event", seq: s, event: { session: `${h}:9f3a…`, kind: "usage", input: 1840, output: 312, cost_usd: 0.0123 } }),
      (h, s) => ({ type: "event", seq: s, event: { session: `${h}:9f3a…`, kind: "assistant_message", text: "已修复，加了互斥锁" } }),
      (h, s) => ({ type: "session", seq: s, session: { key: `${h}:9f3a…`, status: "idle", status_reason: "turn_end" } }),
    ];
    const color = (json) => esc(json)
      .replace(/"([a-z_]+)":/g, '<span class="j-k">"$1"</span>:')
      .replace(/: ?"([^"]*)"/g, ': <span class="j-s">"$1"</span>')
      .replace(/: ?(-?\d+(?:\.\d+)?)/g, ': <span class="j-n">$1</span>')
      .replace(/: ?(true|false)/g, ': <span class="j-t">$1</span>');
    let seq = 4810, i = 0, h = H[0], due = 0;
    const push = () => {
      if (i % SAMPLE.length === 0) h = H[(i / SAMPLE.length) % H.length];
      const ln = document.createElement("span");
      ln.className = "ln";
      ln.innerHTML = color(JSON.stringify(SAMPLE[i % SAMPLE.length](h, ++seq)).replace(/,"/g, ', "').replace(/":/g, '": '));
      pre.append(ln);
      while (pre.children.length > 9) pre.firstChild.remove();
      i++;
    };
    for (let k = 0; k < 6; k++) push();
    loop(pre, (now) => { if (now > due) { push(); due = now + 900; } });
  }
})();
