//! Runs one model in its own process so that unloading it is just
//! exiting, and a crash inside llama.cpp cannot take the service down.
//!
//! Usage: `belvedere-model --model PATH [--cpu]`. Reads commands on stdin,
//! writes events on stdout, one JSON object per line (see
//! `belvedere_core::model_ipc`). The whole process runs at low CPU and I/O
//! priority.

use std::io::{BufRead, Write};
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Instant;

use belvedere_core::model_ipc::{Command, Event, Grammar};
use llama_cpp_2::context::params::LlamaContextParams;
use llama_cpp_2::llama_backend::LlamaBackend;
use llama_cpp_2::llama_batch::LlamaBatch;
use llama_cpp_2::model::params::LlamaModelParams;
use llama_cpp_2::model::LlamaChatMessage;
use llama_cpp_2::model::LlamaModel;
use llama_cpp_2::sampling::LlamaSampler;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

/// Context window to ask for, capped by what the model was trained on.
const CONTEXT: u32 = 8192;
/// Prompt tokens decoded per batch.
const BATCH: u32 = 512;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("{}", belvedere_core::version_line("belvedere-model"));
        return;
    }
    let model_path = args
        .iter()
        .position(|a| a == "--model")
        .and_then(|i| args.get(i + 1))
        .map(PathBuf::from);
    let Some(model_path) = model_path else {
        eprintln!("usage: belvedere-model --model PATH [--cpu]");
        std::process::exit(2);
    };
    let gpu = !args.iter().any(|a| a == "--cpu");

    lower_priority();
    init_logging();

    let mut out = std::io::stdout().lock();
    let emit = |out: &mut std::io::StdoutLock<'_>, ev: &Event| {
        let line = serde_json::to_string(ev).expect("event serializes");
        let _ = writeln!(out, "{line}");
        let _ = out.flush();
    };

    let backend = match LlamaBackend::init() {
        Ok(b) => b,
        Err(err) => {
            emit(
                &mut out,
                &Event::Error {
                    message: format!("llama.cpp backend failed to start: {err}"),
                },
            );
            std::process::exit(1);
        }
    };

    let started = Instant::now();
    let gpu_layers = if gpu { 1000 } else { 0 };
    let params = LlamaModelParams::default().with_n_gpu_layers(gpu_layers);
    let model = match LlamaModel::load_from_file(&backend, &model_path, &params) {
        Ok(m) => m,
        Err(err) => {
            emit(
                &mut out,
                &Event::Error {
                    message: format!("could not load {}: {err}", model_path.display()),
                },
            );
            std::process::exit(1);
        }
    };
    let n_ctx = CONTEXT.min(model.n_ctx_train().max(512));
    // Half the cores: enough to be quick, never enough to starve the desktop.
    let threads = (std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        / 2)
    .max(1) as i32;
    emit(
        &mut out,
        &Event::Loaded {
            gpu_layers,
            context: n_ctx,
            load_ms: started.elapsed().as_millis() as u64,
        },
    );

    // Commands arrive on a separate thread so Cancel can interrupt a
    // generation in progress.
    let cancel = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel::<Command>();
    {
        let cancel = cancel.clone();
        std::thread::spawn(move || {
            let stdin = std::io::stdin();
            for line in stdin.lock().lines() {
                let Ok(line) = line else { break };
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<Command>(&line) {
                    Ok(Command::Cancel) => cancel.store(true, Ordering::Relaxed),
                    Ok(cmd) => {
                        if tx.send(cmd).is_err() {
                            break;
                        }
                    }
                    Err(err) => tracing::warn!("ignoring bad command line: {err}"),
                }
            }
            // stdin closed: the service is gone or wants us gone.
            std::process::exit(0);
        });
    }

    while let Ok(cmd) = rx.recv() {
        let (prompt, max_tokens, grammar) = match cmd {
            Command::Generate { prompt, max_tokens } => (prompt, max_tokens, None),
            Command::Chat {
                messages,
                max_tokens,
                grammar,
                no_think,
            } => match render_chat(&model, &messages) {
                Ok(mut prompt) => {
                    // Pre-fill an empty reasoning block so a Qwen-style
                    // model answers directly instead of thinking first.
                    if no_think && prompt.contains("<|im_start|>") {
                        prompt.push_str("<think>\n\n</think>\n\n");
                    }
                    (prompt, max_tokens, grammar)
                }
                Err(message) => {
                    emit(&mut out, &Event::Error { message });
                    continue;
                }
            },
            Command::Cancel => continue,
        };
        cancel.store(false, Ordering::Relaxed);
        debug_log(&format!(
            "PROMPT TAIL: {:?}",
            &prompt[prompt.len().saturating_sub(400)..]
        ));
        if let Err(message) = generate(
            &backend,
            &model,
            n_ctx,
            threads,
            &prompt,
            max_tokens,
            grammar.as_ref(),
            &cancel,
            &mut out,
            &emit,
        ) {
            emit(&mut out, &Event::Error { message });
        }
    }
}

/// Lays a conversation out the way this model was trained to read it,
/// using the chat template stored in the model file, and opens the
/// assistant's turn.
fn render_chat(
    model: &LlamaModel,
    messages: &[belvedere_core::model_ipc::ChatMessage],
) -> Result<String, String> {
    let template = model
        .chat_template(None)
        .map_err(|e| format!("this model has no chat template: {e}"))?;
    let chat: Vec<LlamaChatMessage> = messages
        .iter()
        .map(|m| LlamaChatMessage::new(m.role.clone(), m.content.clone()))
        .collect::<Result<_, _>>()
        .map_err(|e| format!("bad chat message: {e}"))?;
    model
        .apply_chat_template(&template, &chat, true)
        .map_err(|e| format!("could not apply the chat template: {e}"))
}

#[allow(clippy::too_many_arguments)]
fn generate(
    backend: &LlamaBackend,
    model: &LlamaModel,
    n_ctx: u32,
    threads: i32,
    prompt: &str,
    max_tokens: u32,
    grammar: Option<&Grammar>,
    cancel: &AtomicBool,
    out: &mut std::io::StdoutLock<'_>,
    emit: &dyn Fn(&mut std::io::StdoutLock<'_>, &Event),
) -> Result<(), String> {
    let started = Instant::now();
    let params = LlamaContextParams::default()
        .with_n_ctx(NonZeroU32::new(n_ctx))
        .with_n_batch(BATCH)
        .with_n_threads(threads)
        .with_n_threads_batch(threads);
    let mut ctx = model
        .new_context(backend, params)
        .map_err(|e| format!("could not create context: {e}"))?;

    let vocab = model.vocab();
    let tokens = vocab.tokenize(prompt.as_bytes(), true, true);
    if tokens.is_empty() {
        return Err("prompt is empty".into());
    }
    if tokens.len() as u32 + max_tokens > n_ctx {
        return Err(format!(
            "prompt is {} tokens; with {max_tokens} for the reply that exceeds the {n_ctx} token window",
            tokens.len()
        ));
    }

    // Feed the prompt in batches.
    let mut batch = LlamaBatch::new(BATCH as usize, 1);
    let last = tokens.len() - 1;
    for (i, chunk) in tokens.chunks(BATCH as usize).enumerate() {
        batch.clear();
        for (j, &token) in chunk.iter().enumerate() {
            let pos = i * BATCH as usize + j;
            batch
                .add(token, pos as i32, &[0], pos == last)
                .map_err(|e| e.to_string())?;
        }
        ctx.decode(&mut batch).map_err(|e| e.to_string())?;
    }

    // A grammar, when given, goes first in the chain so it can veto
    // tokens before sampling; lazy grammars sleep until a trigger word.
    let mut chain = Vec::new();
    if let Some(g) = grammar {
        let sampler = if g.triggers.is_empty() {
            LlamaSampler::grammar(model, &g.text, &g.root)
        } else {
            LlamaSampler::grammar_lazy(
                model,
                &g.text,
                &g.root,
                g.triggers.iter().map(|t| t.as_bytes()),
                &[],
            )
        }
        .map_err(|e| format!("bad grammar: {e}"))?;
        chain.push(sampler);
    }
    chain.push(LlamaSampler::temp(0.7));
    chain.push(LlamaSampler::top_p(0.9, 1));
    chain.push(LlamaSampler::dist(1234));
    let mut sampler = LlamaSampler::chain_simple(chain);

    let mut pos = tokens.len() as i32;
    let mut produced = 0u32;
    let mut pending: Vec<u8> = Vec::new();
    while produced < max_tokens && !cancel.load(Ordering::Relaxed) {
        let token = sampler.sample(&ctx, batch.n_tokens() - 1);
        sampler.accept(token);
        if vocab.is_eog(token) {
            break;
        }
        pending.extend(vocab.token_to_piece(token, false, None));
        // Emit whatever is valid UTF-8 so far; keep a split multi-byte
        // character for the next token.
        let valid = match std::str::from_utf8(&pending) {
            Ok(_) => pending.len(),
            Err(e) => e.valid_up_to(),
        };
        if valid > 0 {
            let text = String::from_utf8_lossy(&pending[..valid]).into_owned();
            debug_log(&format!("OUT: {text:?}"));
            emit(out, &Event::Text { text });
            pending.drain(..valid);
        }
        produced += 1;

        batch.clear();
        batch
            .add(token, pos, &[0], true)
            .map_err(|e| e.to_string())?;
        pos += 1;
        ctx.decode(&mut batch).map_err(|e| e.to_string())?;
    }

    let seconds = started.elapsed().as_secs_f32();
    tracing::info!(
        tokens = produced,
        seconds,
        tokens_per_second = produced as f32 / seconds.max(0.001),
        "generation finished"
    );
    emit(
        out,
        &Event::Done {
            tokens: produced,
            seconds,
        },
    );
    Ok(())
}

/// Appends a line to `$BELVEDERE_MODEL_DEBUG` when that variable names a
/// file. For chasing down what the model actually saw and said.
fn debug_log(line: &str) {
    if let Some(path) = std::env::var_os("BELVEDERE_MODEL_DEBUG") {
        use std::io::Write as _;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let _ = writeln!(f, "{line}");
        }
    }
}

/// Nice 10 and the idle I/O class for this process. Done before any thread
/// exists, so every compute thread llama.cpp spawns inherits it. The
/// desktop keeps priority.
fn lower_priority() {
    unsafe {
        if libc::setpriority(libc::PRIO_PROCESS, 0, 10) != 0 {
            eprintln!("belvedere-model: could not lower CPU priority");
        }
        const IOPRIO_WHO_PROCESS: libc::c_int = 1;
        const IOPRIO_CLASS_IDLE: libc::c_int = 3;
        const IOPRIO_CLASS_SHIFT: libc::c_int = 13;
        let prio = IOPRIO_CLASS_IDLE << IOPRIO_CLASS_SHIFT;
        if libc::syscall(libc::SYS_ioprio_set, IOPRIO_WHO_PROCESS, 0, prio) != 0 {
            eprintln!("belvedere-model: could not set idle I/O priority");
        }
    }
}

/// Logs go to stderr (the service forwards them to its own log). llama.cpp's
/// own chatter is routed through tracing and kept to warnings.
fn init_logging() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,llama_cpp_2=warn,llama_cpp_sys_2=warn"));
    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_target(false).with_writer(std::io::stderr))
        .init();
    llama_cpp_2::send_logs_to_tracing(llama_cpp_2::LogOptions::default());
}
