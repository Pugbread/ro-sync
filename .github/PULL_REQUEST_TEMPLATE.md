## What changed

<!-- Describe the user-visible outcome, not only the implementation. -->

## Why

<!-- Explain the workflow, bug, or safety property this change addresses. -->

## Verification

- [ ] `cargo fmt --check` in `daemon/`
- [ ] `cargo test --locked` in `daemon/`
- [ ] `cargo clippy --locked --all-targets -- -D warnings` in `daemon/`
- [ ] Widget JavaScript syntax and platform-command checks
- [ ] Plugin bytecode/build checks when Luau changed
- [ ] Generated command docs rebuilt when the CLI changed
- [ ] Desktop checks when Tauri code or shared frontend code changed

## Safety

- [ ] No credentials, project source, runtime logs, or generated captures are included.
- [ ] Existing Terminal 64 and CLI-only workflows remain compatible, or the migration is documented.
- [ ] New Studio writes are scoped, auditable, and reversible where practical.
