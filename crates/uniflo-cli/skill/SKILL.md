---
name: uniflo
description: Look up local AI coding-agent sessions (Claude Code, Codex, omp, OpenCode, Gemini CLI, Cursor, …) through Uniflo — list and search sessions, read transcripts, full-text search, token usage and cost, the command that resumes a session, agent memory files, and recent sessions of the current project. Use when asked what another agent did or is doing, to find an earlier conversation, to check costs, or to continue a session.
---

# Uniflo

Uniflo is a local daemon that indexes every agent harness session on this machine into one
schema. Query it with the `uniflo` CLI (add `--json` for machine-readable output) or its REST
API at `http://127.0.0.1:7311` (`UNIFLO_URL` overrides; send `Authorization: Bearer $UNIFLO_TOKEN`
when a token is configured). Everything here is read-only.

If the `uniflo_*` MCP tools are available, prefer them: they return the same JSON as REST.

## Session keys

A session is `<harness>:<id>`, e.g. `claude:4f1c…`. CLI commands accept a unique key or id
prefix. In URLs, percent-encode the key (`claude%3A4f1c`). A reference to one event looks like
`uniflo://session/<key>#<event-id>`.

## Find sessions

```sh
uniflo ls 's:work' --json                      # working now
uniflo ls 'h:codex in:myrepo since:2d' --json  # harness, cwd substring, recency
uniflo ls "'deploy script" -n 10 --json        # fuzzy title / first prompt / cwd
curl -s "$UNIFLO_URL/v1/sessions?limit=20&q=$(printf %s 'h:claude since:1d' | jq -sRr @uri)"
```

Filters: `h:`/`harness:`, `s:work|idle`, `in:`/`cwd:` (substring), `since:2h|7d|2026-10-01`,
`before:`, `is:root|sub|live`, `id:<prefix>`, `parent:<key>`; `!` negates. Other words are an
fzf-style fuzzy match.

## Read a session

```sh
uniflo show <key>                     # metadata + latest events
uniflo tail <key> -n 100 --json       # last 100 events as NDJSON
curl -s "$UNIFLO_URL/v1/sessions/<enc key>/events?limit=100&max_text=4000"
curl -s "$UNIFLO_URL/v1/sessions/<enc key>/events?around=<event-id>&limit=20"
```

Events are `user_message`, `assistant_message`, `reasoning`, `tool_call`, `tool_result`,
`turn_start`, `turn_end`, `usage`, `system`. Pages go backwards: pass `before=<next_before>`.

## Full-text search

```sh
uniflo grep 缓存击穿 --filter 'h:claude since:7d' --json
uniflo grep '"exact phrase" -excluded' --json
curl -sG "$UNIFLO_URL/v1/search" --data-urlencode 'q=parse_rel' --data-urlencode 'filter=in:uniflo'
```

Hits are grouped by session; open one with `events?around=<hit.event>`.

## Usage and cost

```sh
uniflo usage --by model --since 7d --json     # also harness, project, cwd, dir, day, hour, weekday, session
uniflo usage <key> --json                     # every step of one session
curl -s "$UNIFLO_URL/v1/usage?group_by=project&since=30d"
```

`cost_usd` is the API-equivalent cost; steps with an unknown model are counted in
`unpriced_steps`, never as $0.

## Resume, context, memory

```sh
uniflo resume <key> --print           # the shell command that continues the session (exit 2: unsupported)
uniflo context --cwd . --limit 5      # recent sessions of this project (git root), Markdown
curl -s "$UNIFLO_URL/v1/sessions/<enc key>/resume"
curl -s "$UNIFLO_URL/v1/memory?cwd=$PWD"            # CLAUDE.md / AGENTS.md / memory files that apply here
curl -s "$UNIFLO_URL/v1/memory/file?path=<listed path>"
```

## When the daemon is not running

CLI commands index in-process (slower, one-shot); `uniflo context` prints nothing. Start it
with `uniflo daemon`.
