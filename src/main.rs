mod contract;
mod live;
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
    #[arg(long, global = true)]
    game_version: Option<String>,
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
    /// Show read-only recommendations from Spire Mentor's local Advisor lane.
    Live {
        #[arg(long, default_value_t = 9002, value_parser = clap::value_parser!(u16).range(1..))]
        port: u16,
        #[arg(long, default_value_t = 1500, value_parser = clap::value_parser!(u64).range(250..=5000))]
        ttl_ms: u64,
        /// Expect the build from this installed game's release_info.json.
        #[arg(long)]
        release_info: Option<PathBuf>,
        /// Save sanitized observations in this local directory.
        #[arg(long)]
        capture_dir: Option<PathBuf>,
        /// Write JSON Lines events with explicit stale notices.
        #[arg(long)]
        json: bool,
    },
    /// Re-evaluate saved Advisor observations with the original model.
    Replay {
        #[arg(required = true, num_args = 1..=128)]
        captures: Vec<PathBuf>,
    },
    /// Sanitize the mod's copied observation for local replay.
    Sanitize { input: PathBuf, output: PathBuf },
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
    if let Mode::Sanitize { input, output } = &cli.command {
        return Ok((live::sanitize_file(input, output)?, true));
    }
    if matches!(cli.command, Mode::Recommend { .. } | Mode::Benchmark { .. }) {
        model::check_build(cli.game_version.as_deref().unwrap_or(BUILD.as_str()))?;
    }
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
        Mode::Live {
            port,
            ttl_ms,
            release_info,
            capture_dir,
            json,
        } => {
            let installed = release_info
                .as_deref()
                .map(live::release_build)
                .transpose()?;
            if let Some(requested) = cli.game_version.as_deref() {
                live::check_version(requested)?;
                if installed
                    .as_deref()
                    .is_some_and(|actual| actual != requested)
                {
                    return Err(Failure::new(
                        "producer_mismatch",
                        "Requested version differs from release_info.json",
                    ));
                }
            }
            live::run(
                net,
                port,
                ttl_ms,
                json,
                capture_dir,
                installed.as_deref().or(cli.game_version.as_deref()),
            )?;
            Ok((json!({"status":"stopped"}), true))
        }
        Mode::Replay { captures } => live::replay(&net, &captures),
        Mode::Sanitize { .. } => unreachable!(),
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
