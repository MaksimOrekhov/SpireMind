mod contract;
mod model;
mod simulation;
#[cfg(test)]
mod tests;

use std::{path::PathBuf, process::ExitCode};

use clap::{Parser, Subcommand};
use serde::Serialize;
use serde_json::{Value, json};

use contract::Context;
use model::{ASCENSION, BUILD};

#[derive(Debug, Serialize)]
struct Failure {
    code: &'static str,
    message: String,
}

impl Failure {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

type Result<T> = std::result::Result<T, Failure>;

#[derive(Parser)]
#[command(
    version,
    about = "Offline, read-only Instinct advisor for simulated Ironclad runs"
)]
struct Cli {
    #[arg(long, global = true)]
    weights: Option<PathBuf>,
    #[arg(long, global = true, default_value = BUILD.as_str())]
    game_version: String,
    #[command(subcommand)]
    command: Mode,
}

#[derive(Subcommand)]
enum Mode {
    /// Rank actions at the first matching simulator decision with a policy choice.
    Recommend {
        #[arg(value_enum)]
        context: Context,
        #[arg(long, default_value_t = simulation::FIRST_SEED, value_parser = clap::value_parser!(i64).range(0..simulation::SEED_LIMIT))]
        seed: i64,
        #[arg(long, default_value_t = 30, value_parser = clap::value_parser!(u64).range(1..=3600))]
        timeout_seconds: u64,
    },
    /// Run a bounded panel of simulated A10 runs with greedy Instinct.
    Benchmark {
        #[arg(long, default_value_t = 10, value_parser = clap::value_parser!(u8).range(1..=20))]
        runs: u8,
        #[arg(long, default_value_t = 120, value_parser = clap::value_parser!(u64).range(1..=3600))]
        timeout_seconds: u64,
    },
}

fn provenance() -> Value {
    json!({
        "source": "spire_mentor_simulator",
        "upstream_revision": model::REVISION,
        "game_version": BUILD.as_str(),
        "character": "Ironclad",
        "ascension": ASCENSION,
        "model": model::pin(),
        "observation": model::OBSERVATION,
    })
}

fn run(cli: Cli) -> Result<(Value, bool)> {
    model::check_build(&cli.game_version)?;
    let path = cli.weights.unwrap_or_else(model::default_weights);
    let net = model::load(&path)?;
    match cli.command {
        Mode::Recommend {
            context,
            seed,
            timeout_seconds,
        } => Ok((
            simulation::recommend(&net, context, seed, timeout_seconds)?,
            true,
        )),
        Mode::Benchmark {
            runs,
            timeout_seconds,
        } => Ok(simulation::benchmark(&net, runs, timeout_seconds)),
    }
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok((response, ok)) => {
            println!("{response}");
            if ok {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        Err(error) => {
            eprintln!(
                "{}",
                json!({"schema_version": 1, "status": "blocked", "error": error})
            );
            ExitCode::FAILURE
        }
    }
}
