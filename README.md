# Neemata Proxy

## What is this?

Neemata Proxy is a Rust reverse proxy exposed via N-API. It sits alongside Neemata runtime workers, accepts incoming traffic, and routes requests to the right worker based on routing rules.

It is built on top of Pingora for its proxying and load-balancing core, while keeping a small JavaScript-facing surface for integration into the Neemata runtime.

## Why does it exist?

The proxy separates request routing and transport work from application logic so that Neemata servers can stay focused on RPC handling. This makes the runtime simpler and keeps the networking concerns in a dedicated component that can evolve independently.

It also provides a single place to manage upstreams and routing for multiple runtime workers, which keeps orchestration consistent across transports.

## Related projects

- Neemata framework: https://github.com/neematajs/neemata
- Pingora: https://github.com/cloudflare/pingora
- N-API bindings: https://github.com/napi-rs/napi-rs
