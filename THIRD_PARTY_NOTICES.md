# 第三方声明

> 随 Uniflo 源码与二进制分发的第三方作品、来源与许可证。

状态：`current` · 更新：2026-10-10

## lobe-icons（harness 品牌图标）

- 来源：[lobehub/lobe-icons](https://github.com/lobehub/lobe-icons)，npm 包 `@lobehub/icons-static-svg` 1.95.1（`sha512-Hw7EPPgVnC4NZLXBfTNJG6hyQgqECfUPC11VVXodPSr1aebKcFxDZlSpxhWwYNdCc6bhxps/x5TtXoPmfKH2ag==`），取各图标的单色版（不带 `-color` 后缀的文件），2026-10-10 核对。
- 用在：网页演示 `examples/web/index.html` 与其内嵌副本 `crates/uniflo-gateway/src/index.html` 的 `#harness-icons` 精灵图；网关 `GET /v1/harnesses/{id}/icon.svg` 从内嵌副本里取同一份。
- 改动：每个图标只保留 `<path>` 的 `d`、`clip-rule`、`opacity` 属性，改成 `<symbol id="hi-<harness id>">`；去掉 `<title>`，以及 OpenClaw 图标里未使用的渐变定义和覆盖整个画布的裁剪路径。
- 对应关系：claude ← `claudecode`，gemini ← `geminicli`，copilot ← `githubcopilot`，kilo ← `kilocode`，roo ← `roocode`，hermes ← `hermesagent`，mimocode ← `xiaomimimo`，其余同名（antigravity、cline、codebuddy、codex、commandcode、cursor、devin、grok、kimi、kiro、minimax、opencode、openclaw、qoder、qwen）。没有对应图标的 harness 显示字母块。
- 商标归各自所有者。图标只用来标识对应的 harness，不表示这些产品或其所有者与 Uniflo 有关联或为其背书。

```text
MIT License

Copyright (c) 2023 LobeHub

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```
