# Neemata Proxy

## What is this?

Neemata Proxy is a Rust reverse proxy exposed via N-API. It sits alongside [Neemata](https://github.com/neematajs/neemata) runtime workers, accepts incoming traffic, and routes requests to the right worker based on routing rules.

It is built on top of [Pingora](https://github.com/cloudflare/pingora) for its proxying and load-balancing core, while keeping a small JavaScript-facing surface for integration into the [Neemata](https://github.com/neematajs/neemata) runtime.

The Node.js integration is implemented with [napi-rs](https://github.com/napi-rs/napi-rs), which builds [Pingora](https://github.com/cloudflare/pingora) the native addon layer used by the runtime.

## Why does it exist?

The proxy separates request routing and transport work from application logic so that [Neemata](https://github.com/neematajs/neemata) servers can stay focused on RPC handling. This makes the runtime simpler and keeps the networking concerns in a dedicated component that can evolve independently.

It also provides a single place to manage upstreams and routing for multiple runtime workers, which keeps orchestration consistent across transports.

## Related projects

- Neemata framework: https://github.com/neematajs/neemata
- Pingora: https://github.com/cloudflare/pingora
- N-API bindings: https://github.com/napi-rs/napi-rs

## Operational notes

- Dynamic upstream changes are eventually consistent with health checks. After `addUpstream()` or `start()`, a backend does not become routable until its health-check loop marks it healthy, so callers should expect a short convergence window where requests may still receive `503`.
- The convergence window is controlled by `healthCheckIntervalMs`. Tests in this repository use polling helpers for that reason, and production callers should follow the same pattern when they need to wait for a backend to become ready.
