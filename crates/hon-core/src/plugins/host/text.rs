//! The text helpers: `regex`, `json`, `base64`, `date`, plus `hon` and `log`.
//!
//! Host functions run outside the VM's interrupt and allocate outside the Lua memory limit, so
//! each one caps its input or output: a plugin can build ~60 MiB strings and tables that share
//! subtables, and must not turn them into unbounded work here.

use std::collections::HashMap;
use std::ffi::c_void;
use std::io;
use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, STANDARD_PAD_INDIFFERENT};
use jiff::Timestamp;
use jiff::tz::TimeZone;
use mlua::{Lua, LuaSerdeExt, LuaString, Table, Value};
use regex::bytes::{Captures, Regex, RegexBuilder};
use tokio::sync::Semaphore;

use crate::plugins::host::LogSink;
use crate::plugins::runtime;

/// The plugin id `log.debug` reports: the chunk name until the manifest has been read.
pub(crate) struct PluginId(pub String);

const JSON_ENCODE_CAP: usize = 32 << 20;
const JSON_DECODE_CAP: usize = 16 << 20;
const PATTERN_CAP: usize = 64 << 10;
/// The compiled program's size limit. When the lazy DFA gives up, matching falls back to
/// O(pattern x capture slots x text): at this limit and `REGEX_TEXT_CAP` one search can run
/// for tens of seconds in release (`a`, 63 `([ab])` groups, then `[ab]{1500}[^ab]`, on 2 MiB:
/// ~42 s). Jobs run off the Lua thread and stop between searches once the call stops waiting,
/// so that one search is what a timed-out job can still spend, not how long the plugin waits.
const REGEX_SIZE_LIMIT: usize = 128 << 10;
const REGEX_DFA_SIZE_LIMIT: usize = 1 << 20;
const REGEX_TEXT_CAP: usize = 2 << 20;
/// Counting the whole match: the fallback's cost grows with capture slots.
const CAPTURE_GROUP_CAP: usize = 64;
const TEMPLATE_CAP: usize = 64 << 10;
const REPLACE_CAP: usize = 32 << 20;
/// What `find_all` collects off the Lua thread: the groups that took part, plus one per match.
/// The Lua tables for this many would already be near the 64 MiB memory limit.
const FIND_ALL_SLOT_CAP: usize = 1 << 20;
const REGEX_DEADLINE_MARGIN: std::time::Duration = std::time::Duration::from_millis(50);
const REGEX_CACHE_CAP: usize = 64;

/// `"<what> is over 32 MiB"`.
pub(crate) fn over_cap(what: &str, cap: usize) -> mlua::Error {
    let size = if cap >= 1 << 20 {
        format!("{} MiB", cap >> 20)
    } else {
        format!("{} KiB", cap >> 10)
    };
    mlua::Error::runtime(format!("{what} is over {size}"))
}

/// One plugin's compiled patterns. When full it is emptied, then refilled as patterns are used.
#[derive(Default)]
struct RegexCache(Mutex<HashMap<String, Regex>>);

impl RegexCache {
    fn get(&self, pattern: &str) -> mlua::Result<Regex> {
        if pattern.len() > PATTERN_CAP {
            return Err(over_cap("regex: the pattern", PATTERN_CAP));
        }
        let mut cache = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(re) = cache.get(pattern) {
            return Ok(re.clone());
        }
        let re = RegexBuilder::new(pattern)
            .size_limit(REGEX_SIZE_LIMIT)
            .dfa_size_limit(REGEX_DFA_SIZE_LIMIT)
            .build()
            .map_err(|e| mlua::Error::runtime(format!("invalid regex: {e}")))?;
        if re.captures_len() > CAPTURE_GROUP_CAP {
            return Err(mlua::Error::runtime(format!(
                "regex: the pattern has over {CAPTURE_GROUP_CAP} capture groups, counting the whole match"
            )));
        }
        if cache.len() >= REGEX_CACHE_CAP {
            cache.clear();
        }
        cache.insert(pattern.to_string(), re.clone());
        Ok(re)
    }
}

/// Registers the `regex`, `json`, `base64`, `date`, `hon` and `log` globals.
pub fn install(lua: &Lua, log: LogSink) -> mlua::Result<()> {
    let globals = lua.globals();
    globals.set("regex", regex_module(lua)?)?;
    globals.set("json", json_module(lua)?)?;
    globals.set("base64", base64_module(lua)?)?;

    let date = lua.create_table()?;
    date.raw_set(
        "parse",
        lua.create_function(|_, (s, format): (Value, Value)| {
            let (Value::String(s), Value::String(format)) = (s, format) else {
                return Ok(None);
            };
            let (Ok(s), Ok(format)) = (s.to_str(), format.to_str()) else {
                return Ok(None);
            };
            Ok(parse_date(&s, &format))
        })?,
    )?;
    globals.set("date", date)?;

    let hon = lua.create_table()?;
    hon.raw_set("browser_ua", crate::sources::BROWSER_UA)?;
    hon.raw_set("version", env!("CARGO_PKG_VERSION"))?;
    globals.set("hon", hon)?;

    let log_module = lua.create_table()?;
    log_module.raw_set(
        "debug",
        lua.create_function(move |lua, msg: LuaString| {
            let id = lua
                .app_data_ref::<PluginId>()
                .map(|id| id.0.clone())
                .unwrap_or_default();
            log(&id, &msg.to_string_lossy());
            Ok(())
        })?,
    )?;
    globals.set("log", log_module)
}

/// `regex.match`, `regex.find_all`, `regex.replace` and `regex.escape`. Patterns run on the
/// text's bytes, so text that isn't UTF-8 still matches.
///
/// Compiling and matching can't be bounded by the caps alone (when the lazy DFA gives up, the
/// fallback costs about pattern states x capture slots x text), so both run off the Lua
/// thread, see [`RegexJobs`], and the plugin waits only until its call's deadline.
fn regex_module(lua: &Lua) -> mlua::Result<Table> {
    let jobs = RegexJobs::default();
    let module = lua.create_table()?;

    let j = jobs.clone();
    module.raw_set(
        "match",
        lua.create_async_function(move |lua, (pattern, text): (String, LuaString)| {
            let j = j.clone();
            async move {
                let text = regex_text(&text)?;
                let t = text.clone();
                let found = j
                    .run(&lua, move |re, _| {
                        Ok(re
                            .get(&pattern)?
                            .captures(&t)
                            .map(|caps| spans(&caps).collect::<Vec<_>>()))
                    })
                    .await?;
                found.map(|spans| captures(&lua, &text, &spans)).transpose()
            }
        })?,
    )?;

    let j = jobs.clone();
    module.raw_set(
        "find_all",
        lua.create_async_function(move |lua, (pattern, text): (String, LuaString)| {
            let j = j.clone();
            async move {
                let text = regex_text(&text)?;
                let t = text.clone();
                let all = j
                    .run(&lua, move |re, stop| {
                        let re = re.get(&pattern)?;
                        let mut all = Vec::new();
                        let mut matches = 0;
                        for caps in re.captures_iter(&t) {
                            stopped(stop)?;
                            all.extend(spans(&caps));
                            matches += 1;
                            if all.len() + matches > FIND_ALL_SLOT_CAP {
                                return Err(mlua::Error::runtime(format!(
                                    "regex.find_all: over {FIND_ALL_SLOT_CAP} captures"
                                )));
                            }
                        }
                        Ok(all)
                    })
                    .await?;
                // Group 0 always takes part, so it starts each match.
                let tables = all
                    .chunk_by(|_, next| next.0 != 0)
                    .map(|spans| captures(&lua, &text, spans))
                    .collect::<mlua::Result<Vec<_>>>()?;
                lua.create_sequence_from(tables)
            }
        })?,
    )?;

    module.raw_set(
        "replace",
        lua.create_async_function(
            move |lua, (pattern, text, repl): (String, LuaString, LuaString)| {
                let j = jobs.clone();
                async move {
                    let text = regex_text(&text)?;
                    let repl = repl.as_bytes();
                    if repl.len() > TEMPLATE_CAP {
                        return Err(over_cap("regex.replace: the replacement", TEMPLATE_CAP));
                    }
                    let repl = repl.to_vec();
                    let out = j
                        .run(&lua, move |re, stop| {
                            replace_all(&re.get(&pattern)?, &text, &repl, REPLACE_CAP, stop)
                        })
                        .await?;
                    lua.create_string(out)
                }
            },
        )?,
    )?;

    module.raw_set(
        "escape",
        lua.create_function(|_, s: String| Ok(regex::escape(&s)))?,
    )?;
    Ok(module)
}

/// One plugin's regex work on tokio's blocking pool, one job at a time. A job the plugin
/// stopped waiting for is told to stop and does so at its next match or template piece, but a
/// single search can't be stopped: the job keeps the slot until it returns, so a plugin can't
/// pile up runaway threads, and its later regex calls wait for the slot within their deadline.
#[derive(Clone)]
struct RegexJobs {
    cache: Arc<RegexCache>,
    slot: Arc<Semaphore>,
}

impl Default for RegexJobs {
    fn default() -> Self {
        RegexJobs {
            cache: Arc::default(),
            slot: Arc::new(Semaphore::new(1)),
        }
    }
}

impl RegexJobs {
    /// Runs `job` with the plugin's pattern cache, until the current call's deadline. The flag
    /// the job gets is set once this call stops waiting, timed out or dropped; see [`stopped`].
    async fn run<T: Send + 'static>(
        &self,
        lua: &Lua,
        job: impl FnOnce(&RegexCache, &AtomicBool) -> mlua::Result<T> + Send + 'static,
    ) -> mlua::Result<T> {
        let Some(deadline) = runtime::deadline(lua) else {
            return Err(mlua::Error::runtime("plugin exceeded its time limit"));
        };
        // Give up a little early: once the deadline passes, the Lua that would raise this
        // error is itself stopped, and the plugin sees only the generic time-limit error.
        let deadline = tokio::time::Instant::from_std(deadline) - REGEX_DEADLINE_MARGIN;
        let slot = tokio::time::timeout_at(deadline, self.slot.clone().acquire_owned())
            .await
            .map_err(|_| too_long())?
            .map_err(mlua::Error::external)?;
        let cache = self.cache.clone();
        let stop = StopOnDrop(Arc::default());
        let job_stop = stop.0.clone();
        let task = tokio::task::spawn_blocking(move || {
            let _slot = slot;
            job(&cache, &job_stop)
        });
        match tokio::time::timeout_at(deadline, task).await {
            Ok(Ok(result)) => result,
            Ok(Err(e)) => Err(mlua::Error::runtime(format!("regex failed: {e}"))),
            Err(_) => Err(too_long()),
        }
    }
}

fn too_long() -> mlua::Error {
    mlua::Error::runtime("regex took too long")
}

/// Sets a job's stop flag when the call that started it stops waiting.
struct StopOnDrop(Arc<AtomicBool>);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// Fails once the call waiting for this job has stopped waiting.
fn stopped(stop: &AtomicBool) -> mlua::Result<()> {
    if stop.load(Ordering::Relaxed) {
        return Err(too_long());
    }
    Ok(())
}

/// A copy of the text to search, which the job owns.
fn regex_text(text: &LuaString) -> mlua::Result<Arc<[u8]>> {
    let text = text.as_bytes();
    if text.len() > REGEX_TEXT_CAP {
        return Err(over_cap("regex: the text", REGEX_TEXT_CAP));
    }
    Ok(Arc::from(&*text))
}

/// A group that took part in a match: its index, start and end. Text is at most
/// `REGEX_TEXT_CAP`, so offsets fit in `u32`.
type Span = (u32, u32, u32);

fn spans(caps: &Captures) -> impl Iterator<Item = Span> {
    caps.iter().enumerate().filter_map(|(i, m)| {
        let m = m?;
        Some((i as u32, m.start() as u32, m.end() as u32))
    })
}

/// `[0]` is the whole match, `[n]` group n; a group that didn't take part is absent.
fn captures(lua: &Lua, text: &[u8], spans: &[Span]) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    for &(i, start, end) in spans {
        table.raw_set(i, lua.create_string(&text[start as usize..end as usize])?)?;
    }
    Ok(table)
}

/// `re.replace_all(text, repl)`, failing once the result passes `cap` or `stop` is set. Built
/// one piece at a time: a single expansion of `repl` can repeat a long match many times, and
/// one match can expand thousands of pieces.
fn replace_all(
    re: &Regex,
    text: &[u8],
    repl: &[u8],
    cap: usize,
    stop: &AtomicBool,
) -> mlua::Result<Vec<u8>> {
    let over = || over_cap("regex.replace: the result", cap);
    let pieces = template_pieces(repl);
    let mut out = Vec::new();
    let mut last = 0;
    for caps in re.captures_iter(text) {
        stopped(stop)?;
        let whole = caps.get(0).expect("group 0 always takes part");
        out.extend_from_slice(&text[last..whole.start()]);
        for piece in &pieces {
            stopped(stop)?;
            caps.expand(piece, &mut out);
            if out.len() > cap {
                return Err(over());
            }
        }
        last = whole.end();
    }
    out.extend_from_slice(&text[last..]);
    if out.len() > cap {
        return Err(over());
    }
    Ok(out)
}

/// `repl` cut before each `$` that starts a capture reference, so each piece expands to at
/// most one capture and expanding the pieces in turn equals expanding `repl`. `$$` and
/// `${...}` stay whole. One pass: where the next `}` and the next byte that isn't valid UTF-8
/// are is worked out up front, so finding where a `${` ends costs nothing.
fn template_pieces(repl: &[u8]) -> Vec<&[u8]> {
    let n = repl.len();
    let mut invalid = vec![false; n];
    let mut at = 0;
    for chunk in repl.utf8_chunks() {
        at += chunk.valid().len();
        for _ in chunk.invalid() {
            invalid[at] = true;
            at += 1;
        }
    }
    // For each position: the first `}`, and the first invalid byte, at or after it.
    let mut next_close = vec![n; n + 1];
    let mut next_invalid = vec![n; n + 1];
    for i in (0..n).rev() {
        next_close[i] = if repl[i] == b'}' {
            i
        } else {
            next_close[i + 1]
        };
        next_invalid[i] = if invalid[i] { i } else { next_invalid[i + 1] };
    }

    let mut pieces = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < n {
        if repl[i] != b'$' {
            i += 1;
            continue;
        }
        if repl.get(i + 1) == Some(&b'$') {
            i += 2;
            continue;
        }
        if i > start {
            pieces.push(&repl[start..i]);
            start = i;
        }
        i = match repl.get(i + 1) {
            // A reference only when the brace closes and the name is UTF-8, as in regex's
            // own parser; otherwise the `$` is literal and parsing goes on after it. The name
            // starts after an ASCII `{` and ends at an ASCII `}`, so it is UTF-8 exactly when
            // it holds no byte that the whole template's decoding marks invalid.
            Some(b'{') => {
                let close = next_close[i + 2];
                if close < n && next_invalid[i + 2] >= close {
                    close + 1
                } else {
                    i + 1
                }
            }
            _ => i + 1,
        };
    }
    pieces.push(&repl[start..]);
    pieces
}

/// Collects output and fails once it would pass `cap`.
struct CappedWriter {
    buf: Vec<u8>,
    cap: usize,
}

impl io::Write for CappedWriter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if self.buf.len() + data.len() > self.cap {
            return Err(io::Error::other("over the cap"));
        }
        self.buf.extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

const JSON_DEPTH_LIMIT: usize = 128;
/// Tables whose encoding is at least this long are remembered and copied when met again.
const JSON_MEMO_MIN: usize = 64;

/// Lua to JSON with sorted object keys, reading tables raw. A table met twice is copied from
/// its first encoding rather than walked again, so tables that share subtables (each level
/// holding the one below twice) cost time in proportion to the output, which is capped.
struct JsonEncoder {
    array_mt: Table,
    out: CappedWriter,
    done: HashMap<*const c_void, Range<usize>>,
    /// The tables being encoded, outermost first.
    open: Vec<*const c_void>,
}

impl JsonEncoder {
    fn encode(lua: &Lua, v: &Value) -> mlua::Result<Vec<u8>> {
        let mut enc = JsonEncoder {
            array_mt: lua.array_metatable(),
            out: CappedWriter {
                buf: Vec::new(),
                cap: JSON_ENCODE_CAP,
            },
            done: HashMap::new(),
            open: Vec::new(),
        };
        enc.value(v)?;
        Ok(enc.out.buf)
    }

    fn write(&mut self, bytes: &[u8]) -> mlua::Result<()> {
        io::Write::write_all(&mut self.out, bytes).map_err(|_| Self::over())
    }

    fn over() -> mlua::Error {
        over_cap("json.encode: the result", JSON_ENCODE_CAP)
    }

    fn serde<T: serde::Serialize + ?Sized>(&mut self, v: &T) -> mlua::Result<()> {
        serde_json::to_writer(&mut self.out, v).map_err(|e| {
            if e.is_io() {
                Self::over()
            } else {
                mlua::Error::runtime(format!("json.encode: {e}"))
            }
        })
    }

    fn string(&mut self, s: &LuaString) -> mlua::Result<()> {
        let s = s
            .to_str()
            .map_err(|_| mlua::Error::runtime("json.encode: a string is not valid UTF-8"))?;
        self.serde(&*s)
    }

    fn value(&mut self, v: &Value) -> mlua::Result<()> {
        match v {
            Value::Nil => self.write(b"null"),
            Value::LightUserData(ud) if ud.0.is_null() => self.write(b"null"),
            Value::Boolean(true) => self.write(b"true"),
            Value::Boolean(false) => self.write(b"false"),
            Value::Integer(n) => self.serde(n),
            // Non-finite numbers become `null`, as serde_json writes them.
            Value::Number(n) => self.serde(n),
            Value::String(s) => self.string(s),
            Value::Table(t) => self.table(t),
            other => Err(mlua::Error::runtime(format!(
                "json.encode: can't encode a {}",
                other.type_name()
            ))),
        }
    }

    fn table(&mut self, t: &Table) -> mlua::Result<()> {
        let ptr = t.to_pointer();
        if let Some(span) = self.done.get(&ptr).cloned() {
            if self.out.buf.len() + span.len() > self.out.cap {
                return Err(Self::over());
            }
            self.out.buf.extend_from_within(span);
            return Ok(());
        }
        if self.open.contains(&ptr) {
            return Err(mlua::Error::runtime("json.encode: a table contains itself"));
        }
        if self.open.len() >= JSON_DEPTH_LIMIT {
            return Err(mlua::Error::runtime(
                "json.encode: tables nested too deeply",
            ));
        }
        self.open.push(ptr);
        let start = self.out.buf.len();
        let result = self.table_body(t);
        self.open.pop();
        result?;
        let span = start..self.out.buf.len();
        if span.len() >= JSON_MEMO_MIN {
            self.done.insert(ptr, span);
        }
        Ok(())
    }

    /// Like mlua's serializer: an array when the table has a length or came from a JSON
    /// array, otherwise an object.
    fn table_body(&mut self, t: &Table) -> mlua::Result<()> {
        let len = t.raw_len();
        if len > 0 || t.metatable().as_ref() == Some(&self.array_mt) {
            self.write(b"[")?;
            for i in 1..=len {
                if i > 1 {
                    self.write(b",")?;
                }
                self.value(&t.raw_get::<Value>(i)?)?;
            }
            return self.write(b"]");
        }
        let mut entries = Vec::new();
        t.for_each(|k: Value, v: Value| {
            let key = match k {
                Value::String(s) => s.to_str()?.to_string(),
                Value::Integer(n) => n.to_string(),
                _ => {
                    return Err(mlua::Error::runtime(
                        "json.encode: object keys must be strings or integers",
                    ));
                }
            };
            entries.push((key, v));
            Ok(())
        })?;
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        self.write(b"{")?;
        for (i, (key, v)) in entries.iter().enumerate() {
            if i > 0 {
                self.write(b",")?;
            }
            self.serde(key)?;
            self.write(b":")?;
            self.value(v)?;
        }
        self.write(b"}")
    }
}

/// `json.decode`, `json.encode` and `json.null`, the value JSON `null` decodes to.
fn json_module(lua: &Lua) -> mlua::Result<Table> {
    let module = lua.create_table()?;
    module.raw_set(
        "decode",
        lua.create_function(|lua, s: LuaString| {
            let bytes = s.as_bytes();
            if bytes.len() > JSON_DECODE_CAP {
                return Err(over_cap("json.decode: the input", JSON_DECODE_CAP));
            }
            // Straight into Lua values, so what decoding allocates counts against the limit.
            let failed =
                |e: &dyn std::fmt::Display| mlua::Error::runtime(format!("json.decode: {e}"));
            let mut de = serde_json::Deserializer::from_slice(&bytes);
            let value = serde_transcode::transcode(&mut de, mlua::serde::Serializer::new(lua))
                .map_err(|e| failed(&e))?;
            de.end().map_err(|e| failed(&e))?;
            Ok(value)
        })?,
    )?;
    module.raw_set(
        "encode",
        lua.create_function(|lua, v: Value| lua.create_string(JsonEncoder::encode(lua, &v)?))?,
    )?;
    module.raw_set("null", Value::NULL)?;
    Ok(module)
}

/// `base64.decode` and `base64.encode`, standard alphabet; decoding takes padding or none.
fn base64_module(lua: &Lua) -> mlua::Result<Table> {
    let module = lua.create_table()?;
    module.raw_set(
        "decode",
        lua.create_function(|lua, s: LuaString| {
            let bytes = STANDARD_PAD_INDIFFERENT
                .decode(&*s.as_bytes())
                .map_err(|e| mlua::Error::runtime(format!("base64.decode: {e}")))?;
            lua.create_string(bytes)
        })?,
    )?;
    module.raw_set(
        "encode",
        lua.create_function(|_, s: LuaString| Ok(STANDARD.encode(&*s.as_bytes())))?,
    )?;
    Ok(module)
}

/// Unix seconds for `s` read with `format`: `"rfc3339"` or a strptime pattern. A time
/// without an offset is UTC. `None` when it doesn't parse.
fn parse_date(s: &str, format: &str) -> Option<i64> {
    let ts = if format == "rfc3339" {
        s.parse::<Timestamp>().ok()?
    } else {
        let tm = jiff::fmt::strtime::parse(format, s).ok()?;
        match tm.to_timestamp() {
            Ok(ts) => ts,
            Err(_) => tm
                .to_datetime()
                .ok()?
                .to_zoned(TimeZone::UTC)
                .ok()?
                .timestamp(),
        }
    };
    Some(ts.as_second())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    use mlua::Value;

    use super::*;
    use crate::plugins::host::testing::{run, run_err};
    use crate::plugins::runtime::{HostConfig, LibSource, Runtime};

    #[tokio::test]
    async fn regex_match_groups_and_find_all() {
        let (whole, major, minor): (String, String, String) =
            run(r#"local m = regex.match("(\\d+)\\.(\\d+)", "v 10.5")
               return m[0], m[1], m[2]"#)
            .await;
        assert_eq!(
            (whole.as_str(), major.as_str(), minor.as_str()),
            ("10.5", "10", "5")
        );

        let found: Vec<String> = run(r#"local out = {}
               for _, m in regex.find_all("'([^']*)'", "'a','b'") do table.insert(out, m[1]) end
               return out"#)
        .await;
        assert_eq!(found, ["a", "b"]);

        let (none, skipped, taken): (bool, Value, String) =
            run(r#"local m = regex.match("(a)|(b)", "b")
               return regex.match("x", "abc") == nil and #regex.find_all("x", "abc") == 0,
                   m[1], m[2]"#)
            .await;
        assert!(none);
        assert_eq!(skipped, Value::Nil);
        assert_eq!(taken, "b");
    }

    /// The spec's snippet, except the second pattern uses `[=[ ]=]`: in `[[=\[([^\[]*)\]]]`
    /// the long string ends at the first `]]`, which is a Lua syntax error.
    #[tokio::test]
    async fn regex_mangakatana_two_step() {
        let urls: Vec<String> = run(
            r#"local body = [==[var x=1; data-src', thzq); var thzq=['https://a/1.jpg','https://a/2.jpg',];]==]
               local name = regex.match([[data-src['"],\s*(\w+)]], body)[1]
               local arr = regex.match("var " .. regex.escape(name) .. [=[=\[([^\[]*)\]]=], body)[1]
               local urls = {}
               for _, m in regex.find_all([['([^']*)']], arr) do table.insert(urls, m[1]) end
               return urls"#,
        )
        .await;
        assert_eq!(urls, ["https://a/1.jpg", "https://a/2.jpg"]);
    }

    #[tokio::test]
    async fn regex_replace_and_escape() {
        let (replaced, escaped): (String, String) =
            run(r#"return regex.replace("(\\w+)@", "a@ b@", "<$1>"), regex.escape("a.b*")"#).await;
        assert_eq!(replaced, "<a> <b>");
        assert_eq!(escaped, r"a\.b\*");
    }

    #[tokio::test]
    async fn regex_invalid_pattern_errors() {
        let err = run_err(r#"return regex.match("(", "x")"#).await;
        assert!(err.contains("regex"), "{err}");
    }

    #[test]
    fn regex_size_limit_is_tighter_than_the_default() {
        let big = r"\w{100}\w{100}";
        assert!(Regex::new(big).is_ok());
        let err = RegexCache::default().get(big).unwrap_err().to_string();
        assert!(err.contains("size limit"), "{err}");
        // The measured worst case (lazy DFA gives up, O(pattern x text) fallback): the largest
        // N that compiles is what took ~11 s on 2 MiB in release.
        let cache = RegexCache::default();
        assert!(cache.get("a[ab]{1804}[^ab]").is_ok());
        let err = cache.get("a[ab]{1900}[^ab]").unwrap_err().to_string();
        assert!(err.contains("size limit"), "{err}");
    }

    #[test]
    fn regex_cache_is_capped() {
        let cache = RegexCache::default();
        for i in 0..REGEX_CACHE_CAP + 5 {
            cache.get(&format!("a{i}")).unwrap();
        }
        let len = cache.0.lock().unwrap().len();
        assert!((1..=REGEX_CACHE_CAP).contains(&len), "{len}");
        assert_eq!(cache.get("x+").unwrap().as_str(), "x+");
    }

    #[tokio::test]
    async fn json_roundtrip_and_null() {
        let (is_null, b, list, nested): (bool, i64, String, String) =
            run(r#"return json.decode('{"a":[1,null]}').a[2] == json.null,
                   json.decode(json.encode({b=1})).b,
                   json.encode(json.decode('[1,"x",null,[]]')),
                   json.encode({k = {true, false}})"#)
            .await;
        assert!(is_null);
        assert_eq!(b, 1);
        assert_eq!(list, r#"[1,"x",null,[]]"#);
        assert_eq!(nested, r#"{"k":[true,false]}"#);
        let err = run_err(r#"return json.decode("{nope")"#).await;
        assert!(err.contains("json.decode"), "{err}");
    }

    #[tokio::test]
    async fn base64_roundtrip() {
        let (encoded, back, padded, unpadded): (String, bool, String, String) =
            run(r#"local raw = "hi\0\255"
               return base64.encode("hi"), base64.decode(base64.encode(raw)) == raw,
                   base64.decode("aGk="), base64.decode("aGk")"#)
            .await;
        assert_eq!(encoded, "aGk=");
        assert!(back);
        assert_eq!((padded.as_str(), unpadded.as_str()), ("hi", "hi"));
        let err = run_err(r#"return base64.decode("!!")"#).await;
        assert!(err.contains("base64.decode"), "{err}");
    }

    #[tokio::test]
    async fn date_parse_formats() {
        let (rfc, strp, offset, garbage, wrong): (i64, i64, i64, Value, Value) =
            run(r#"return date.parse("2024-01-15T10:00:00Z", "rfc3339"),
                   date.parse("Jan-15-2024", "%b-%d-%Y"),
                   date.parse("2024-01-15 12:00 +0200", "%Y-%m-%d %H:%M %z"),
                   date.parse("garbage", "rfc3339"),
                   date.parse("Jan-15", "%b-%d")"#)
            .await;
        assert_eq!(rfc, 1705312800);
        assert_eq!(strp, 1705276800);
        assert_eq!(offset, 1705312800);
        assert_eq!(garbage, Value::Nil);
        assert_eq!(wrong, Value::Nil);
    }

    #[tokio::test]
    async fn hon_globals_present() {
        let (ua, version, readonly): (String, String, bool) =
            run(r#"return hon.browser_ua, hon.version,
                   not pcall(function() hon.version = "x" end)"#)
            .await;
        assert_eq!(ua, crate::sources::BROWSER_UA);
        assert_eq!(version, env!("CARGO_PKG_VERSION"));
        assert!(readonly);
    }

    /// Runs `body`, which must raise within a second, and returns the error.
    async fn quick_err(body: &str) -> String {
        let start = std::time::Instant::now();
        let err = run_err(body).await;
        let took = start.elapsed();
        assert!(took < std::time::Duration::from_secs(1), "{took:?}: {err}");
        err
    }

    #[tokio::test]
    async fn json_encode_of_shared_subtables_hits_the_cap() {
        let err =
            quick_err("local t = {1} for i = 1, 40 do t = {t, t} end return json.encode(t)").await;
        assert!(
            err.contains("json.encode: the result is over 32 MiB"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn json_encode_copies_shared_tables_and_refuses_cycles() {
        let (shared, special): (String, String) =
            run(r#"local x = {s = string.rep("a", 70), n = {1, 2}}
               return json.encode({a = x, b = x, c = {x}}),
                   json.encode({0/0, math.huge, 1.5, json.null})"#)
            .await;
        let x = format!(r#"{{"n":[1,2],"s":"{}"}}"#, "a".repeat(70));
        assert_eq!(shared, format!(r#"{{"a":{x},"b":{x},"c":[{x}]}}"#));
        assert_eq!(special, "[null,null,1.5,null]");

        let err = run_err("local t = {} t.self = t return json.encode(t)").await;
        assert!(
            err.contains("json.encode: a table contains itself"),
            "{err}"
        );
        let err = run_err("local t = {} for i = 1, 200 do t = {t} end return json.encode(t)").await;
        assert!(
            err.contains("json.encode: tables nested too deeply"),
            "{err}"
        );
        let err = run_err("return json.encode({f = print})").await;
        assert!(
            err.contains("json.encode: can't encode a function"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn json_decode_input_is_capped() {
        let err =
            quick_err(r#"return json.decode(string.rep(" ", 17 * 1024 * 1024) .. "1")"#).await;
        assert!(
            err.contains("json.decode: the input is over 16 MiB"),
            "{err}"
        );
        let err = run_err(r#"return json.decode("[1] x")"#).await;
        assert!(err.contains("json.decode: trailing characters"), "{err}");
    }

    #[tokio::test]
    async fn regex_replace_output_is_capped() {
        let err =
            quick_err(r#"return regex.replace("", string.rep("a", 1e6), string.rep("x", 6e4))"#)
                .await;
        assert!(
            err.contains("regex.replace: the result is over 32 MiB"),
            "{err}"
        );
        // An unclosed `${` is a literal `$`; the references after it still count.
        let err = quick_err(
            r#"return regex.replace("a+", string.rep("a", 2^20), "${" .. string.rep("$0", 3e4))"#,
        )
        .await;
        assert!(
            err.contains("regex.replace: the result is over 32 MiB"),
            "{err}"
        );
    }

    #[test]
    fn replace_all_matches_the_regex_crate() {
        let re = Regex::new(r"(?<x>a)(b)?").unwrap();
        let text = b"ab a zab";
        let templates: [&[u8]; 18] = [
            b"",
            b"-",
            b"[$2$1]",
            b"$$1",
            b"$$$1",
            b"${x}y",
            b"$xy",
            b"${1}0",
            b"${x",
            b"${x $1}",
            b"$",
            b"a$",
            b"$1$",
            b"${$1$1",
            b"${\xff$0}$1",
            b"${\xff}",
            b"$${1}",
            b"${}$0",
        ];
        // Each piece holds at most one capture reference: against a 21-byte match it grows
        // by at most 21 bytes.
        let go = AtomicBool::new(false);
        let long = Regex::new(r"(?<x>a{20})(b)?").unwrap();
        let caps = long.captures(b"aaaaaaaaaaaaaaaaaaaab").unwrap();
        for repl in templates {
            let shown = String::from_utf8_lossy(repl);
            assert_eq!(
                replace_all(&re, text, repl, usize::MAX, &go).unwrap(),
                &*re.replace_all(text, repl),
                "{shown}"
            );
            for piece in template_pieces(repl) {
                let mut out = Vec::new();
                caps.expand(piece, &mut out);
                assert!(out.len() <= piece.len() + 21, "{shown}: {piece:?}");
            }
        }
        let err = replace_all(&re, text, b"long", 8, &go).unwrap_err();
        assert!(err.to_string().contains("the result is over"), "{err}");
        assert_eq!(replace_all(&re, b"short", b"x", 5, &go).unwrap(), b"short");
        let err = replace_all(&re, text, b"x", usize::MAX, &AtomicBool::new(true)).unwrap_err();
        assert!(err.to_string().contains("regex took too long"), "{err}");
    }

    #[tokio::test]
    async fn regex_pattern_and_text_are_capped() {
        let err = quick_err(r#"return regex.match(string.rep("a", 65 * 1024), "a")"#).await;
        assert!(err.contains("regex: the pattern is over 64 KiB"), "{err}");
        let err = quick_err(r#"return regex.match("\\w{100}\\w{100}", "a")"#).await;
        assert!(err.contains("size limit"), "{err}");
        let err =
            quick_err(r#"return regex.find_all("a", string.rep("a", 2 * 1024 * 1024 + 1))"#).await;
        assert!(err.contains("regex: the text is over 2 MiB"), "{err}");
        let n: i64 =
            run(r#"return #regex.find_all("a", string.rep("b", 2 * 1024 * 1024 - 1) .. "a")"#)
                .await;
        assert_eq!(n, 1);
    }

    #[tokio::test]
    async fn regex_capture_groups_are_capped() {
        let n: i64 =
            run(r#"return #regex.match(string.rep("(a)", 63), string.rep("a", 63))"#).await;
        assert_eq!(n, 63);
        let err = run_err(r#"return regex.match(string.rep("(a)", 65), "a")"#).await;
        assert!(
            err.contains("regex: the pattern has over 64 capture groups"),
            "{err}"
        );
    }

    /// Twice the template cap, so a quadratic scan would take seconds even in release.
    #[test]
    fn template_pieces_is_linear() {
        let start = std::time::Instant::now();
        let template = b"${".repeat(64 * 1024);
        assert_eq!(template_pieces(&template).len(), 64 * 1024);
        let mut unclosed = b"${".repeat(64 * 1024);
        unclosed.extend_from_slice(b"\xff}");
        assert_eq!(template_pieces(&unclosed).len(), 64 * 1024);
        let took = start.elapsed();
        assert!(took < std::time::Duration::from_secs(1), "{took:?}");
    }

    #[tokio::test]
    async fn regex_replace_template_is_capped_and_scanned_linearly() {
        let len: i64 = run(r#"return #regex.replace("a", "a", string.rep("${", 32 * 1024))"#).await;
        assert_eq!(len, 64 * 1024);
        let start = std::time::Instant::now();
        let len: i64 = run(
            r#"return #regex.replace("a", "a", string.rep("${", 16 * 1024) .. string.rep("${\255}", 6 * 1024))"#,
        )
        .await;
        assert_eq!(len, 32 * 1024 + 6 * 1024 * 4);
        let took = start.elapsed();
        assert!(took < std::time::Duration::from_secs(1), "{took:?}");
        let err = quick_err(r#"return regex.replace("a", "a", string.rep("x", 65 * 1024))"#).await;
        assert!(
            err.contains("regex.replace: the replacement is over 64 KiB"),
            "{err}"
        );
    }

    /// Random a/b text: `a[ab]{1804}[^ab]` makes the lazy DFA give up on it, and the slow
    /// fallback then runs for seconds.
    fn ab_text(len: usize) -> String {
        let mut x: u64 = 88172645463325252;
        (0..len)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                if x & 1 == 0 { 'a' } else { 'b' }
            })
            .collect()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn regex_runs_off_the_lua_thread_under_the_deadline() {
        let mut host = HostConfig::default();
        host.deadline = std::time::Duration::from_millis(300);
        let code = r#"return {
            id = "t", name = "T", lang = "en", api = 1,
            search = function() return {} end,
            chapters = function() return {} end,
            pages = function() return {} end,
            slow = function(text) return regex.match("a[ab]{1804}[^ab]", text) end,
            quick = function() return regex.match("b+", "abba")[0] end,
        }"#;
        let libs = LibSource::Dir(PathBuf::from("/nonexistent"));
        let rt = Runtime::load(code, "t.lua", libs, host).unwrap();

        let start = std::time::Instant::now();
        let err = rt
            .call::<_, Value>("slow", ab_text(128 * 1024))
            .await
            .unwrap_err()
            .to_string();
        let took = start.elapsed();
        assert!(err.contains("regex took too long"), "{err}");
        assert!(took < std::time::Duration::from_secs(1), "{took:?}");

        // The timed-out job holds the plugin's one regex slot until it finishes; later calls
        // time out the same way until then, and work after.
        let give_up = std::time::Instant::now() + std::time::Duration::from_secs(120);
        loop {
            match rt.call::<_, String>("quick", ()).await {
                Ok(found) => break assert_eq!(found, "bb"),
                Err(e) => assert!(e.to_string().contains("regex took too long"), "{e}"),
            }
            assert!(
                std::time::Instant::now() < give_up,
                "the slow job never finished"
            );
        }
    }

    /// Runs `slow` (a Lua expression over `text`) with a 300 ms deadline. It must fail with
    /// `regex took too long` within a second, and the job must then stop between searches and
    /// give back the plugin's regex slot: a quick regex call works within a second after.
    async fn assert_regex_job_stops(slow: &str, text: String) {
        let mut host = HostConfig::default();
        host.deadline = std::time::Duration::from_millis(300);
        let code = format!(
            r#"return {{
            id = "t", name = "T", lang = "en", api = 1,
            search = function() return {{}} end,
            chapters = function() return {{}} end,
            pages = function() return {{}} end,
            slow = function(text) return {slow} end,
            quick = function() return regex.match("b+", "abba")[0] end,
        }}"#
        );
        let libs = LibSource::Dir(PathBuf::from("/nonexistent"));
        let rt = Runtime::load(&code, "t.lua", libs, host).unwrap();

        let start = std::time::Instant::now();
        let err = rt
            .call::<_, Value>("slow", text)
            .await
            .unwrap_err()
            .to_string();
        let took = start.elapsed();
        assert!(err.contains("regex took too long"), "{err}");
        assert!(took < std::time::Duration::from_secs(1), "{took:?}");

        let timed_out = std::time::Instant::now();
        loop {
            match rt.call::<_, String>("quick", ()).await {
                Ok(found) => break assert_eq!(found, "bb"),
                Err(e) => assert!(e.to_string().contains("regex took too long"), "{e}"),
            }
            let waited = timed_out.elapsed();
            assert!(
                waited < std::time::Duration::from_secs(1),
                "the timed-out job still held the slot after {waited:?}"
            );
        }
    }

    /// 256 KiB + 1 empty matches, each expanding 32,768 `$9` pieces: ~52 s in release.
    #[tokio::test(flavor = "multi_thread")]
    async fn regex_replace_job_stops_after_the_deadline() {
        assert_regex_job_stops(
            r#"regex.replace("", text, string.rep("$9", 32768))"#,
            "a".repeat(256 * 1024),
        )
        .await;
    }

    /// Every step scans to the end of the text for a `c`, then matches one `a`: quadratic.
    #[tokio::test(flavor = "multi_thread")]
    async fn regex_find_all_job_stops_after_the_deadline() {
        assert_regex_job_stops(
            r#"regex.find_all("[ab]*c|a", text)"#,
            "a".repeat(256 * 1024),
        )
        .await;
    }

    /// The cap counts the groups that took part, plus one per match: 20,000 matches of a
    /// 64-slot pattern where one group takes part fit, though 20,000 x 64 slots would not.
    #[tokio::test]
    async fn regex_find_all_cap_counts_participating_groups() {
        let (n, last): (i64, String) = run(r#"local all = regex.find_all(
                   string.rep("(b)|", 62) .. "(a)", string.rep("a", 20000))
               return #all, all[20000][63]"#)
        .await;
        assert_eq!((n, last.as_str()), (20000, "a"));
        let err = run_err(r#"return regex.find_all("", string.rep("a", 2^19))"#).await;
        assert!(
            err.contains("regex.find_all: over 1048576 captures"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn date_parse_of_non_strings_is_nil() {
        let (a, b, c): (Value, Value, Value) =
            run(r#"return date.parse(nil, "rfc3339"), date.parse("x", 5), date.parse({}, "%Y")"#)
                .await;
        assert_eq!((a, b, c), (Value::Nil, Value::Nil, Value::Nil));
    }

    #[tokio::test]
    async fn log_debug_reaches_sink_with_plugin_id() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink_seen = seen.clone();
        let mut host = HostConfig::default();
        host.log = Arc::new(move |id: &str, msg: &str| {
            sink_seen.lock().unwrap().push(format!("{id}: {msg}"));
        });
        let code = r#"
            log.debug("loading")
            return {
                id = "src", name = "S", lang = "en", api = 1,
                search = function() log.debug("searching " .. 3) return {} end,
                chapters = function() return {} end,
                pages = function() return {} end,
            }"#;
        let libs = LibSource::Dir(PathBuf::from("/nonexistent"));
        let rt = Runtime::load(code, "s.lua", libs, host).unwrap();
        rt.call::<_, ()>("search", "q").await.unwrap();
        assert_eq!(
            *seen.lock().unwrap(),
            ["s.lua: loading", "src: searching 3"]
        );
    }
}
