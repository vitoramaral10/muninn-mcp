# Contributing

Issues and pull requests are welcome. For a change bigger than a bug fix, open an issue first so we can agree on the approach.

## Setup

The toolchain is pinned in `rust-toolchain.toml`; `rustup` installs it on the first build. SQLite is compiled in, so no system libraries are needed.

```sh
cargo build
cargo test
```

To run the server against a folder of notes:

```sh
MUNINN_NOTES_DIR=./notes MUNINN_DATA_DIR=./data MCP_HOST=127.0.0.1 cargo run -- serve
```

Without Rust installed, the same commands run in the toolchain image:

```sh
docker run --rm -v "$PWD:/src" -w /src rust:1.99.1-alpine3.24 \
  sh -c 'apk add -q musl-dev && cargo test'
```

## Before a pull request

CI runs these three and builds the Docker image:

```sh
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test
```

- **Tests.** A change in behavior comes with a test. A bug fix comes with a test that failed before the fix.
- **Tool descriptions and parameters.** If you change one, update [`docs/tools.md`](docs/tools.md) to match. The test `tool_reference_matches_the_code` fails when the two differ.
- **Parsing.** If you change how notes are parsed (`note.rs`, or `chunks` in `store.rs`), bump `PARSER_VERSION` in `store.rs`, so existing indexes re-read every note.
- **Compatibility.** The note format is the contract: notes must stay plain Markdown that any editor reads, and a change must keep existing notes parsing the same way.
- **Changelog.** Add a line under `[Unreleased]` in [`CHANGELOG.md`](CHANGELOG.md) for anything a user would notice.

## Commits

Commit messages follow [Conventional Commits](https://www.conventionalcommits.org): `feat:`, `fix:`, `docs:`, `refactor:`, `test:`, `chore:`. Write the subject in the imperative and keep it under 72 characters.

## Code style

`rustfmt.toml` sets the format; clippy with `-D warnings` sets the rest. Comments say why, not what: a reader can see what the code does. Each module opens with a `//!` comment saying what it holds.
