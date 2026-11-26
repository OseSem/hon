//! One sandboxed Luau state per plugin: loading the chunk, reading its manifest, the
//! memory and time limits, and hon's own `require`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, anyhow, bail};
use mlua::chunk::ChunkMode;
use mlua::{FromLuaMulti, IntoLuaMulti, Lua, RegistryKey, Table, Value, VmState};
use sha2::{Digest, Sha256};

use crate::plugins::host::LogSink;
use crate::plugins::host::http::{self, Http};
use crate::plugins::host::text::PluginId;
use crate::plugins::host::{html, text};

/// The plugin interface version this hon implements. A plugin declaring a higher `api` needs a
/// newer hon.
pub const API_VERSION: u32 = 1;

const DEFAULT_REQUESTS_PER_SECOND: f64 = 4.0;
const REQUESTS_PER_SECOND_RANGE: std::ops::RangeInclusive<f64> = 0.1..=50.0;

/// What a plugin's returned table says about itself.
#[derive(Debug, Clone, PartialEq)]
pub struct Manifest {
    pub id: String,
    pub name: String,
    pub lang: String,
    pub api: u32,
    pub version: u32,
    pub nsfw: bool,
    pub requests_per_second: f64,
    /// Sent on every request the plugin makes, sorted by name.
    pub headers: Vec<(String, String)>,
    pub has_fetch_page: bool,
}

/// Where `require("name")` finds `<name>.lua`.
#[derive(Debug, Clone)]
pub enum LibSource {
    Dir(PathBuf),
    /// Only the modules in `hashes` load, and only when the file's SHA-256 (hex) matches.
    Verified {
        dir: PathBuf,
        hashes: HashMap<String, String>,
    },
}

/// The limits a plugin state runs under and the host modules it gets.
#[derive(Clone)]
pub struct HostConfig {
    /// Wall time for loading the chunk and for each call.
    pub deadline: Duration,
    /// Bytes the Luau state may allocate.
    pub memory_limit: usize,
    /// The plugin's `http`; `load` configures it from the manifest. One per plugin.
    pub http: Arc<Http>,
    /// Where `log.debug` goes. The default drops messages.
    pub log: LogSink,
    /// Lets tests install their own host functions through the real install path.
    #[cfg(test)]
    test_install: Option<fn(&Lua) -> mlua::Result<()>>,
}

impl Default for HostConfig {
    fn default() -> Self {
        HostConfig {
            deadline: Duration::from_secs(60),
            memory_limit: 64 << 20,
            http: Arc::new(Http::default()),
            log: Arc::new(|_, _| {}),
            #[cfg(test)]
            test_install: None,
        }
    }
}

impl std::fmt::Debug for HostConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostConfig")
            .field("deadline", &self.deadline)
            .field("memory_limit", &self.memory_limit)
            .field("http", &self.http)
            .finish_non_exhaustive()
    }
}

impl HostConfig {
    /// Registers the host modules as globals. Runs before `sandbox(true)` makes globals
    /// read-only, under the load deadline: building an async function runs a little Lua.
    fn install(&self, lua: &Lua) -> mlua::Result<()> {
        http::install(lua, self.http.clone())?;
        html::install(lua)?;
        text::install(lua, self.log.clone())?;
        #[cfg(test)]
        if let Some(install) = self.test_install {
            install(lua)?;
        }
        Ok(())
    }
}

/// When the running load or call must stop; checked by the interrupt. Outside a load or call
/// it is already past, so Lua the host runs by accident (an `__index` on a returned table)
/// stops at once instead of running unbounded.
struct Deadline(Instant);

/// Modules `require` has run, in load order.
#[derive(Default)]
struct Loaded {
    order: Vec<String>,
    modules: HashMap<String, RegistryKey>,
}

pub struct Runtime {
    lua: Lua,
    plugin: Table,
    pub manifest: Manifest,
    deadline: Duration,
    /// Held for a whole `call`: the deadline is per state, so calls must not interleave
    /// (a plugin's `coroutine.yield()` would otherwise let a second call start).
    calls: tokio::sync::Mutex<()>,
}

impl Runtime {
    /// Runs the plugin chunk in a fresh sandboxed state and reads its manifest.
    pub fn load(
        code: &str,
        chunk_name: &str,
        libs: LibSource,
        host: HostConfig,
    ) -> anyhow::Result<Runtime> {
        let lua = Lua::new();
        let in_chunk = |e: mlua::Error| anyhow!("{chunk_name}: {e}");
        lua.set_memory_limit(host.memory_limit).map_err(in_chunk)?;
        lua.set_app_data(Deadline(Instant::now()));
        lua.set_app_data(Loaded::default());
        lua.set_app_data(PluginId(chunk_name.to_string()));
        lua.set_interrupt(|lua| {
            let expired = lua
                .app_data_ref::<Deadline>()
                .is_none_or(|d| Instant::now() >= d.0);
            if expired {
                return Err(mlua::Error::runtime("plugin exceeded its time limit"));
            }
            Ok(VmState::Continue)
        });
        // Everything from here to the end of the manifest read can run Lua: installing host
        // modules (async function wrappers are built in Lua), the chunk, and `__index`
        // metamethods while reading the manifest.
        let deadline = DeadlineGuard::set(&lua, host.deadline);
        host.install(&lua).map_err(in_chunk)?;
        let require = lua
            .create_function(move |lua, name: String| require(lua, &libs, &name))
            .map_err(in_chunk)?;
        lua.globals().set("require", require).map_err(in_chunk)?;
        lua.sandbox(true).map_err(in_chunk)?;

        let loaded = lua
            .load(code)
            .set_name(format!("@{chunk_name}"))
            .set_mode(ChunkMode::Text)
            .eval::<Value>()
            .map_err(in_chunk)
            .and_then(|v| match v {
                Value::Table(plugin) => Ok(plugin),
                _ => bail!("{chunk_name}: plugin must return a table"),
            })
            .and_then(|plugin| {
                let manifest = read_manifest(&plugin).map_err(|e| anyhow!("{chunk_name}: {e}"))?;
                Ok((plugin, manifest))
            });
        drop(deadline);
        let (plugin, manifest) = loaded?;
        lua.set_app_data(PluginId(manifest.id.clone()));
        host.http
            .configure(&manifest)
            .map_err(|e| anyhow!("{chunk_name}: {e:#}"))?;
        Ok(Runtime {
            lua,
            plugin,
            manifest,
            deadline: host.deadline,
            calls: tokio::sync::Mutex::new(()),
        })
    }

    /// Calls `plugin[func](args)` under the deadline. Async host functions it calls yield
    /// instead of blocking. Calls on one `Runtime` run one at a time.
    ///
    /// `R` is converted while the deadline is armed. Lua values handed back (a `Table`) are
    /// read after it has passed, so read them with raw access (`raw_get`, `raw_len`,
    /// `sequence_values`); anything that runs a metamethod fails with the time-limit error.
    pub async fn call<A: IntoLuaMulti, R: FromLuaMulti>(
        &self,
        func: &str,
        args: A,
    ) -> anyhow::Result<R> {
        let _serialized = self.calls.lock().await;
        let _deadline = DeadlineGuard::set(&self.lua, self.deadline);
        let result = self.call_inner(func, args).await;
        result.map_err(|e| anyhow!("{}: {e}", self.manifest.name))
    }

    async fn call_inner<A: IntoLuaMulti, R: FromLuaMulti>(
        &self,
        func: &str,
        args: A,
    ) -> anyhow::Result<R> {
        let Value::Function(f) = self.plugin.get::<Value>(func)? else {
            bail!("plugin has no function `{func}`");
        };
        match tokio::time::timeout(self.deadline, f.call_async::<R>(args)).await {
            Ok(r) => Ok(r?),
            Err(_) => bail!("`{func}` exceeded its time limit"),
        }
    }

    /// The modules `require` has loaded so far, in order.
    pub fn required_modules(&self) -> Vec<String> {
        self.lua
            .app_data_ref::<Loaded>()
            .map(|l| l.order.clone())
            .unwrap_or_default()
    }
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn set_deadline(lua: &Lua, at: Instant) {
    if let Some(mut d) = lua.app_data_mut::<Deadline>() {
        d.0 = at;
    }
}

/// Arms the deadline, and expires it when dropped (also when a call's future is dropped
/// mid-call).
struct DeadlineGuard<'a>(&'a Lua);

impl<'a> DeadlineGuard<'a> {
    fn set(lua: &'a Lua, limit: Duration) -> Self {
        set_deadline(lua, Instant::now() + limit);
        DeadlineGuard(lua)
    }
}

impl Drop for DeadlineGuard<'_> {
    fn drop(&mut self) {
        set_deadline(self.0, Instant::now());
    }
}

fn require(lua: &Lua, libs: &LibSource, name: &str) -> mlua::Result<Value> {
    let cached = lua.app_data_ref::<Loaded>().and_then(|l| {
        l.modules
            .get(name)
            .map(|key| lua.registry_value::<Value>(key))
    });
    if let Some(module) = cached {
        return module;
    }
    let code = read_module(libs, name).map_err(|e| mlua::Error::runtime(format!("{e:#}")))?;
    let module = lua
        .load(code)
        .set_name(format!("@{name}.lua"))
        .set_mode(ChunkMode::Text)
        .eval::<Value>()?;
    let key = lua.create_registry_value(module.clone())?;
    if let Some(mut loaded) = lua.app_data_mut::<Loaded>() {
        loaded.order.push(name.to_string());
        loaded.modules.insert(name.to_string(), key);
    }
    Ok(module)
}

fn read_module(libs: &LibSource, name: &str) -> anyhow::Result<Vec<u8>> {
    let valid = !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
    if !valid {
        bail!("require: `{name}` is not a valid module name");
    }
    let (dir, expected) = match libs {
        LibSource::Dir(dir) => (dir, None),
        LibSource::Verified { dir, hashes } => {
            let Some(hash) = hashes.get(name) else {
                bail!("require: module `{name}` is not in the source index");
            };
            (dir, Some(hash))
        }
    };
    let path = dir.join(format!("{name}.lua"));
    let code = std::fs::read(&path).with_context(|| format!("require: module `{name}`"))?;
    if let Some(expected) = expected
        && !sha256_hex(&code).eq_ignore_ascii_case(expected)
    {
        bail!("require: module `{name}` doesn't match its hash in the source index");
    }
    Ok(code)
}

fn read_manifest(plugin: &Table) -> anyhow::Result<Manifest> {
    for func in ["search", "chapters", "pages"] {
        match field(plugin, func)? {
            Some(Value::Function(_)) => {}
            Some(_) => bail!("plugin field `{func}` must be a function"),
            None => bail!("plugin is missing `{func}`"),
        }
    }
    let has_fetch_page = match field(plugin, "fetch_page")? {
        Some(Value::Function(_)) => true,
        Some(_) => bail!("plugin field `fetch_page` must be a function"),
        None => false,
    };
    let requests_per_second = match field(plugin, "requests_per_second")? {
        None => DEFAULT_REQUESTS_PER_SECOND,
        Some(v) => match number(&v) {
            Some(n) if REQUESTS_PER_SECOND_RANGE.contains(&n) => n,
            _ => bail!("plugin field `requests_per_second` must be a number from 0.1 to 50"),
        },
    };
    let nsfw = match field(plugin, "nsfw")? {
        None => false,
        Some(Value::Boolean(b)) => b,
        Some(_) => bail!("plugin field `nsfw` must be a boolean"),
    };
    Ok(Manifest {
        id: required_string(plugin, "id")?,
        name: required_string(plugin, "name")?,
        lang: required_string(plugin, "lang")?,
        api: integer(plugin, "api")?.context("plugin is missing `api`")?,
        version: integer(plugin, "version")?.unwrap_or(1),
        nsfw,
        requests_per_second,
        headers: headers(plugin)?,
        has_fetch_page,
    })
}

fn field(plugin: &Table, key: &str) -> anyhow::Result<Option<Value>> {
    match plugin.get::<Value>(key)? {
        Value::Nil => Ok(None),
        v => Ok(Some(v)),
    }
}

/// Luau numbers with no fraction come back as `Value::Integer`.
fn number(v: &Value) -> Option<f64> {
    match *v {
        Value::Integer(n) => Some(n as f64),
        Value::Number(n) => Some(n),
        _ => None,
    }
}

fn required_string(plugin: &Table, key: &str) -> anyhow::Result<String> {
    match field(plugin, key)? {
        Some(Value::String(s)) => Ok(s.to_str()?.to_string()),
        Some(_) => bail!("plugin field `{key}` must be a string"),
        None => bail!("plugin is missing `{key}`"),
    }
}

fn integer(plugin: &Table, key: &str) -> anyhow::Result<Option<u32>> {
    let Some(v) = field(plugin, key)? else {
        return Ok(None);
    };
    let n = match v {
        Value::Integer(n) => Some(n),
        Value::Number(n) if n.fract() == 0.0 => Some(n as i64),
        _ => None,
    };
    match n.and_then(|n| u32::try_from(n).ok()) {
        Some(n) => Ok(Some(n)),
        None => bail!("plugin field `{key}` must be a whole number"),
    }
}

fn headers(plugin: &Table) -> anyhow::Result<Vec<(String, String)>> {
    let table = match field(plugin, "headers")? {
        None => return Ok(Vec::new()),
        Some(Value::Table(t)) => t,
        Some(_) => bail!("plugin field `headers` must be a table"),
    };
    let mut headers = Vec::new();
    for pair in table.pairs::<Value, Value>() {
        let (Value::String(k), Value::String(v)) = pair? else {
            bail!("plugin field `headers` must map strings to strings");
        };
        headers.push((k.to_str()?.to_string(), v.to_str()?.to_string()));
    }
    headers.sort();
    Ok(headers)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plugin(extra: &str) -> String {
        format!(
            r#"return {{
                id = "t", name = "T", lang = "en", api = 1,
                search = function(query) return {{}} end,
                chapters = function(manga) return {{}} end,
                pages = function(chapter) return {{}} end,
                {extra}
            }}"#
        )
    }

    fn no_libs() -> LibSource {
        LibSource::Dir(PathBuf::from("/nonexistent"))
    }

    fn load(code: &str) -> Runtime {
        Runtime::load(code, "t.lua", no_libs(), HostConfig::default()).unwrap()
    }

    fn load_err(code: &str) -> String {
        match Runtime::load(code, "t.lua", no_libs(), HostConfig::default()) {
            Ok(_) => panic!("expected the plugin to fail to load"),
            Err(e) => e.to_string(),
        }
    }

    async fn call_err(rt: &Runtime, func: &str) -> String {
        match rt.call::<_, mlua::Value>(func, ()).await {
            Ok(v) => panic!("expected `{func}` to fail, got {v:?}"),
            Err(e) => e.to_string(),
        }
    }

    #[test]
    fn runtime_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Runtime>();
    }

    #[tokio::test]
    async fn call_future_is_send() {
        fn assert_send<T: Send>(_: &T) {}
        let rt = load(&plugin(""));
        let fut = rt.call::<_, ()>("search", "x");
        assert_send(&fut);
        fut.await.unwrap();
    }

    #[tokio::test]
    async fn loads_manifest_with_defaults() {
        let rt = load(&plugin(""));
        let m = &rt.manifest;
        assert_eq!(m.id, "t");
        assert_eq!(m.name, "T");
        assert_eq!(m.lang, "en");
        assert_eq!(m.api, 1);
        assert_eq!(m.version, 1);
        assert!(!m.nsfw);
        assert_eq!(m.requests_per_second, 4.0);
        assert!(m.headers.is_empty());
        assert!(!m.has_fetch_page);
    }

    #[tokio::test]
    async fn loads_optional_manifest_fields() {
        let rt = load(&plugin(
            r#"version = 3, nsfw = true, requests_per_second = 0.5,
               headers = { Referer = "https://example.com/", ["X-A"] = "b" },
               fetch_page = function(page) return "" end,"#,
        ));
        let m = &rt.manifest;
        assert_eq!(m.version, 3);
        assert!(m.nsfw);
        assert_eq!(m.requests_per_second, 0.5);
        assert_eq!(
            m.headers,
            vec![
                ("Referer".to_string(), "https://example.com/".to_string()),
                ("X-A".to_string(), "b".to_string()),
            ]
        );
        assert!(m.has_fetch_page);
    }

    #[test]
    fn missing_required_field_is_named() {
        let code = r#"return {
            id = "t", name = "T", lang = "en", api = 1,
            search = function() end, chapters = function() end,
        }"#;
        let err = load_err(code);
        assert!(err.contains("t.lua: plugin is missing `pages`"), "{err}");
    }

    #[test]
    fn wrongly_typed_field_is_named() {
        let err = load_err(&plugin("").replace(r#"lang = "en""#, "lang = 1"));
        assert!(err.contains("`lang` must be a string"), "{err}");
        let err = load_err(&plugin("requests_per_second = 100,"));
        assert!(err.contains("`requests_per_second`"), "{err}");
        let rt = load(&plugin("requests_per_second = 10,"));
        assert_eq!(rt.manifest.requests_per_second, 10.0);
    }

    #[test]
    fn syntax_error_names_chunk() {
        let err = load_err("return {");
        assert!(err.starts_with("t.lua: "), "{err}");
    }

    #[tokio::test]
    async fn sandbox_blocks_io_and_os() {
        let bytecode = mlua::chunk::Compiler::new().compile("return 1").unwrap();
        let bytes: Vec<String> = bytecode.iter().map(u8::to_string).collect();
        let rt = load(&plugin(&format!(
            r#"io_open = function() return io.open("x") end,
               os_execute = function() return os.execute("true") end,
               bytecode = function() return loadstring(string.char({}))() end,
               load_fn = function() return load("return 1")() end,
               write_lib = function() string.foo = 1 end,"#,
            bytes.join(", ")
        )));
        for func in ["io_open", "os_execute", "bytecode", "load_fn", "write_lib"] {
            let err = call_err(&rt, func).await;
            assert!(err.starts_with("T: "), "{func}: {err}");
        }
        let err = call_err(&rt, "bytecode").await;
        assert!(err.contains("syntax error"), "{err}");
        let err = call_err(&rt, "write_lib").await;
        assert!(err.contains("readonly"), "{err}");
    }

    #[tokio::test]
    async fn plugin_functions_may_come_from_a_metatable() {
        let rt = load(
            r#"
            local methods = {
                search = function() return {} end,
                chapters = function() return {} end,
                pages = function() return {} end,
                answer = function() return 42 end,
            }
            return setmetatable(
                { id = "t", name = "T", lang = "en", api = 1 },
                { __index = methods }
            )"#,
        );
        assert_eq!(rt.call::<_, i64>("answer", ()).await.unwrap(), 42);
    }

    #[tokio::test]
    async fn memory_limit_stops_runaway_alloc() {
        let rt = load(&plugin(
            r#"big = function() return #string.rep("x", 100 * 1024 * 1024) end,
               small = function() return #string.rep("x", 1024) end,"#,
        ));
        let err = call_err(&rt, "big").await;
        assert!(err.contains("memory"), "{err}");
        assert_eq!(rt.call::<_, i64>("small", ()).await.unwrap(), 1024);
    }

    #[tokio::test]
    async fn deadline_stops_busy_loop() {
        let host = HostConfig {
            deadline: Duration::from_millis(200),
            ..HostConfig::default()
        };
        let code = plugin(
            r#"spin = function() while true do end end,
               quick = function() return 7 end,"#,
        );
        let rt = Runtime::load(&code, "t.lua", no_libs(), host).unwrap();
        let start = Instant::now();
        let err = call_err(&rt, "spin").await;
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "{:?}",
            start.elapsed()
        );
        assert!(err.contains("time limit"), "{err}");
        assert_eq!(rt.call::<_, i64>("quick", ()).await.unwrap(), 7);
    }

    #[test]
    fn deadline_stops_busy_loop_at_load() {
        let host = HostConfig {
            deadline: Duration::from_millis(200),
            ..HostConfig::default()
        };
        let err = match Runtime::load("while true do end", "t.lua", no_libs(), host) {
            Ok(_) => panic!("expected load to fail"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("time limit"), "{err}");
    }

    #[tokio::test]
    async fn async_host_functions_install_and_run() {
        fn install(lua: &Lua) -> mlua::Result<()> {
            let slow_answer = lua.create_async_function(|_, ()| async {
                tokio::time::sleep(Duration::from_millis(10)).await;
                Ok(42)
            })?;
            lua.globals().set("slow_answer", slow_answer)
        }
        let host = HostConfig {
            test_install: Some(install),
            ..HostConfig::default()
        };
        let code = plugin("answer = function() return slow_answer() end,");
        let rt = Runtime::load(&code, "t.lua", no_libs(), host).unwrap();
        assert_eq!(rt.call::<_, i64>("answer", ()).await.unwrap(), 42);
    }

    #[tokio::test]
    async fn calling_unknown_function_errors() {
        let rt = load(&plugin(""));
        let err = call_err(&rt, "nope").await;
        assert!(err.contains("T: plugin has no function `nope`"), "{err}");
    }

    fn lib_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("t.lua"),
            "local n = 0\nreturn { bump = function() n += 1; return n end }",
        )
        .unwrap();
        dir
    }

    const USES_T: &str = r#"
        local t = require("t")
        return {
            id = "t", name = "T", lang = "en", api = 1,
            search = function() return {} end,
            chapters = function() return {} end,
            pages = function() return {} end,
            same = function() return require("t") == t and require("t") == require("t") end,
            bump = function() return require("t").bump() end,
            bad = function() return require("../x") end,
            unknown = function() return require("other") end,
        }"#;

    #[tokio::test]
    async fn require_resolves_lib_dir_and_caches() {
        let dir = lib_dir();
        let libs = LibSource::Dir(dir.path().to_path_buf());
        let rt = Runtime::load(USES_T, "t.lua", libs, HostConfig::default()).unwrap();
        assert!(rt.call::<_, bool>("same", ()).await.unwrap());
        assert_eq!(rt.call::<_, i64>("bump", ()).await.unwrap(), 1);
        assert_eq!(rt.call::<_, i64>("bump", ()).await.unwrap(), 2);
        assert_eq!(rt.required_modules(), vec!["t".to_string()]);
    }

    #[tokio::test]
    async fn require_refuses_bad_names_and_hash_mismatch() {
        let dir = lib_dir();
        let libs = LibSource::Dir(dir.path().to_path_buf());
        let rt = Runtime::load(USES_T, "t.lua", libs, HostConfig::default()).unwrap();
        let err = call_err(&rt, "bad").await;
        assert!(err.contains("../x"), "{err}");
        let err = call_err(&rt, "unknown").await;
        assert!(err.contains("other"), "{err}");

        let wrong = LibSource::Verified {
            dir: dir.path().to_path_buf(),
            hashes: HashMap::from([("t".to_string(), sha256_hex(b"something else"))]),
        };
        let err = match Runtime::load(USES_T, "t.lua", wrong, HostConfig::default()) {
            Ok(_) => panic!("expected the hash check to fail"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("hash"), "{err}");

        let file = std::fs::read(dir.path().join("t.lua")).unwrap();
        let right = LibSource::Verified {
            dir: dir.path().to_path_buf(),
            hashes: HashMap::from([("t".to_string(), sha256_hex(&file))]),
        };
        let rt = Runtime::load(USES_T, "t.lua", right, HostConfig::default()).unwrap();
        assert!(rt.call::<_, bool>("same", ()).await.unwrap());
        let err = call_err(&rt, "unknown").await;
        assert!(err.contains("other"), "{err}");
    }

    #[test]
    fn sha256_hex_matches_known_digest() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    /// Runs `f` on its own thread and fails the test if it hasn't finished in 8 s, so a
    /// deadline that fails open shows up as a failure instead of a hung test run. `f` must own
    /// the `Runtime`, or dropping it after a timeout blocks on the spinning state.
    fn within_watchdog<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(f());
        });
        rx.recv_timeout(Duration::from_secs(8))
            .expect("plugin Lua ran past its deadline")
    }

    #[test]
    fn interleaved_calls_keep_the_deadline() {
        let host = HostConfig {
            deadline: Duration::from_millis(300),
            ..HostConfig::default()
        };
        let code = plugin(
            r#"a = function() coroutine.yield() return 1 end,
               b = function() coroutine.yield() coroutine.yield() while true do end end,"#,
        );
        let rt = Runtime::load(&code, "t.lua", no_libs(), host).unwrap();
        let (a, b) = within_watchdog(move || {
            let tokio = tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build()
                .unwrap();
            tokio.block_on(async {
                let (a, b) = futures::join!(rt.call::<_, i64>("a", ()), rt.call::<_, ()>("b", ()));
                (a.map_err(|e| e.to_string()), b.map_err(|e| e.to_string()))
            })
        });
        assert_eq!(a, Ok(1));
        let err = b.unwrap_err();
        assert!(err.contains("time limit"), "{err}");
    }

    #[tokio::test]
    async fn lua_outside_a_call_is_stopped() {
        let rt = load(&plugin(
            r#"evil = function()
                   return setmetatable({}, { __index = function() while true do end end })
               end,"#,
        ));
        let table = rt.call::<_, Table>("evil", ()).await.unwrap();
        // `rt` moves too: dropping it here while the thread spins would block on the state lock.
        let err = within_watchdog(move || {
            let _rt = rt;
            table.get::<Value>("x").unwrap_err().to_string()
        });
        assert!(err.contains("time limit"), "{err}");
    }

    #[tokio::test]
    async fn plugin_globals_live_in_its_own_environment() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("m.lua"),
            "return { read = function() return counter end }",
        )
        .unwrap();
        let code = r#"
            return {
                id = "t", name = "T", lang = "en", api = 1,
                search = function() end, chapters = function() end, pages = function() end,
                bump = function() counter = (counter or 0) + 1; return counter end,
                from_module = function() return require("m").read() end,
            }"#;
        let libs = LibSource::Dir(dir.path().to_path_buf());
        let rt = Runtime::load(code, "t.lua", libs, HostConfig::default()).unwrap();
        assert_eq!(rt.call::<_, i64>("bump", ()).await.unwrap(), 1);
        assert_eq!(rt.call::<_, i64>("bump", ()).await.unwrap(), 2);
        assert_eq!(rt.call::<_, i64>("from_module", ()).await.unwrap(), 2);

        // The plugin's environment proxies reads to the read-only shared globals, which the
        // write didn't touch.
        let env = rt.lua.globals();
        assert_eq!(env.raw_get::<i64>("counter").unwrap(), 2);
        let shared: Table = env.metatable().unwrap().raw_get("__index").unwrap();
        assert!(shared.is_readonly());
        assert_eq!(shared.raw_get::<Value>("counter").unwrap(), Value::Nil);
    }
}
