Review constraints distilled from active project instructions and the implementation plan:

- Preserve task, workflow, API, SSE, authentication, recovery, and persisted-data behavior.
- Keep the client free of normal Cargo dependencies on server, engine, persistence, and runner implementation.
- Keep API wire types independent from internal engine and persistence types.
- Preserve discovery file ownership, atomic publication, and owner-only permissions.
- Preserve runner HOME and credential isolation, process cleanup, and monitoring semantics.
- Do not disturb pre-existing user work or commit/publish changes.
