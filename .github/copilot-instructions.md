# Neemata Proxy (Rust + NAPI) – AI Contributor Guide

## Big picture
- The proxy is a Rust `cdylib` exposed to Node via `napi-rs`; the public JS API is the `Proxy` class in [../src/proxy.rs](../src/proxy.rs).
- Routing is lock‑free on the hot path: `RouterConfig` lives in an `ArcSwap` and is updated on config changes; request handling reads without locks in [../src/router.rs](../src/router.rs).
- Upstreams are managed per app/transport; health checks run in background tasks (Pingora LB) managed by a small service registry in [../src/server.rs](../src/server.rs) and [../src/lb.rs](../src/lb.rs).
- Listener binding is done manually to surface bind errors to JS (Pingora’s default listener panics); see start flow in [../src/proxy.rs](../src/proxy.rs).
- Behavior and JS API contract are specified in [../SPEC.md](../SPEC.md); tests mirror these semantics in [../test/server.spec.ts](../test/server.spec.ts).

## Key flows & boundaries
- `Proxy.start()` binds a `TcpListener`, configures TLS, then hands the fd to Pingora; state machine is `Stopped → Starting → Running → Stopping` in [../src/proxy.rs](../src/proxy.rs).
- `Proxy.add_upstream()` / `remove_upstream()` update in‑memory pools and refresh router config even when stopped; when running, they also upsert/remove LB health tasks in [../src/proxy.rs](../src/proxy.rs).
- Routing precedence is `subdomain → path → default`; `path` routing rewrites the first segment (e.g. `/auth/x` → `/x`) in [../src/router.rs](../src/router.rs).
- Upgrade/WebSocket requests must use HTTP/1 pools; HTTP/2 upstreams are only for non‑upgrade requests (selection logic in [../src/router.rs](../src/router.rs)).

## Project-specific conventions
- Stable JS error `code` values are required; define or reuse codes in [../src/errors.rs](../src/errors.rs) and return them via `napi` errors.
- Only `type: "port"` upstreams are supported; `unix_socket` is rejected (see parsing in [../src/proxy.rs](../src/proxy.rs)).
- Secure upstreams require a verify hostname derived from routing; for `path`/`default`, `ApplicationOptions.sni` is mandatory (see [../src/proxy.rs](../src/proxy.rs)).
- Options validation lives in [../src/options.rs](../src/options.rs); keep parsing + validation centralized there.

## Workflows
- Build (Node + Rust NAPI): `pnpm build` (runs `napi build` via `package.json`).
- Debug build: `pnpm build:debug` (outputs to `dist/`).
- Tests: `pnpm test` (runs `cargo test` and `vitest run`).
- Rust formatting/lint: `pnpm fmt` / `pnpm lint`.

## Where to look first
- JS API + state machine: [../src/proxy.rs](../src/proxy.rs)
- Routing logic & path rewrite: [../src/router.rs](../src/router.rs)
- Option parsing/validation: [../src/options.rs](../src/options.rs)
- Error codes: [../src/errors.rs](../src/errors.rs)
- LB health checks: [../src/lb.rs](../src/lb.rs)
- Spec + end‑to‑end tests: [../SPEC.md](../SPEC.md), [../test/server.spec.ts](../test/server.spec.ts)
