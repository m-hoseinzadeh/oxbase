# Contributing to Oxbase

Thanks for your interest in Oxbase. This page explains how to send a change.

## Before you start

- For anything bigger than a small fix, open an issue first so we can agree on the approach.
- [PLAN.md](PLAN.md) describes the architecture and milestones. The most important rule: each
  database implements the same per-object roles (Fetcher, DdlGenerator, Comparator, Capabilities,
  DmlWriter). Avoid one-off, engine-specific code paths outside that structure.
- Clean-room rule: never copy code, assets, or text from other database tools, including
  Navicat. Reimplement behavior from public documentation and your own testing.

## Contributor License Agreement

Every outside contributor must sign the [Contributor License Agreement](CLA.md) before a pull
request can be merged. You keep the copyright in your work. The CLA gives the project the right to
use and relicense your contribution, including in a future paid edition of Oxbase (the project is
"open core").

Signing takes one comment. When you open your first pull request, the CLA Assistant bot replies
with instructions. Reply on the pull request with:

```
I have read the CLA Document and I hereby sign the CLA
```

If you contribute as part of your job, your employer may need to sign too. See section 8 of the
CLA, or email hoseinzadeh.ashraf@gmail.com.

## Development setup and checks

See the [README](README.md) for prerequisites and the full command list. Before opening a pull
request, run:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cd ui && pnpm install --frozen-lockfile && pnpm run lint && pnpm run typecheck && pnpm run test
```

Changes to drivers or dialects should also pass the live conformance suite:

```bash
scripts/fixtures-up.sh
cargo test -p oxbase-conformance --locked -- --include-ignored
scripts/fixtures-down.sh
```

## Pull requests

- Keep each pull request focused on one change, with tests.
- Describe what changed and why, and how you tested it.
- New dependencies must have licenses compatible with Apache-2.0. `cargo deny check` in CI
  enforces this.
- Never commit credentials, tokens, or real connection details. The test-database passwords in
  `tests/fixtures.toml` are throwaway values for local Docker containers only.

## License

By contributing, you agree that your contributions are licensed under the Apache License 2.0 and
the terms of the [CLA](CLA.md).
