# Refract Contracts

The public TypeScript boundary for Refract's management API.

`src/admin-api.ts` mirrors the JSON shape emitted by the Rust management API;
`src/protocol.ts` contains protocol identifiers shared by the admin console and
the public homepage. This package contains types and stable protocol metadata,
not UI components, state, or HTTP transport code.

The Rust serde payload remains the wire-format source of truth. When that
payload changes, update this package in the same change and run both frontend
type checks.
