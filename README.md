# hon

hon reads manga in your terminal. It searches MangaDex and MangaKatana at once, shows
which source has the most chapters, and opens pages right in the terminal or downloads
them as PDF, CBZ or plain images for your own viewer. It remembers which chapters you've
read and where you stopped. It's a single binary with no runtime dependencies, and works
without a config file.

## Install

| Platform | Command |
| --- | --- |
| macOS (Homebrew) | `brew install OseSem/tap/hon` |
| Arch Linux (AUR) | `yay -S hon-bin` (or `paru -S hon-bin`) |
| Windows (Scoop) | `scoop bucket add osesem https://github.com/OseSem/scoop-bucket` then `scoop install hon` |
| macOS, Linux (script) | `curl --proto '=https' --tlsv1.2 -LsSf https://github.com/OseSem/hon/releases/latest/download/hon-installer.sh \| sh` |
| Windows (script) | `powershell -ExecutionPolicy Bypass -c "irm https://github.com/OseSem/hon/releases/latest/download/hon-installer.ps1 \| iex"` |
| Anywhere with Rust | `cargo install hon --locked` |

Building with `cargo install` compiles the bundled SQLite and the TLS library's
cryptography (aws-lc-sys) from C. You need a C compiler, plus
[NASM](https://www.nasm.us/) on x86_64 Windows.

Prebuilt archives for macOS (Apple silicon and Intel), Linux (x86_64 and aarch64, static
musl) and Windows (x86_64) are on the [releases page](https://github.com/OseSem/hon/releases).
Each one has the man page as `hon.1`. The AUR package installs it for `man hon`; elsewhere,
copy it into a `man1` directory on your `MANPATH`.

## Usage

```sh
hon                                   # search, browse and read in the terminal
hon search "solo leveling"            # print every source's matches
hon download "solo leveling" 1-10     # download chapters 1 to 10
hon download "solo leveling" 45-      # chapter 45 up to the latest
hon download "solo leveling" 1-10,15,20.5 --format cbz --source mangadex
hon read "solo leveling"              # pick up where you left off
hon read "solo leveling" 12           # read chapter 12
```

`download` and `read` need a title that matches one series exactly, or a query with only
one result. Otherwise they list the candidates and exit with an error; copy the full title
from that list. Without `--source`, hon uses the source with the newest chapter. If some
chapters you asked for aren't on that source but are on another, `download` asks before
fetching them from there (or prints the command to do it when not run in a terminal).

Downloads go to `<download_dir>/<Title>/<Title> - Ch <N>.pdf` (or `.cbz`, or a folder of
images).

Run `hon --help`, `hon <command> --help` or `man hon` for every option.

### Keys

Ctrl-C quits from any screen.

**Search**

| Key | Action |
| --- | --- |
| type, then Enter | search |
| Esc | stop typing |
| ↑ ↓ | move |
| Tab | pick which source to open |
| Enter | open the chapter list |
| / | new search |
| q | quit |

**Chapter list**

| Key | Action |
| --- | --- |
| ↑ ↓, PgUp PgDn, Home End | move |
| Enter | read in the terminal |
| o | open in your viewer (downloads first if needed) |
| d | download |
| m | mark read or unread |
| s | switch source |
| Esc | back to search |

**Reader**

| Key | Action |
| --- | --- |
| → l Space PgDn | next page |
| ← h PgUp | previous page |
| ↑ ↓ | scroll |
| f | fit to height or width |
| o | open the chapter in your viewer |
| r | retry a page that failed to load |
| q Esc | back to the chapter list |

At the end of a chapter: Enter reads the next one, ← or h goes back to the last page, l or
Esc returns to the chapter list, and q quits. When the next chapter is only on another
source, hon asks first: y reads it there, n goes back to the chapter list.

### Image quality

hon draws pages with the terminal's graphics protocol when it has one, which gives sharp
images in:

- Kitty
- Ghostty
- WezTerm
- iTerm2
- terminals with Sixel support (for example foot, Konsole, Windows Terminal 1.22+, xterm
  started with `-ti vt340`)

Every other terminal gets half-block characters: readable for a quick look, but too coarse
for small text. Press `o` to open the chapter in your viewer instead.

## Configuration

hon runs without a config file. To change the defaults, create `config.toml` in:

| OS | Path |
| --- | --- |
| Linux | `~/.config/hon/config.toml` (or `$XDG_CONFIG_HOME/hon/config.toml`) |
| macOS | `~/Library/Application Support/hon/config.toml` |
| Windows | `%APPDATA%\hon\config.toml` |

It takes three keys, all optional:

| Key | Default | Meaning |
| --- | --- | --- |
| `format` | `"pdf"` | Download format: `"pdf"`, `"cbz"` or `"images"` |
| `viewer` | the OS default app | Command that opens a downloaded chapter; the chapter's path is added as the last argument |
| `download_dir` | `hon` in your Downloads folder | Where chapters are saved; `~/` expands to your home directory |

```toml
format = "cbz"
viewer = "open -a YACReader"
download_dir = "~/manga"
```

`viewer` is split into arguments the way a shell would, so on Windows, quote a path that
has backslashes or spaces. Without quotes the backslashes are dropped. A TOML literal string
(single quotes) holding a double-quoted path keeps it readable:

```toml
viewer = '"C:\Program Files\SumatraPDF\SumatraPDF.exe"'
```

Reading progress is kept in `hon.db` in the data directory (`~/.local/share/hon` on Linux,
the same folder as the config file on macOS and Windows). Setting `HON_HOME` puts the
config file, database and downloads under that one directory instead.

## Future work

- Following series, with notifications for new chapters and a start screen that shows
  unread counts
- Languages other than English
- More sources
- Filling gaps in one source's chapters from other sources automatically

## License

[GPL-3.0-only](LICENSE)
