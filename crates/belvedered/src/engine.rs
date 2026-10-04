//! Runs a model by way of the `belvedere-model` helper process. Loading
//! starts the helper; unloading kills it, which returns every byte it
//! held. One model at a time.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use belvedere_core::model_ipc::{Command, Event};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::sync::mpsc;
use tracing::{info, warn};

/// What the engine is doing right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    Unloaded,
    Loading { name: String },
    Ready { name: String },
    Generating { name: String },
}

impl State {
    pub fn label(&self) -> &'static str {
        match self {
            State::Unloaded => "unloaded",
            State::Loading { .. } => "loading",
            State::Ready { .. } => "ready",
            State::Generating { .. } => "generating",
        }
    }

    pub fn model_name(&self) -> &str {
        match self {
            State::Unloaded => "",
            State::Loading { name } | State::Ready { name } | State::Generating { name } => name,
        }
    }
}

/// Facts about a load, for the log and the PR.
#[derive(Debug, Clone)]
pub struct Loaded {
    pub gpu_layers: u32,
    pub context: u32,
    pub load_time: Duration,
}

/// A piece of a streamed reply.
#[derive(Debug, Clone)]
pub enum Chunk {
    Text(String),
    Done { tokens: u32, seconds: f32 },
    Failed(String),
}

type Listener = Arc<Mutex<Option<mpsc::UnboundedSender<Chunk>>>>;

struct Helper {
    child: Child,
    stdin: ChildStdin,
    /// Where events from the helper go while a generation is running.
    listener: Listener,
}

struct Inner {
    state: State,
    /// When the model last did anything, for the idle unload.
    last_used: Instant,
    helper: Option<Helper>,
}

/// Handle to the model process. Cheap to clone.
#[derive(Clone)]
pub struct Engine {
    inner: Arc<tokio::sync::Mutex<Inner>>,
    /// A copy of (state, last_used) readable without awaiting.
    status: Arc<Mutex<(State, Instant)>>,
}

impl Default for Engine {
    fn default() -> Self {
        Self::new()
    }
}

impl Engine {
    pub fn new() -> Self {
        Engine {
            inner: Arc::new(tokio::sync::Mutex::new(Inner {
                state: State::Unloaded,
                last_used: Instant::now(),
                helper: None,
            })),
            status: Arc::new(Mutex::new((State::Unloaded, Instant::now()))),
        }
    }

    pub fn state(&self) -> State {
        self.status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .0
            .clone()
    }

    pub fn last_used(&self) -> Instant {
        self.status.lock().unwrap_or_else(|e| e.into_inner()).1
    }

    fn publish(&self, inner: &Inner) {
        *self.status.lock().unwrap_or_else(|e| e.into_inner()) =
            (inner.state.clone(), inner.last_used);
    }

    /// Loads a model, replacing whatever was loaded. `gpu` false keeps
    /// everything on the CPU.
    pub async fn load(&self, path: PathBuf, name: String, gpu: bool) -> Result<Loaded, String> {
        let mut inner = self.inner.lock().await;
        if let Some(old) = inner.helper.take() {
            stop_helper(old).await;
        }
        inner.state = State::Loading { name: name.clone() };
        inner.last_used = Instant::now();
        self.publish(&inner);

        match start_helper(&path, gpu, self.clone()).await {
            Ok((helper, loaded)) => {
                inner.helper = Some(helper);
                inner.state = State::Ready { name };
                inner.last_used = Instant::now();
                self.publish(&inner);
                Ok(loaded)
            }
            Err(err) => {
                inner.state = State::Unloaded;
                self.publish(&inner);
                Err(err)
            }
        }
    }

    pub async fn unload(&self) {
        let mut inner = self.inner.lock().await;
        if let Some(helper) = inner.helper.take() {
            stop_helper(helper).await;
            info!("model unloaded");
        }
        inner.state = State::Unloaded;
        inner.last_used = Instant::now();
        self.publish(&inner);
    }

    /// Streams a reply. Dropping the receiver cancels generation.
    pub async fn generate(
        &self,
        prompt: String,
        max_tokens: u32,
    ) -> mpsc::UnboundedReceiver<Chunk> {
        let (out, rx) = mpsc::unbounded_channel();
        let mut inner = self.inner.lock().await;
        let Some(helper) = inner.helper.as_mut() else {
            let _ = out.send(Chunk::Failed("no model is loaded".into()));
            return rx;
        };
        *helper.listener.lock().unwrap_or_else(|e| e.into_inner()) = Some(out.clone());
        let cmd = Command::Generate { prompt, max_tokens };
        if let Err(err) = send(&mut helper.stdin, &cmd).await {
            let _ = out.send(Chunk::Failed(format!("model helper unreachable: {err}")));
            return rx;
        }
        let name = inner.state.model_name().to_string();
        inner.state = State::Generating { name };
        inner.last_used = Instant::now();
        self.publish(&inner);
        rx
    }

    /// Called by the reader task when a generation ends.
    async fn generation_ended(&self) {
        let mut inner = self.inner.lock().await;
        if let State::Generating { name } = &inner.state {
            inner.state = State::Ready { name: name.clone() };
        }
        inner.last_used = Instant::now();
        self.publish(&inner);
    }

    /// Called by the reader task when the helper's output closes.
    async fn helper_gone(&self) {
        let mut inner = self.inner.lock().await;
        if inner.helper.is_some() && !matches!(inner.state, State::Loading { .. }) {
            warn!("model helper exited unexpectedly");
            if let Some(helper) = inner.helper.take() {
                let listener = helper
                    .listener
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .take();
                if let Some(l) = listener {
                    let _ = l.send(Chunk::Failed("the model process exited".into()));
                }
            }
            inner.state = State::Unloaded;
            self.publish(&inner);
        }
    }

    async fn cancel(&self) {
        let mut inner = self.inner.lock().await;
        if let Some(helper) = inner.helper.as_mut() {
            let _ = send(&mut helper.stdin, &Command::Cancel).await;
        }
    }
}

async fn send(stdin: &mut ChildStdin, cmd: &Command) -> std::io::Result<()> {
    let mut line = serde_json::to_string(cmd).expect("command serializes");
    line.push('\n');
    stdin.write_all(line.as_bytes()).await?;
    stdin.flush().await
}

/// Where the helper binary lives: beside this executable, or on PATH.
/// `BELVEDERE_MODEL_HELPER` overrides (tests use it).
pub fn helper_path() -> PathBuf {
    if let Some(p) = std::env::var_os("BELVEDERE_MODEL_HELPER") {
        return PathBuf::from(p);
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let sibling = dir.join("belvedere-model");
            if sibling.is_file() {
                return sibling;
            }
        }
    }
    PathBuf::from("belvedere-model")
}

/// Whether the GPU is to be used: on unless `BELVEDERE_FORCE_CPU` is set.
pub fn gpu_enabled() -> bool {
    std::env::var_os("BELVEDERE_FORCE_CPU").is_none()
}

/// Starts the helper and waits for it to report the model loaded.
async fn start_helper(
    path: &PathBuf,
    gpu: bool,
    engine: Engine,
) -> Result<(Helper, Loaded), String> {
    let mut cmd = tokio::process::Command::new(helper_path());
    cmd.arg("--model").arg(path);
    if !gpu {
        cmd.arg("--cpu");
    }
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = cmd.spawn().map_err(|e| {
        format!(
            "could not start the model helper ({}): {e}",
            helper_path().display()
        )
    })?;
    let stdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");

    // The helper's log lines become ours.
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            info!(target: "belvedere_model", "{line}");
        }
    });

    let listener: Listener = Arc::new(Mutex::new(None));
    let (loaded_tx, loaded_rx) = tokio::sync::oneshot::channel::<Result<Loaded, String>>();
    let reader_listener = listener.clone();
    tokio::spawn(async move {
        let mut loaded_tx = Some(loaded_tx);
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let event = match serde_json::from_str::<Event>(&line) {
                Ok(ev) => ev,
                Err(err) => {
                    warn!("bad line from model helper: {err}");
                    continue;
                }
            };
            match event {
                Event::Loaded {
                    gpu_layers,
                    context,
                    load_ms,
                } => {
                    if let Some(tx) = loaded_tx.take() {
                        let _ = tx.send(Ok(Loaded {
                            gpu_layers,
                            context,
                            load_time: Duration::from_millis(load_ms),
                        }));
                    }
                }
                Event::Text { text } => {
                    let dropped = {
                        let guard = reader_listener.lock().unwrap_or_else(|e| e.into_inner());
                        guard
                            .as_ref()
                            .is_some_and(|l| l.send(Chunk::Text(text)).is_err())
                    };
                    if dropped {
                        // Listener went away: stop the generation.
                        reader_listener
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .take();
                        engine.cancel().await;
                    }
                }
                Event::Done { tokens, seconds } => {
                    let listener = reader_listener
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .take();
                    if let Some(l) = listener {
                        let _ = l.send(Chunk::Done { tokens, seconds });
                    }
                    engine.generation_ended().await;
                }
                Event::Error { message } => {
                    if let Some(tx) = loaded_tx.take() {
                        let _ = tx.send(Err(message));
                        continue;
                    }
                    let listener = reader_listener
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .take();
                    if let Some(l) = listener {
                        let _ = l.send(Chunk::Failed(message));
                    }
                    engine.generation_ended().await;
                }
            }
        }
        engine.helper_gone().await;
    });

    let loaded = match tokio::time::timeout(Duration::from_secs(300), loaded_rx).await {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => Err("the model helper exited before loading".to_string()),
        Err(_) => Err("the model took more than five minutes to load".to_string()),
    };
    match loaded {
        Ok(loaded) => {
            info!(
                gpu_layers = loaded.gpu_layers,
                context = loaded.context,
                ms = loaded.load_time.as_millis() as u64,
                "model loaded"
            );
            Ok((
                Helper {
                    child,
                    stdin,
                    listener,
                },
                loaded,
            ))
        }
        Err(err) => {
            let _ = child.kill().await;
            Err(err)
        }
    }
}

/// Asks the helper to exit by closing its stdin, then makes sure.
async fn stop_helper(mut helper: Helper) {
    drop(helper.stdin);
    if tokio::time::timeout(Duration::from_secs(2), helper.child.wait())
        .await
        .is_err()
    {
        let _ = helper.child.kill().await;
    }
}

/// Whether to unload now: a model is loaded, nothing is using it, and it
/// has sat idle for `idle_after`. Pure, so it can be tested at any clock.
pub fn should_unload(
    state: &State,
    last_used: Instant,
    now: Instant,
    idle_after: Duration,
) -> bool {
    matches!(state, State::Ready { .. }) && now.saturating_duration_since(last_used) >= idle_after
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unload_only_when_ready_and_idle_long_enough() {
        let idle = Duration::from_secs(300);
        let t0 = Instant::now();
        let ready = State::Ready { name: "m".into() };
        assert!(!should_unload(
            &ready,
            t0,
            t0 + Duration::from_secs(299),
            idle
        ));
        assert!(should_unload(
            &ready,
            t0,
            t0 + Duration::from_secs(300),
            idle
        ));
        assert!(should_unload(
            &ready,
            t0,
            t0 + Duration::from_secs(3000),
            idle
        ));

        // Busy or already unloaded: never.
        let generating = State::Generating { name: "m".into() };
        assert!(!should_unload(
            &generating,
            t0,
            t0 + Duration::from_secs(3000),
            idle
        ));
        assert!(!should_unload(
            &State::Unloaded,
            t0,
            t0 + Duration::from_secs(3000),
            idle
        ));
        let loading = State::Loading { name: "m".into() };
        assert!(!should_unload(
            &loading,
            t0,
            t0 + Duration::from_secs(3000),
            idle
        ));
    }

    #[test]
    fn state_labels_and_names() {
        assert_eq!(State::Unloaded.label(), "unloaded");
        assert_eq!(State::Unloaded.model_name(), "");
        let s = State::Ready {
            name: "Qwen".into(),
        };
        assert_eq!(s.label(), "ready");
        assert_eq!(s.model_name(), "Qwen");
    }
}
