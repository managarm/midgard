## Formatting and linting

Every commit must pass the checks that CI runs (see `.github/workflows/ci.yml`):

```
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

This applies to each commit individually, not only to the tip of a branch, such that
`git bisect` works and CI is green on any commit.

## Commit messages

The commit message convention follows Managarm's.

```
<module>: <A sentence describing what was changed>
```

Modules describe where the change is made:
- `bifrost`, `midgard`, `midgard-hv`, `midgard-protocol`, `midgard-vmm`, `midgard-init`: the crate of that name in `crates/`.
- `guest`: the guest configuration in `guest/` (e.g. the Linux kernel config).
- `ci`: the CI workflow in `.github/`.
- `build`: the Cargo workspace (`Cargo.toml`, `Cargo.lock`).
- `meta`: meta changes, like this file or a README update.

If a commit spans multiple modules, list them separated by commas, e.g.
`midgard-hv, ci: Add the hypervisor interface with a KVM backend`.

Commit messages must be a single subject line — no blank line, no body paragraphs, no bullet lists.
Keep the commit line short and let the diff speak for itself.
The only thing allowed below the subject is the `Assisted-by:` trailer
(separated by one blank line):

```
midgard-vmm: Generate the MADT from the vCPU count

Assisted-by: Claude Code (Opus 5.5)
```

Use the model that authored the commit (e.g. `Claude Code (Opus 5.5)`). Only deviate from the
one-line rule if the user explicitly asks for a longer message.

## Commit structure

- Each commit addresses one specific concern.
  If reasonable, split a diff that touches multiple subsystems into one commit per subsystem.
- Commits must be self-contained: each commit must build and pass the checks above on its own.
- Keep the history linear: no merge commits.