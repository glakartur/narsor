Intent: Split the single narsor package into client, protocol, engine, server, runner, and controller workspace packages while preserving product behavior. The client must perform task operations through REST/SSE, and the package graph must enforce ownership.

Risk divisions:
1. Client/server package and API boundary (`Cargo.toml`, `src/client_lib.rs`, `crates/narsor-server/**`, `src/api/**`): ensure client operations route through correct server endpoints and API adapters retain behavior.
2. Protocol migration (`crates/narsor-protocol/**`, `src/controller_protocol/**`, domain projections): ensure serialized contracts, defaults, fallback enums, and DTO conversions remain compatible.
3. Process launch and discovery (`src/api/discovery.rs`, `src/daemon/**`, runner/server entrypoints, backend process startup): ensure sibling binary selection, config/root forwarding, auth files, and process cleanup work together.
4. Runner and controller ownership (`crates/narsor-runner/**`, `crates/narsor-controller-runtime/**`, `crates/narsor-controller/**`, moved engine sources): ensure environment isolation, reconciliation, protocol ownership, and Kubernetes bootstrap still compose.
5. CLI/TUI behavior and workflow/material handling (`src/cli/**`, `src/tui/**`, `src/client/**`, related tests): ensure moved frontend paths preserve command semantics, safe file handling, and task operations over API.

Cross-division checks: client API types and standalone server routes must agree; test harness binary paths must point to sibling workspace executables; local reconciler startup must remain owned by the server while the remote controller remains standalone. Review representative paths and the exact diff for missing call sites or behavior.
