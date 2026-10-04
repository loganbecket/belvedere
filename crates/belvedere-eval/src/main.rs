//! `belvedere-eval --suite chat-tasks`: scores the model against the
//! cases in `evals/chat-tasks/cases.json` and writes the results to
//! `evals/results/chat-tasks/`.

mod agent_runner;
mod cases;
mod report;
mod runner;
mod score;

use std::path::PathBuf;

use belvedere_core::models::{self, Locations};

struct Args {
    suite: String,
    /// A model name (as `ListModels` shows it) or a path to a GGUF file.
    model: Option<String>,
    cpu: bool,
    /// Run only cases whose id contains this.
    only: Option<String>,
    evals_dir: PathBuf,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        suite: String::new(),
        model: None,
        cpu: false,
        only: None,
        evals_dir: PathBuf::from("evals"),
    };
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--suite" => args.suite = it.next().ok_or("--suite needs a name")?,
            "--model" => args.model = Some(it.next().ok_or("--model needs a name or path")?),
            "--only" => args.only = Some(it.next().ok_or("--only needs text")?),
            "--evals-dir" => {
                args.evals_dir = PathBuf::from(it.next().ok_or("--evals-dir needs a path")?)
            }
            "--cpu" => args.cpu = true,
            "--version" | "-V" => {
                println!("{}", belvedere_core::version_line("belvedere-eval"));
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    if args.suite.is_empty() {
        return Err(
            "usage: belvedere-eval --suite <name> [--model <name|path>] [--cpu] [--only <text>]"
                .into(),
        );
    }
    Ok(args)
}

/// Which model file to use: the one named, or the best on disk (the 9B
/// Qwen if present, else anything that can use tools, else anything).
fn choose_model(wanted: Option<&str>) -> Result<(PathBuf, String), String> {
    if let Some(w) = wanted {
        let p = PathBuf::from(w);
        if p.is_file() {
            let name = p
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            return Ok((p, name));
        }
    }
    let scan = models::scan(&Locations::standard());
    if scan.found.is_empty() {
        return Err("no model files found on this machine".into());
    }
    if let Some(w) = wanted {
        let lw = w.to_lowercase();
        return scan
            .found
            .iter()
            .find(|f| f.name.to_lowercase() == lw)
            .or_else(|| {
                scan.found
                    .iter()
                    .find(|f| f.name.to_lowercase().contains(&lw))
            })
            .map(|f| (f.path.clone(), f.name.clone()))
            .ok_or_else(|| {
                format!(
                    "no model named {w:?}; known: {}",
                    scan.found
                        .iter()
                        .map(|f| f.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            });
    }
    let lower = |f: &models::Found| f.name.to_lowercase();
    let pick = scan
        .found
        .iter()
        .find(|f| lower(f).contains("qwen3.5") && lower(f).contains("4b"))
        .or_else(|| scan.found.iter().find(|f| f.info.supports_tools()))
        .or_else(|| scan.found.first())
        .expect("non-empty");
    Ok((pick.path.clone(), pick.name.clone()))
}

#[tokio::main]
async fn main() {
    // Show the model helper's own log lines (it writes them to stderr and
    // the engine forwards them) so a crash there is not silent.
    {
        use tracing_subscriber::{fmt, prelude::*, EnvFilter};
        let filter = EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| EnvFilter::new("warn,belvedere_model=info"));
        tracing_subscriber::registry()
            .with(filter)
            .with(fmt::layer().with_target(false).with_writer(std::io::stderr))
            .init();
    }
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };

    let path = args.evals_dir.join(&args.suite).join("cases.json");
    let mut suite = match cases::Suite::load(&path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };
    if let Some(only) = &args.only {
        suite.cases.retain(|c| c.id.contains(only.as_str()));
    }
    println!("suite {}: {} case(s)", suite.name, suite.cases.len());

    let (model_path, model_name) = match choose_model(args.model.as_deref()) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };
    println!(
        "model: {model_name} ({})",
        if args.cpu { "CPU" } else { "GPU" }
    );
    let mut runner = match agent_runner::AgentRunner::new(model_path, model_name, !args.cpu).await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("could not load the model: {e}");
            std::process::exit(1);
        }
    };

    let results = report::run_suite(&suite, &mut runner, |v| {
        let mark = if v.pass { "PASS" } else { "FAIL" };
        println!("{mark} {:<10} {}", v.kind, v.id);
        for f in &v.failures {
            println!("       {f}");
        }
    })
    .await
    .with_cases(&suite);
    runner.shutdown().await;

    print!("\n{}", results.table());
    match results.write(&args.evals_dir.join("results")) {
        Ok(p) => println!("results: {}", p.display()),
        Err(e) => {
            eprintln!("could not write results: {e}");
            std::process::exit(1);
        }
    }
}
