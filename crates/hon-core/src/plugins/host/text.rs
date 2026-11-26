//! The text helpers: `regex`, `json`, `base64`, `date`, plus `hon` and `log`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, STANDARD_PAD_INDIFFERENT};
use jiff::Timestamp;
use jiff::tz::TimeZone;
use mlua::{Lua, LuaSerdeExt, LuaString, Table, Value};
use regex::bytes::{Captures, Regex};

use crate::plugins::host::LogSink;

/// The plugin id `log.debug` reports: the chunk name until the manifest has been read.
pub(crate) struct PluginId(pub String);

const REGEX_CACHE_CAP: usize = 64;

/// One plugin's compiled patterns. When full it is emptied, then refilled as patterns are used.
#[derive(Default)]
struct RegexCache(Mutex<HashMap<String, Regex>>);

impl RegexCache {
    fn get(&self, pattern: &str) -> mlua::Result<Regex> {
        let mut cache = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(re) = cache.get(pattern) {
            return Ok(re.clone());
        }
        let re = Regex::new(pattern)
            .map_err(|e| mlua::Error::runtime(format!("invalid regex {pattern}: {e}")))?;
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
        lua.create_function(|_, (s, format): (LuaString, String)| {
            Ok(s.to_str().ok().and_then(|s| parse_date(&s, &format)))
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
            match c.get(&pattern)?.captures(&text.as_bytes()) {
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
                lua.create_string(re.replace_all(&text.as_bytes(), &*repl.as_bytes()))
            },
        )?,
    )?;

    module.raw_set(
        "escape",
        lua.create_function(|_, s: String| Ok(regex::escape(&s)))?,
    )?;
    Ok(module)
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

/// `json.decode`, `json.encode` and `json.null`, the value JSON `null` decodes to.
fn json_module(lua: &Lua) -> mlua::Result<Table> {
    let module = lua.create_table()?;
    module.raw_set(
        "decode",
        lua.create_function(|lua, s: LuaString| {
            let value: serde_json::Value = serde_json::from_slice(&s.as_bytes())
                .map_err(|e| mlua::Error::runtime(format!("json.decode: {e}")))?;
            lua.to_value(&value)
        })?,
    )?;
    module.raw_set(
        "encode",
        lua.create_function(|_, v: Value| {
            serde_json::to_string(&v.to_serializable().sort_keys(true))
                .map_err(|e| mlua::Error::runtime(format!("json.encode: {e}")))
        })?,
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
