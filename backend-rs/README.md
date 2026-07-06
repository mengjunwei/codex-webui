# backend-rs

Rust rewrite of the codex-webui backend (replaces the NestJS backend in `../src`).

## Status

Placeholder. The Cargo workspace scaffolding lands in **Phase 0** of the
implementation plan.

## Reference

- Design spec: `../docs/superpowers/specs/2026-07-06-codex-webui-rust-migration-design.md`
- Existing TS backend (reference oracle during migration): `../src`

## Goals (per spec)

- **A** Performance / resource footprint
- **B** Single self-contained binary
- **C** Type safety + long-term maintainability
- Bar: production-usable, behavioral parity with the TS backend.

The API contract (REST routes, Socket.IO namespace/events, OpenAPI operationIds,
error-code strings) is preserved verbatim so the React frontend in `../web`
needs no changes.
