# uniflo-client

Typed, blocking Rust client for the [Uniflo](https://github.com/Crosery/uniflo) daemon's local
HTTP gateway (default `http://127.0.0.1:7311`). Uniflo normalizes every agent-harness session on
your machine (Claude Code, Codex, OpenCode, …) into one schema; this crate lets other tools and
agents build on it from crates.io without hand-rolling HTTP calls.

```sh
cargo add uniflo-client
```

```rust
let client = uniflo_client::Client::local()?;
for s in client.sessions("s:work", 20)? {
    println!("{} · {}", s.key, s.title.unwrap_or_default());
}
```

`Client::local()` reads `UNIFLO_URL` (default `http://127.0.0.1:7311`) and `UNIFLO_TOKEN` from
the environment, the same way the `uniflo` CLI does. Use `Client::new(url)` to point at an
explicit daemon instead.

## Scope

Every method is read-only (sessions, transcripts, full-text search, usage/cost reports, resume
commands, agent memory files, and a live NDJSON event stream). There are no write endpoints in
this first version. See the repository's `docs/api.md` for the full REST surface and
`docs/sdk.md` for how this crate relates to `uniflo-schema` (just the wire types) and to embedding
Uniflo without a daemon at all.

This crate depends on nothing but [`uniflo-schema`](https://crates.io/crates/uniflo-schema),
`serde` and `serde_json`: the HTTP transport is a small `std`-only HTTP/1.1 client over
`TcpStream`, so pulling in `uniflo-client` does not pull in an async runtime or a full HTTP stack.

## Examples

```sh
cargo run -p uniflo-client --example watch   # live tool calls across every session
cargo run -p uniflo-client --example usage   # cost by model
```

## Versioning

The wire schema (`uniflo-schema`) is v1 and only grows: new optional fields and new `kind` values
may appear, and this crate's types tolerate them. A breaking change would bump
`uniflo_schema::SCHEMA_VERSION` and ship a major version of this crate; see the repository's
`docs/decisions/ADR-0003-wire-schema-只增不改.md`.

## License

MIT, same as the rest of the Uniflo workspace.
