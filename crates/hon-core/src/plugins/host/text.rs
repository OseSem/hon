//! The text helpers: `regex`, `json`, `base64`, `date`, plus `hon` and `log`.
//!
//! Host functions run outside the VM's interrupt and allocate outside the Lua memory limit, so
//! each one caps its input or output: a plugin can build ~60 MiB strings and tables that share
//! subtables, and must not turn them into unbounded work here.

use std::collections::HashMap;
use std::ffi::c_void;
use std::io;
use std::ops::Range;
use std::sync::{Arc, Mutex, PoisonError};

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, STANDARD_PAD_INDIFFERENT};
use jiff::Timestamp;
use jiff::tz::TimeZone;
use mlua::{Lua, LuaSerdeExt, LuaString, Table, Value};
use regex::bytes::{Captures, Regex, RegexBuilder};

use crate::plugins::host::LogSink;

/// The plugin id `log.debug` reports: the chunk name until the manifest has been read.
pub(crate) struct PluginId(pub String);

const JSON_ENCODE_CAP: usize = 32 << 20;
const JSON_DECODE_CAP: usize = 16 << 20;
const PATTERN_CAP: usize = 64 << 10;
/// Both the compiled program's and the lazy DFA's size limit, per pattern.
const REGEX_SIZE_LIMIT: usize = 1 << 20;
const REGEX_TEXT_CAP: usize = 32 << 20;
const REPLACE_CAP: usize = 32 << 20;
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
            .dfa_size_limit(REGEX_SIZE_LIMIT)
            .build()
            .map_err(|e| mlua::Error::runtime(format!("invalid regex: {e}")))?;
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
fn regex_module(lua: &Lua) -> mlua::Result<Table> {
    let cache = Arc::new(RegexCache::default());
    let module = lua.create_table()?;

    let c = cache.clone();
    module.raw_set(
        "match",
        lua.create_function(move |lua, (pattern, text): (String, LuaString)| {
            let re = c.get(&pattern)?;
            let text = text.as_bytes();
            check_text(&text)?;
            match re.captures(&text) {
                Some(caps) => Ok(Some(captures(lua, &caps)?)),
                None => Ok(None),
            }
        })?,
    )?;

    let c = cache.clone();
    module.raw_set(
        "find_all",
        lua.create_function(move |lua, (pattern, text): (String, LuaString)| {
            let re = c.get(&pattern)?;
            let text = text.as_bytes();
            check_text(&text)?;
            let all = re
                .captures_iter(&text)
                .map(|caps| captures(lua, &caps))
                .collect::<mlua::Result<Vec<_>>>()?;
            lua.create_sequence_from(all)
        })?,
    )?;

    module.raw_set(
        "replace",
        lua.create_function(
            move |lua, (pattern, text, repl): (String, LuaString, LuaString)| {
                let re = cache.get(&pattern)?;
                let text = text.as_bytes();
                check_text(&text)?;
                let out = replace_all(&re, &text, &repl.as_bytes(), REPLACE_CAP)
                    .ok_or_else(|| over_cap("regex.replace: the result", REPLACE_CAP))?;
                lua.create_string(out)
            },
        )?,
    )?;

    module.raw_set(
        "escape",
        lua.create_function(|_, s: String| Ok(regex::escape(&s)))?,
    )?;
    Ok(module)
}

fn check_text(text: &[u8]) -> mlua::Result<()> {
    if text.len() > REGEX_TEXT_CAP {
        return Err(over_cap("regex: the text", REGEX_TEXT_CAP));
    }
    Ok(())
}

/// `[0]` is the whole match, `[n]` group n; a group that didn't take part is absent.
fn captures(lua: &Lua, caps: &Captures) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    for (i, m) in caps.iter().enumerate() {
        if let Some(m) = m {
            table.raw_set(i, lua.create_string(m.as_bytes())?)?;
        }
    }
    Ok(table)
}

/// `re.replace_all(text, repl)`, or `None` once the result passes `cap`. Built one piece at a
/// time: a single expansion of `repl` can repeat a long match many times.
fn replace_all(re: &Regex, text: &[u8], repl: &[u8], cap: usize) -> Option<Vec<u8>> {
    let pieces = template_pieces(repl);
    let mut out = Vec::new();
    let mut last = 0;
    for caps in re.captures_iter(text) {
        let whole = caps.get(0).expect("group 0 always takes part");
        out.extend_from_slice(&text[last..whole.start()]);
        for piece in &pieces {
            caps.expand(piece, &mut out);
            if out.len() > cap {
                return None;
            }
        }
        last = whole.end();
    }
    out.extend_from_slice(&text[last..]);
    (out.len() <= cap).then_some(out)
}

/// `repl` cut before each `$` that starts a capture reference, so each piece expands to at
/// most one capture and expanding the pieces in turn equals expanding `repl`. `$$` and
/// `${...}` stay whole.
fn template_pieces(repl: &[u8]) -> Vec<&[u8]> {
    let mut pieces = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < repl.len() {
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
            Some(b'{') => repl[i..]
                .iter()
                .position(|&b| b == b'}')
                .map_or(repl.len(), |p| i + p + 1),
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
            quick_err(r#"return regex.replace("", string.rep("a", 1e6), string.rep("x", 1e5))"#)
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
        for repl in [
            "", "-", "[$2$1]", "$$1", "$$$1", "${x}y", "$xy", "${1}0", "${x", "${x $1}", "$", "a$",
            "$1$",
        ] {
            assert_eq!(
                replace_all(&re, text, repl.as_bytes(), usize::MAX).unwrap(),
                &*re.replace_all(text, repl.as_bytes()),
                "{repl}"
            );
        }
        assert_eq!(replace_all(&re, text, b"long", 8), None);
        assert_eq!(replace_all(&re, b"short", b"x", 5).unwrap(), b"short");
    }

    #[tokio::test]
    async fn regex_pattern_and_text_are_capped() {
        let err = quick_err(r#"return regex.match(string.rep("a", 65 * 1024), "a")"#).await;
        assert!(err.contains("regex: the pattern is over 64 KiB"), "{err}");
        let err = quick_err(r#"return regex.match("\\w{100}\\w{100}", "a")"#).await;
        assert!(err.contains("size limit"), "{err}");
        let err =
            quick_err(r#"return regex.find_all("a", string.rep("a", 33 * 1024 * 1024))"#).await;
        assert!(err.contains("regex: the text is over 32 MiB"), "{err}");
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
