//! The modules the host gives every plugin as globals.

use std::sync::Arc;

pub mod html;
pub mod http;
pub mod text;

/// Where `log.debug` goes: called with the plugin id and the message.
pub type LogSink = Arc<dyn Fn(&str, &str) + Send + Sync>;

#[cfg(test)]
pub(crate) mod testing {
    use std::path::PathBuf;

    use crate::plugins::runtime::{HostConfig, LibSource, Runtime};

    /// A plugin with id "t" whose `run` function has `body` as its body.
    pub fn runtime_with(body: &str, host: HostConfig) -> Runtime {
        let code = format!(
            r#"return {{
                id = "t", name = "T", lang = "en", api = 1,
                search = function() return {{}} end,
                chapters = function() return {{}} end,
                pages = function() return {{}} end,
                run = function() {body} end,
            }}"#
        );
        let libs = LibSource::Dir(PathBuf::from("/nonexistent"));
        Runtime::load(&code, "t.lua", libs, host).unwrap()
    }

    pub fn runtime(body: &str) -> Runtime {
        runtime_with(body, HostConfig::default())
    }

    /// Runs `body` as a plugin function and returns what it returns.
    pub async fn run<R: mlua::FromLuaMulti>(body: &str) -> R {
        runtime(body).call::<_, R>("run", ()).await.unwrap()
    }

    /// Runs `body` and returns the error it raises.
    pub async fn run_err(body: &str) -> String {
        match runtime(body).call::<_, mlua::Value>("run", ()).await {
            Ok(v) => panic!("expected an error, got {v:?}"),
            Err(e) => e.to_string(),
        }
    }
}
