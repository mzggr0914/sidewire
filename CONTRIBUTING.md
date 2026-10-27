# Contributing

Bug reports and focused pull requests are welcome.

Use Node.js 24 and pnpm 12.3.4 (the version pinned in both package manifests).
Before opening a PR, install the formatter dependencies from the repository root,
then check formatting:

```text
pnpm install --frozen-lockfile
pnpm format:check
```

Run `pnpm format` to fix Prettier formatting. Prettier formats
Java, JavaScript, CSS, HTML, JSON, YAML, Markdown and shell scripts, including
the extensionless `module/bin/sidewirectl`. CI checks this on Windows and Linux.
Rust formatting is independent: use `cargo fmt --all` to fix it. The Rust CI job
runs `cargo fmt --all -- --check` separately; pnpm commands do not invoke Cargo.
Generated files and dependency lockfiles are excluded. PowerShell and TOML
are not covered by this command; `.editorconfig` records their basic style.
PowerShell would need a separate PSScriptAnalyzer formatter, and TOML would
need a formatter such as Taplo before they can be enforced in CI.
To block merges on formatting failures, repository rules must mark the two
`Formatting` jobs as required checks. Adding the workflow alone reports failures
but does not configure branch protection.

Run the Rust checks separately:

```text
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

If you changed the WebUI, also run `pnpm build` from `webui/`. If you changed `sidewired`, make sure the Android arm64 target still builds.

A few preferences:

- keep changes scoped; avoid unrelated cleanup in the same PR
- preserve secure-by-default behavior
- do not add automatic secure → insecure fallback
- note any protocol or capability change explicitly
- include real-device details when reporting Android-specific behavior

For security issues, use the process in [SECURITY.md](SECURITY.md) instead of a public issue.
