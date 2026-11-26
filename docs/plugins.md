# Writing source plugins

hon ships with no sources. Each source is a Lua plugin that people install from a source
repo:

```sh
hon repo add <url>          # pin the repo's signing key and read its index
hon source list             # what the added repos offer
hon source install <id>
```

This page is the reference for writing those plugins and publishing a repo of them. It
covers interface version `api = 1`.

Plugins run in [Luau](https://luau.org), the Lua 5.1 dialect, in a sandbox. A plugin can't
touch files or run commands, and it reaches the network only through `http`. hon parses the
plugin's results itself, so a broken plugin gives an error message, not a crash.

## A complete plugin

A source for a made-up site at `https://example.com`. Its search page lists results as
`<div class="result">`, each series page lists chapters as `<li class="chapter">`, and each
chapter page has its image URLs in a script.

`sources/example.lua`:

```lua
local util = require("util")

local BASE = "https://example.com"

return {
  id = "example",
  name = "Example",
  lang = "en",
  api = 1,
  version = 1,
  requests_per_second = 2,
  headers = { ["User-Agent"] = hon.browser_ua, Referer = BASE .. "/" },

  search = function(query)
    local doc = util.get_html(BASE .. "/search", { query = { q = query } })
    local results = {}
    for _, card in ipairs(doc:select(".result")) do
      local link = card:select_one("a.title")
      local slug = link and (link:attr("href") or ""):match("^/manga/([%w-]+)$")
      if slug then
        local alts = {}
        for _, alt in ipairs(card:select(".alt")) do
          table.insert(alts, alt:text())
        end
        table.insert(results, {
          id = slug,
          title = link:text(),
          url = BASE .. "/manga/" .. slug,
          alt_titles = alts,
        })
      end
    end
    return results
  end,

  chapters = function(manga)
    local doc = util.get_html(BASE .. "/manga/" .. manga.id)
    local chapters = {}
    for _, row in ipairs(doc:select("li.chapter")) do
      local link = row:select_one("a")
      local time = row:select_one("time")
      table.insert(chapters, {
        id = link:attr("href"):match("^/chapter/(%d+)$"),
        number = row:attr("data-number"),
        title = row:attr("data-title"),
        published = time and date.parse(time:attr("datetime") or "", "rfc3339"),
      })
    end
    return chapters
  end,

  pages = function(chapter)
    local body = util.get(BASE .. "/chapter/" .. chapter.id)
    local m = regex.match([[var pages = (\[.*?\]);]], body)
    if not m then
      error("no page list in chapter " .. chapter.id, 0)
    end
    return json.decode(m[1])
  end,
}
```

`lib/util.lua`, a module the plugin requires:

```lua
local util = {}

-- The body of a 200 response; anything else is an error the user sees.
function util.get(url, opts)
  local r = http.get(url, opts)
  if r.status ~= 200 then
    error(url .. " returned HTTP " .. r.status, 0)
  end
  return r.body
end

function util.get_html(url, opts)
  return html.parse(util.get(url, opts))
end

return util
```

Try each function without installing anything:

```sh
hon source test sources/example.lua --search "solo leveling"
hon source test sources/example.lua --chapters solo-leveling
hon source test sources/example.lua --pages 101
```

## The manifest

A plugin file returns one table: the manifest fields below plus the functions.

| Field | Type | Required | Meaning |
| --- | --- | --- | --- |
| `id` | string | yes | Matches `^[a-z0-9][a-z0-9_-]{0,63}$`. It names the installed file, what `--source` takes, and the source in reading history. It must equal the `id` the repo's index lists for the file. |
| `name` | string | yes | Shown to the user and put in front of every error message. Control characters become spaces; it can't be empty. |
| `lang` | string | yes | The language of the chapters. Only `"en"` sources install in this version of hon; others show "language not supported yet". |
| `api` | whole number | yes | The plugin interface version: `1`. A plugin with a higher `api` shows "needs a newer hon" and can't be installed. |
| `version` | whole number | no, default `1` | Raise it on every change, so `hon source list` shows the update. |
| `nsfw` | boolean | no, default `false` | `hon source list` hides NSFW sources unless given `--nsfw`. |
| `requests_per_second` | number | no, default `4` | This plugin's rate limit, from `0.1` to `50`. Every `http` request and every default page download waits its turn. |
| `headers` | table of string to string | no | Sent with every request this plugin makes, including hon's own page downloads. `opts.headers` on a single request replaces a header of the same name. |
| `search` | function | yes | See below. |
| `chapters` | function | yes | |
| `pages` | function | yes | |
| `fetch_page` | function | no | |

## The functions

hon calls each function with one argument and reads what it returns.

### `search(query)`

`query` is the string the user searched for. Return a list of series:

```lua
{
  { id = "solo-leveling", title = "Solo Leveling", url = "https://example.com/manga/solo-leveling",
    alt_titles = { "Na Honjaman Level Up" } },
}
```

- `id`: required. Your own key for the series; hon hands it back to `chapters`.
- `title`: required. hon groups results from every source by title and alternative title.
- `url`: required. The series page, handed back to `chapters`.
- `alt_titles`: optional list of strings. Other names the series goes by.

An empty list means no results.

### `chapters(manga)`

`manga` is `{ id = ..., title = ..., url = ... }` from one of your search results. Return
the series' chapters, in any order:

```lua
{
  { id = "101", number = "1" },
  { id = "115", number = 1.5, title = "Side story", published = 1767312000 },
}
```

- `id`: required. Handed back to `pages`.
- `number`: required, a string or a number. See the output rules for the format.
- `title`: optional.
- `published`: optional, the release time as whole Unix seconds. `date.parse` returns that.

`hon source test --chapters <id>` passes only `id`, with `title` and `url` set to `""`, so
build URLs from the id when you can.

### `pages(chapter)`

`chapter` is `{ id = ..., number = ..., title = ... }`: your `id`, the chapter number as a
string such as `"10.5"`, and the title, or `nil` without one. Return the chapter's image URLs
in reading order:

```lua
{ "https://example.com/i/1.jpg", "https://example.com/i/2.jpg" }
```

`hon source test --pages <id>` passes your `id` with `number = "0"` and `title = nil`, so
don't rely on the number or title to find the chapter.

### `fetch_page(page)`, optional

Without it, hon downloads each page URL with a GET that sends your manifest `headers`
under your rate limit, and fails on any status other than 2xx. That GET doesn't run any Lua,
so pages download while other plugin calls run.

Define `fetch_page` when an image needs more than that, such as a per-chapter `Referer`
header or a decoding step. `page` is `{ url = ... }` with one URL from `pages`. Return the
image bytes as a string:

```lua
fetch_page = function(page)
  local r = http.get(page.url, { headers = { Referer = "https://example.com/reader" } })
  if r.status ~= 200 then
    error(page.url .. " returned HTTP " .. r.status, 0)
  end
  return r.body
end,
```

### Errors

Raise an error with `error("message")` when a page doesn't look as expected. hon shows the
first line, cut to 512 bytes, after the source name: `Example: no page list in chapter
101`. Lua puts the file name and line number in front of the message; pass level `0`, as in
`error(msg, 0)`, to leave them out.

## Output rules

hon checks everything a plugin returns, and fixes or refuses what it can't use:

- **Required strings.** `id`, `title` and `url` in search results, `id` in chapters and every
  page URL must be non-empty strings. An empty one is an error.
- **Text.** Every string must be valid UTF-8 and at most 64 KiB. In `title`, `alt_titles`
  and chapter titles, control characters become spaces, runs of spaces become one and the
  ends are trimmed. In ids and URLs, a control character is an error.
- **Chapter numbers.** Digits with an optional fractional part, such as `"12"`, `"10.5"` or
  `"007"`. Spaces around them are ignored, and a Lua number such as `12` or `10.5` works too.
  A chapter whose number is anything else (`"Extra"`, `"12a"`, `"-1"`, `"1e3"`) is dropped
  without an error.
- **`published`.** Whole Unix seconds. Any other value, including a fractional number, is
  ignored.
- **Sorting and duplicates.** hon sorts chapters by number itself and keeps one per number:
  the first one in the order you returned them. Return them in any order, but put the
  version you want kept first when a site lists a number twice.
- **`alt_titles`.** Only the first 32 are read; empty ones are dropped.
- **Counts.** Search results past 2,000 are dropped. More than 20,000 chapters or 2,000 pages
  is an error.
- **Text budget.** hon copies at most 64 MiB of string data out of one call's result. Past
  that, the call fails.
- **`fetch_page`** must return a string of at most 32 MiB.

## The sandbox

- Each plugin runs in its own Luau states, created with Luau's sandbox mode. hon runs up to
  four calls of one plugin at once, each in its own state: the first state loads with the
  plugin, and hon loads the plugin file again into a new one (up to four) only when every
  loaded state is busy. A fifth call waits for a free state.
- A plugin's globals live in its state's own environment, and they persist across calls for
  as long as hon runs, so a global can cache a token or a lookup between calls. They are per
  state, not shared between calls running at once: a cache in a global may be built once
  in each state. The states of one plugin share its cookie jar, headers and rate limit. The standard
  libraries (`string`, `table`, `math` and the rest) and hon's modules are read-only:
  `string.foo = 1` raises "attempt to modify a readonly table".
- Luau has no `io`. Its `os` has only time functions (`os.time`, `os.clock`, `os.date`,
  `os.difftime`).
- `http`, `html.parse`, the `select` and `select_one` methods of documents and elements, and
  the matching functions of `regex` (`match`, `find_all`, `replace`) are async: they only
  work inside the plugin's functions. Called at a file's top level, or at a module's, they
  fail with "attempt to yield across metamethod/C-call boundary". Everything else, including
  `regex.escape`, an element's `text`, `attr` and `html`, and `json`, works anywhere.
- One call runs at a time per state.

### Libs

Code shared between plugins goes in `lib/<name>.lua` in the repo, and a plugin loads it with
`require("<name>")`:

- Module names match `^[a-z0-9_]+$`. `require` finds `lib/<name>.lua` and nothing else: no
  paths, dots or other folders.
- `require` is synchronous. It runs the module once, the first time, and returns the same
  value after that.
- Require libs at the top of the plugin file. `hon source index` records a source's libs by
  loading the file and noting what it requires. A `require` inside a function doesn't run
  then, so the lib isn't listed, isn't installed, and fails at that `require` once the
  source is installed.

### Long strings and patterns

Lua long strings are handy for regex patterns, since backslashes stay as they are:
`[[\d+\.\d+]]`. A long string ends at the first `]]`, though, so a pattern that contains
`]]` needs a level: `[=[ ... ]=]`.

```lua
-- Wrong: the string ends at the "]]" inside the character class.
-- local m = regex.match([[=\[([^\]]*)\]]], text)
local m = regex.match([=[=\[([^\]]*)\]]=], "a=[1,2]")   -- m[1] == "1,2"
```

## Host functions

These globals are in every plugin. `opts` arguments are optional.

### `http`

`http.get(url, opts)` and `http.post(url, opts)` send a request and return the response:

```lua
local r = http.get("https://example.com/search", { query = { q = "solo leveling", page = 2 } })
-- r.status: 200
-- r.url: the URL after redirects
-- r.headers: lower-case names; repeated headers joined with ", "
-- r.body: the body as a string
```

`opts` fields:

| Field | Meaning |
| --- | --- |
| `headers` | Table of extra headers. A name already in the manifest `headers` is replaced. |
| `query` | Table of query parameters, added to the URL. |
| `form` | Table sent as an `application/x-www-form-urlencoded` body. |
| `body` | A raw body string. `form` and `body` can't be used together. |
| `retry_empty` | `false` stops the retry of a 2xx response with an empty body. Default `true`. |

Values in `headers`, `query` and `form` may be strings or numbers.

```lua
local r = http.post("https://example.com/api/search", {
  headers = { ["Content-Type"] = "application/json" },
  body = json.encode({ q = query }),
})
```

How requests behave:

- Every status comes back as a response, 404 and 500 included, except throttling. Check
  `r.status` yourself.
- On 429 or 503, and on a 2xx whose body is empty or only whitespace, hon waits and tries
  again, up to 3 times: after 2 s, 4 s and 8 s. A `Retry-After` header (seconds) or an
  `X-RateLimit-Retry-After` header (a Unix time) replaces the wait, up to 60 s. Out of
  retries, the call fails with `<Name>: the site is rate limiting; try again in a few
  seconds`.
- A connection failure, or no response within 20 seconds, is an error.
- Each plugin has its own cookie jar, which keeps cookies between calls.
- Redirects are followed.

### `html`

`html.parse(text)` parses an HTML page and returns a document. Query it with CSS selectors:

```lua
local doc = html.parse([[<div class="card"><a href="/manga/42"> Blue Period </a><img data-src="/c.jpg"></div>]])
local card = doc:select_one(".card")           -- the first match, or nil
local link = card:select_one("a")
link:text()                    -- "Blue Period": all the text inside, trimmed
link:attr("href")              -- "/manga/42"; nil when the attribute isn't there
card:html()                    -- the inner HTML, as a string
#doc:select("a")               -- 1: select returns every match, in document order
card:select_one("img"):attr("data-src")   -- "/c.jpg"
```

A document has `select(css)` and `select_one(css)`. An element has those, searching only
inside it, plus `text()`, `attr(name)` and `html()`. An invalid selector is an error.

Parsing and selecting run off the plugin's thread and stop at the call's deadline, with
"html.parse took too long" or "html.select took too long". A document whose elements nest
more than 512 deep, counting `<html>` and `<body>`, is refused: deep nesting is what makes
parsing and selectors slow, and real pages stay far below it.

### `regex`

Patterns use the syntax of Rust's [`regex`](https://docs.rs/regex/latest/regex/#syntax)
crate: no look-around and no backreferences, and `.*?` is lazy as usual. Matching runs on the
text's bytes, so text that isn't valid UTF-8 still matches.

`regex.match(pattern, text)` returns the first match, or `nil`. Index `0` is the whole match
and `1`, `2`, ... the groups. A group that didn't take part is `nil`.

```lua
local m = regex.match([[Chapter (\d+(?:\.\d+)?)]], "Read Chapter 10.5 now")
-- m[0] == "Chapter 10.5", m[1] == "10.5"
```

`regex.find_all(pattern, text)` returns a list of every match, each shaped like `match`'s:

```lua
local urls = {}
for _, m in ipairs(regex.find_all([['([^']+\.jpg)']], "x = ['a.jpg', 'b.jpg']")) do
  table.insert(urls, m[1])            -- "a.jpg", then "b.jpg"
end
```

`regex.replace(pattern, text, replacement)` replaces every match. In `replacement`, `$1` or
`${1}` is a group and `$$` is a literal `$`. Use `${1}` when a letter, digit or `_` follows,
since `$1a` means the group named `1a`.

```lua
regex.replace([[(\w+)@(\w+)]], "a@b", "${2}-${1}")    -- "b-a"
regex.replace([[\s+]], "  Solo   Leveling ", " ")     -- " Solo Leveling "
```

`regex.escape(text)` returns a pattern that matches `text` literally:

```lua
regex.escape("1.5 (x)")     -- "1\.5 \(x\)"
```

### `json`

`json.decode(text)` turns JSON into Lua values. JSON `null` becomes `json.null`, so a key
that's there with `null` can be told from a missing one:

```lua
local data = json.decode('{"title": "One", "tags": ["a", "b"], "cover": null}')
-- data.title == "One", data.tags[2] == "b", data.cover == json.null
```

`json.encode(value)` turns a Lua value into JSON, with object keys sorted:

```lua
json.encode({ q = "x", page = 2 })   -- '{"page":2,"q":"x"}'
json.encode({})                      -- '{}'
json.encode(json.decode("[]"))       -- '[]': decoded arrays stay arrays
```

A table with a length becomes an array, any other table an object. Object keys must be
strings or integers. `nil` and `json.null` encode as `null`.

### `base64`

Standard alphabet. `decode` accepts the text with or without padding.

```lua
base64.encode("hon")      -- "aG9u"
base64.decode("aG9uIQ")   -- "hon!"
```

### `date`

`date.parse(text, format)` returns whole Unix seconds, or `nil` when the text doesn't
parse. `format` is `"rfc3339"` or a `strftime`-style pattern. A time with no offset is read
as UTC, and a date with no time as midnight UTC.

```lua
date.parse("2026-01-02T03:04:05+01:00", "rfc3339")   -- 1767319445
date.parse("Jan 02, 2026", "%b %d, %Y")              -- 1767312000
date.parse("2026-01-02 03:04", "%Y-%m-%d %H:%M")     -- 1767323040
date.parse("yesterday", "%Y-%m-%d")                  -- nil
```

### `hon`

```lua
hon.version      -- this hon's version, such as "0.1.0"
hon.browser_ua   -- a desktop browser's User-Agent, for sites that refuse other clients
```

Requests send hon's own User-Agent (`hon/<version>`) unless the manifest or the request sets
`User-Agent`.

### `log`

`log.debug(message)` writes a line to stderr under `hon source test`. hon drops it
everywhere else.

```lua
log.debug("found " .. #results .. " results")   -- example: found 2 results
```

## Limits

| What | Limit |
| --- | --- |
| Memory per plugin state | 64 MiB |
| Time for loading the file, and for each call | 60 s. Busy loops are stopped at the deadline too. |
| One HTTP request | 20 s |
| HTTP response body | 32 MiB |
| Strings returned to hon | 64 KiB each; `fetch_page` bytes up to 32 MiB |
| String data copied out of one call | 64 MiB |
| Search results | 2,000; the rest are dropped |
| Chapters per series | 20,000 |
| Pages per chapter | 2,000 |
| `alt_titles` per result | 32 read |
| `regex` pattern | 64 KiB, at most 64 capture groups counting the whole match |
| Compiled `regex` | 128 KiB. A short pattern can still go over it, such as a large repetition like `\w{1000}`, and is refused with a size-limit error |
| `regex` text | 2 MiB |
| `regex.replace` replacement | 64 KiB; the result at most 32 MiB |
| `regex.find_all` | about a million captures in all |
| `html.parse` input | 16 MiB, elements nested at most 512 deep |
| Documents held at once | 2,000,000 nodes and 64 MiB of input in all; documents you no longer use count until Lua's garbage collector frees them |
| `json.decode` input | 16 MiB, nested at most 128 deep |
| `json.encode` output | 32 MiB, nested at most 128 deep |
| Repo files | `index.json` and each plugin or lib file 4 MiB; `index.json.minisig` and `key.pub` 16 KiB |

A plugin that runs past a limit gets an error for that call. The next call gets its own
60 s.

Searching is shorter. When the user searches, hon waits 20 s for each source's `search`,
then 20 s for the `chapters` of the results it shows, not 60 s for each. A source still
working when that time is up is listed as failed for that search.

## Source repos

A repo is a folder of files:

```text
index.json           what the repo offers, with every file's SHA-256
index.json.minisig   the minisign signature of index.json
key.pub              the minisign public key
sources/example.lua  one file per source
lib/util.lua         modules the sources require
```

### `index.json`

`hon source index <repo-dir>` writes it. For the example above:

```json
{
  "name": "repo",
  "api": 1,
  "sources": [
    {
      "id": "example",
      "name": "Example",
      "version": 1,
      "lang": "en",
      "nsfw": false,
      "api": 1,
      "file": "sources/example.lua",
      "sha256": "6da9d2217342f73b5af1e87168ad52c22527bf00ad38af4db85718e6c6a02425",
      "libs": [
        "lib/util.lua"
      ]
    }
  ],
  "libs": [
    {
      "file": "lib/util.lua",
      "sha256": "e3ce93e1a1ef752c3d2bccffcc55147379e41b271fcc79b9fc86dc6d4d55d332"
    }
  ]
}
```

| Field | Meaning |
| --- | --- |
| `name` | The repo's name in `hon repo list` and `hon source list`. `hon source index` keeps the name an existing `index.json` has, and otherwise uses the folder's name. |
| `api` | The interface version the index was written for. |
| `sources[].id`, `name`, `version`, `lang`, `nsfw`, `api` | Copied from the plugin's manifest. hon decides what to list and install from these, before it runs the file. |
| `sources[].file` | The plugin's path, `sources/<name>.lua`. |
| `sources[].sha256` | The file's SHA-256, in lowercase hex. |
| `sources[].libs` | The `lib/<name>.lua` files the source requires. Each must be in `libs`. |
| `libs[].file`, `libs[].sha256` | Every lib in the repo and its SHA-256. |

### Signing

The signature on `index.json` covers every file, since the index holds each file's hash.
Sign with [minisign](https://jedisct1.github.io/minisign/):

```sh
minisign -G -p key.pub -s ~/.minisign/my-repo.key   # once: make a key pair
hon source index .                                  # after every change
minisign -Sm index.json -s ~/.minisign/my-repo.key  # writes index.json.minisig
```

Keep the secret key out of the repo. Publish `key.pub` with the repo, or give users the key
line to pass as `hon repo add <location> --key <base64>`.

hon pins the key when the repo is added, from `key.pub` or `--key`; given both, they must
match. After that, hon checks every index it fetches against the pinned key and refuses one
that doesn't verify. It checks each file's SHA-256 against the signed index at install and
again every time it loads the plugin, so a changed file stops working until it's
reinstalled.

### Publishing a repo

`hon repo add` takes three forms:

- `hon repo add <https-url>`: the URL of the repo folder or of its `index.json`, on any
  static file host. hon fetches `index.json`, `index.json.minisig` and `key.pub` next to
  each other. Plain `http://` is refused, except for `localhost` and `127.0.0.1`. The URL
  can't hold a user name or password.
- `hon repo add <folder>`: a repo folder on this machine. Handy while you write a repo.
- `hon repo add --git <url>`: hon clones the repo with the machine's `git`, which works for
  private repos that `git` can reach with your own credentials. It accepts `https://`,
  `ssh://`, `file://`, `user@host:path` and a local path, and refuses plain `http://` and
  `git://`. Every refresh is a fresh shallow clone.

`hon source update` refreshes every added repo and reinstalls each installed source whose
version or file changed. Raise `version` with every change anyway: `hon source list` shows
an update as `v1 → v2`.

## Testing and indexing

### `hon source test`

```sh
hon source test <file.lua> --search <query>
hon source test <file.lua> --chapters <manga-id>
hon source test <file.lua> --pages <chapter-id>
```

It loads a plugin file without installing it, runs one function, and prints the result as
hon reads it: after the output rules, so chapters come out sorted, de-duplicated, and
without the ones whose number didn't parse. Search prints `id  title  url`, chapters
`number  id  title`, and pages one URL per line. `log.debug` lines go to stderr.

`require` looks in `../lib` next to the file, which is the repo's `lib/` for a file in
`sources/`. Point it elsewhere with `--lib-dir <dir>`. The files aren't hash-checked here.

### `hon source index`

```sh
hon source index <repo-dir>
```

It loads every `sources/*.lua`, reads each manifest, and writes `<repo-dir>/index.json` with
the hashes of every source and of every `lib/*.lua`. A file that doesn't load, or two files
with one `id`, stop it with an error. Sign `index.json` afterwards; it prints the command.

It finds a source's libs by noting what the file requires while it loads, which is why libs
must be required at the top of the file.
