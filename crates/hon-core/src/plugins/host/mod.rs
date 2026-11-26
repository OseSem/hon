//! The modules the host gives every plugin as globals.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use mlua::Lua;
use tokio::sync::Semaphore;

use crate::plugins::runtime;

pub mod html;
pub mod http;
pub mod text;

/// Where `log.debug` goes: called with the plugin id and the message.
pub type LogSink = Arc<dyn Fn(&str, &str) + Send + Sync>;

/// A job gives up this long before the call's deadline: once the deadline passes, the Lua that
/// would raise the job's error is itself stopped, and the plugin sees only the generic
/// time-limit error.
const JOB_DEADLINE_MARGIN: Duration = Duration::from_millis(50);

/// One plugin state's heavy host work (`regex` matching, `html` parsing and selecting) on
/// tokio's blocking pool, one job at a time, so it can't hold up the async threads or run past
/// the call's deadline.
///
/// A job the plugin stopped waiting for is told to stop, and does so at its next step: the
/// next match or template piece for `regex`, the next 4 KiB of input for `html.parse`, the next
/// element for `select`. A step itself can't be stopped. The job keeps the slot until it
/// returns, so a plugin can't pile up runaway threads, and its later jobs wait for the slot
/// within their own deadline. hon's exit waits for running jobs, so it waits for at most one
/// such step per plugin state.
#[derive(Clone)]
pub(crate) struct HostJobs {
    slot: Arc<Semaphore>,
}

impl Default for HostJobs {
    fn default() -> Self {
        HostJobs {
            slot: Arc::new(Semaphore::new(1)),
        }
    }
}

impl HostJobs {
    /// Runs `job` until the current call's deadline; `what` names it in the error when it
    /// takes too long ("regex took too long"). The flag the job gets is set once this call
    /// stops waiting, timed out or dropped; see [`stopped`].
    pub(crate) async fn run<T: Send + 'static>(
        &self,
        lua: &Lua,
        what: &str,
        job: impl FnOnce(&AtomicBool) -> mlua::Result<T> + Send + 'static,
    ) -> mlua::Result<T> {
        let too_long = || mlua::Error::runtime(format!("{what} took too long"));
        let Some(deadline) = runtime::deadline(lua) else {
            return Err(mlua::Error::runtime("plugin exceeded its time limit"));
        };
        let deadline = tokio::time::Instant::from_std(deadline) - JOB_DEADLINE_MARGIN;
        let slot = tokio::time::timeout_at(deadline, self.slot.clone().acquire_owned())
            .await
            .map_err(|_| too_long())?
            .map_err(mlua::Error::external)?;
        let stop = StopOnDrop(Arc::default());
        let job_stop = stop.0.clone();
        let task = tokio::task::spawn_blocking(move || {
            let _slot = slot;
            job(&job_stop)
        });
        match tokio::time::timeout_at(deadline, task).await {
            Ok(Ok(result)) => result,
            Ok(Err(e)) => Err(mlua::Error::runtime(format!("{what} failed: {e}"))),
            Err(_) => Err(too_long()),
        }
    }
}

/// Sets a job's stop flag when the call that started it stops waiting.
struct StopOnDrop(Arc<AtomicBool>);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// Fails once the call waiting for this job has stopped waiting. Nobody sees the error: it
/// only ends the job.
pub(crate) fn stopped(stop: &AtomicBool) -> mlua::Result<()> {
    if stop.load(Ordering::Relaxed) {
        return Err(mlua::Error::runtime("stopped: nothing waits for this job"));
    }
    Ok(())
}

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
