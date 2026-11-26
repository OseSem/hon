# Contributing to hon

Thanks for wanting to help. hon tries to stay small: one binary, nothing extra to install,
no config needed. Changes that keep it that way are the easiest to merge.

## Getting started

You need Rust 1.90 or newer and a C compiler (bundled SQLite and the TLS library are
built from C). On x86_64 Windows you also need [NASM](https://www.nasm.us/).

```sh
git clone https://github.com/OseSem/hon
cd hon
cargo run            # the interactive reader
cargo run -- --help  # the commands
```

Set `HON_HOME` to a scratch directory while hacking, so your real reading progress and
downloads stay untouched:

```sh
HON_HOME=/tmp/hon-dev cargo run
```

## Where code goes

The repo is a Cargo workspace with two crates:

- `crates/hon-core`: the source plugin runtime and repos, search, the library database and
  downloads. No terminal or UI code, so a future GUI or server can use it too.
- `crates/hon`: the `hon` binary, meaning the TUI, the commands, the config file and
  external viewers.

Anything another frontend would also need belongs in `hon-core`.

## Before you open a pull request

```sh
cargo fmt
cargo clippy --all-targets -- -D warnings
cargo test
```

- Tests must not hit the network. Use [wiremock](https://docs.rs/wiremock) for HTTP, and
  plugins written inside the test that target its server. A test that talks to a real
  service is marked `#[ignore]`; run those yourself with `cargo test -- --ignored`.
- If you change a command or flag, regenerate the man page:
  `cargo run -- man > man/hon.1` (a test fails until you do).
- Commit messages follow [Conventional Commits](https://www.conventionalcommits.org/):
  `feat(plugins): …`, `fix(tui): …`, `docs: …`.
- Changes that only touch `.md` files skip CI.

## Sources

Sources aren't accepted into hon itself. hon ships with none: each source is a Lua plugin
in a source repo, and users add the repos they want. To write one, or to publish a repo,
see [docs/plugins.md](docs/plugins.md).

Changes to the plugin system itself (`crates/hon-core/src/plugins/`) are welcome here.
Keep `docs/plugins.md` in step with them.

## Reporting bugs

Open an issue with what you ran, what you expected and what happened. For display
problems, say which terminal you use (Kitty, Ghostty, WezTerm, iTerm2, Windows Terminal…)
and include a screenshot if you can. `hon --version` helps too.
