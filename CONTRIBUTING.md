# Contributing

Thanks for your interest in the project. This document covers the workflow and
the checks a change needs to pass.

## Getting set up

See [Getting Started](README.md#getting-started) in the README for prerequisites
(Rust, RabbitMQ, CockroachDB/PostgreSQL) and configuration.

Install the pre-commit hooks so formatting and lints run before each commit:

```bash
pip install pre-commit
pre-commit install
```

## Checks

CI runs these on every pull request, whatever branch it targets. Run them
locally first — they are the same commands:

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

Note `--all-targets`: without it clippy skips test code.

Tests requiring a live broker or database are behind a feature flag and do not
run by default or in CI:

```bash
cargo test --features integration-tests
```

Chart changes are validated by the Deploy Checks workflow, which runs
`helm lint` and renders the templates against every values file.

## Pull requests

- **One concern per pull request.** Small, focused changes are easier to review
  and far easier to merge.
- **Explain the reasoning, not just the diff.** Why this approach, what was
  rejected, and what a reviewer should look at hardest.
- **Say what you verified and what you did not.** An untested path is fine as
  long as it is called out; an untested path described as tested is not.

### Stacked pull requests

When a change depends on another, base it on that branch and say so in the
description. **Merge stacked pull requests in order.**

This matters more than it sounds. Merging out of order once produced a `main`
that did not compile: two branches removed adjacent lines from `src/lib.rs`, git
offered the conflict as a choice between the two, and the correct resolution —
dropping *both* lines — looked like neither side. Keeping the stack linear and
merging bottom-up avoids the whole class of problem.

## Commit messages

Explain what changed and why. If a change fixes something subtle, say what the
old behaviour was — the next reader will not have your context.

## Security

Please do not open public issues for security problems. See
[SECURITY.md](SECURITY.md) for how to report them.

## License

By contributing you agree that your contributions are licensed under the MIT
License, the same as the rest of the project.
