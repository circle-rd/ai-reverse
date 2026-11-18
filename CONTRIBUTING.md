# Contributing to ai-reverse

Read [AGENTS.md](AGENTS.md) first; it is the canonical engineering and security
standard for this repository.

## Local checks

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-targets --all-features --locked
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --locked
cargo audit
docker build -t reverse-agent .
```

All of these must pass before review. CI re-runs them on every pull request and
on pushes to `main` and `develop`.

## Workflow

1. Branch from `develop`.
2. Keep changes focused; add tests and documentation in the same change.
3. Use English for code, comments, commit messages and documentation.
4. Do not commit secrets, credentials, `.env` files or build output.
5. Open a pull request into `develop`. `main` is the production branch and is
   updated from validated `develop` changes.

## Contract changes

The agent/relay wire contract lives in `crates/reverse-core/src/wire.rs` and the
validated identifier rules in `crates/reverse-core/src/ids.rs`. When the wire
contract or the reserved identifier list changes, update the shared types, the
agent tests and this documentation in the same change.

## Local validation workspace

`validation/` is deliberately ignored by git (see AGENTS.md section 10,
exception E1): it holds the relay crate, the LiteLLM PoC recipes and the local
development stacks. Never commit anything under `validation/`; relay work
happens there and is reviewed out of band until the exception is revisited.

## License

By contributing, you agree that your contribution is licensed under the MIT
License (see [LICENSE](LICENSE)).
