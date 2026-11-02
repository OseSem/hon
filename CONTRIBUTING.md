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

## Before you open a pull request

```sh
cargo fmt
cargo clippy --all-targets -- -D warnings
cargo test
```

- Tests must not hit the network. Sources are tested against saved pages in
  `tests/fixtures/` served by [wiremock](https://docs.rs/wiremock). Tests that talk to the
  real sites are marked `#[ignore]`; run them yourself with `cargo test -- --ignored`.
- If you change a command or flag, regenerate the man page:
  `cargo run -- man > man/hon.1` (a test fails until you do).
- Commit messages follow [Conventional Commits](https://www.conventionalcommits.org/):
  `feat(sources): add Foo`, `fix(tui): …`, `docs: …`.
- Changes that only touch `.md` files skip CI.

## Adding a source

Sources are plain Rust, compiled into the binary. A source is a type that implements the
`Source` trait in `src/sources/mod.rs`:

```rust
#[async_trait::async_trait]
pub trait Source: Send + Sync {
    fn id(&self) -> SourceId;
    async fn search(&self, query: &str) -> anyhow::Result<Vec<MangaRef>>;
    async fn chapters(&self, manga: &MangaRef) -> anyhow::Result<Vec<ChapterRef>>;
    async fn pages(&self, chapter: &ChapterRef) -> anyhow::Result<Vec<PageRef>>;
    async fn fetch_page(&self, page: &PageRef) -> anyhow::Result<Vec<u8>>;
}
```

To add one:

1. Add a variant to `SourceId` in `src/model.rs`, plus its display name, its slug (used by
   `--source`) and an entry in `SourceId::ALL`.
2. Create `src/sources/<name>.rs` with the implementation. `mangadex.rs` (a JSON API) and
   `mangakatana.rs` (HTML scraping) are the two examples to copy from.
3. Register it with a `pub mod` line and one entry in `all()` in `src/sources/mod.rs`.
4. Save a search page, a series page and a chapter page under `tests/fixtures/<name>/` and
   test against them with wiremock, the same way the existing sources do.
5. Add a row to the Sources table in `README.md`.

What a source must do:

- `chapters()` returns English chapters only, sorted ascending, one entry per chapter
  number. Chapter numbers can be fractional (`10.5`).
- Bad or changed pages return an error that names the source. Never panic on network data.
- Be gentle with the site: respect rate limits and retry sparingly.

What hon won't accept: sources that get around paywalls, logins or DRM on official paid
services. Sources only read what a site serves openly.

## Reporting bugs

Open an issue with what you ran, what you expected and what happened. For display
problems, say which terminal you use (Kitty, Ghostty, WezTerm, iTerm2, Windows Terminal…)
and include a screenshot if you can. `hon --version` helps too.
