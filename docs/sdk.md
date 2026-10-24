# Rust SDK

> 面向 Rust 使用者的接入方式：`uniflo-client` 连接运行中的守护进程；`uniflo-schema` 只要类型；`uniflo-core` + `uniflo-adapters`（可选 `uniflo-search`、`uniflo-gateway`）在自己的程序里嵌入 Uniflo，不跑独立守护进程。

状态：`current` · 更新：2026-10-24

## 选哪个 crate

| 场景 | 用 | 额外依赖 |
|---|---|---|
| 守护进程已经在跑（`uniflo daemon`），你的程序只是查询它 | `uniflo-client` | 只有 `uniflo-schema`、`serde`、`serde_json` |
| 自己发 HTTP 请求（别的语言的绑定、WASM…），只要类型和 JSON 形状 | `uniflo-schema` | 只有 `serde`、`serde_json` |
| 想直接拿到会话数据，不想跑独立进程、不想开端口 | `uniflo-core` + `uniflo-adapters` | 见下 |
| 以上基础上还要全文检索 | 加 `uniflo-search` | 依赖 `uniflo-core`（内嵌 `rusqlite`） |
| 以上基础上还要一套 HTTP 接口（挂进自己的 axum 程序，或单独起监听） | 加 `uniflo-gateway` | 依赖 `uniflo-core`、`uniflo-search`、`axum` |

依赖只能向下指（`docs/conventions/DEVELOPMENT.md#模块边界`）：`uniflo-schema ← uniflo-core ← uniflo-search / uniflo-adapters ← uniflo-gateway ← uniflo-cli`；`uniflo-client` 不在这条链上，只依赖 `uniflo-schema`。

## 安装

```sh
cargo add uniflo-client                   # 连接运行中的守护进程
cargo add uniflo-schema                   # 只要类型
cargo add uniflo-core uniflo-adapters     # 嵌入式索引（继续看下面两节）
cargo add uniflo-search                   # + 全文检索
cargo add uniflo-gateway                  # + 自己的 HTTP 接口
```

`uniflo-adapters` 默认不开任何 harness 的 cargo feature，用得到哪个 harness 就开哪个（`claude`、`codex`、`opencode`……），或者 `--features default` 开全部；清单见 `docs/adapters.md`。

## `uniflo-client`：连接运行中的守护进程

```rust
let client = uniflo_client::Client::local()?;
for s in client.sessions("s:work", 20)? {
    println!("{} · {}", s.key, s.title.unwrap_or_default());
}
```

这段也是 `uniflo-client` crate 文档首页的例子（`cargo doc -p uniflo-client` 可本地生成）。`Client::local()` 读 `UNIFLO_URL`（默认 `http://127.0.0.1:7311`）和 `UNIFLO_TOKEN`，和 `uniflo` CLI 一致；也可以 `Client::new("http://host:port")` 指定地址，`.with_token(..)` / `.with_timeout(..)` 覆盖默认值。

方法覆盖的路由：`health`（`/v1/health`）、`sessions` / `session`（`/v1/sessions`）、`events`（`/v1/sessions/{key}/events`，含 `around` 窗口）、`search`（`/v1/search`）、`usage` / `session_usage`（`/v1/usage`、`/v1/sessions/{key}/usage`）、`models`（`/v1/models`）、`harnesses`（`/v1/harnesses`）、`resume`（`/v1/sessions/{key}/resume`）、`memory` / `memory_file`（`/v1/memory`、`/v1/memory/file`），以及 `stream`（`/v1/stream.ndjson`，返回 `Iterator<Item = Result<Envelope, Error>>` 的实时流）。另有 `get_json::<T>(path_and_query)` 作为打任何别的路径的后门。全部只读，这一版没有写接口（会话清理、归档删除、开终端都不在内）。字段形状、参数与错误码见 `docs/api.md`；具体类型见 `docs/schema.md`。

## `uniflo-schema`：只要类型

```rust
use uniflo_schema::{Body, Event};

fn is_tool_call(e: &Event) -> bool {
    matches!(e.body, Body::ToolCall { .. })
}
```

`Session` / `Event` / `Envelope` 是整个项目唯一对外契约：事件按 `kind` 打平（`#[serde(tag = "kind")]`），同 `id` 再次出现即覆盖（流式更新）。v1 只增不改——只新增可选字段和新的 `kind` 取值，解析时应忽略不认识的字段；破坏性修改才会升 `SCHEMA_VERSION`，见 `docs/decisions/ADR-0003-wire-schema-只增不改.md`。自己实现 HTTP 调用、或者在别的语言里对着同一套 JSON 形状写绑定时，只需要这个 crate；`uniflo-client` 本身也只依赖它。

## 嵌入式使用：不跑独立守护进程

直接依赖 `uniflo-core` + `uniflo-adapters`，在自己的进程里建一次索引：

```rust
use uniflo_core::{Engine, EngineOptions};

let adapters = uniflo_adapters::all();
let engine = Engine::new(adapters, EngineOptions::default());
engine.index();                 // 头尾摘要，立刻可查
let sessions = engine.sessions();
println!("{} sessions", sessions.len());
```

`Engine::new` 返回 `Arc<Engine>`；`index()` 只读文件头尾，`sessions()` / `session(key)` / `history(key, &q)` 之后都是纯内存查询。需要持续跟随文件变化（新事件、状态变化）时再跑后台循环，它是异步的，需要 Tokio 运行时：

```rust,ignore
tokio::spawn(engine.clone().run());
```

只想一次性查询、不跑后台循环时，用 `engine.index_usage()` 建好用量账本后再调 `engine.usage_report(..)` / `engine.session_usage(..)` / `engine.models(..)`（都是同步方法）。

需要全文检索再加 `uniflo-search`（也是同步的，自己起写线程，不需要 Tokio）：

```rust,ignore
use uniflo_search::fts::{Fts, FtsOptions};

let fts = Fts::start(engine.clone(), FtsOptions::default())?; // 索引放 Uniflo 缓存目录；嵌入时通常改 FtsOptions.path
let hits = fts.search(&uniflo_search::fts::SearchParams { q: "缓存击穿".into(), ..Default::default() })?;
```

需要一套 HTTP 接口（给自己的桌面端/网页端用，或挂进已有的 axum 程序）再加 `uniflo-gateway`：返回的是一个普通 `axum::Router`，可以直接 `axum::serve`，也可以 `.merge()` 进已有的 app：

```rust,ignore
use std::sync::Arc;
use uniflo_gateway::{GuardOptions, router_with_fts};

let router = router_with_fts(engine.clone(), GuardOptions::default(), Some(Arc::new(fts)));
// 单独起监听：
let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
uniflo_gateway::serve(listener, router, std::future::pending()).await?;
```

细节（读取模型、状态机、性能预算、模块边界、隐私红线）见 `docs/architecture.md`、`docs/conventions/DEVELOPMENT.md`。

## 版本

wire schema 当前 `SCHEMA_VERSION = 1`，只增不改：客户端应忽略不认识的字段与 `kind` 取值。决策见 `docs/decisions/ADR-0003-wire-schema-只增不改.md`。这些 crate 随整个 workspace 一起发版（根 `Cargo.toml` 的 `[workspace.package].version`），不是各自独立的语义化版本节奏；破坏性的 wire 变更会同时升 `SCHEMA_VERSION` 和这些 crate 的主版本号。

## 参考

- `docs/api.md`：网关 REST / SSE / NDJSON / WebSocket 的完整路由、参数与安全规则。
- `docs/schema.md`：`Session` / `Event` / `Envelope` 的字段定义。
- `docs/architecture.md`：数据流、嵌入式使用的扩展点。
- `docs/adapters.md`：`uniflo-adapters` 支持的 harness 与 cargo feature 名。
