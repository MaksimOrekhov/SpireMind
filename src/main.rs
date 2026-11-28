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
#[command(version, about = "Local, read-only Instinct advisor for Ironclad")]
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
        /// Apply live profile v2 to native simulator combat decisions.
        #[arg(long)]
        combat_v2: bool,
    },
    /// Compare simulator-derived public combat positions and original Instinct.
    Compatibility {
        #[arg(long, default_value_t = 8, value_parser = clap::value_parser!(u8).range(1..=32))]
        runs: u8,
        #[arg(long, default_value_t = 180, value_parser = clap::value_parser!(u32).range(10..=1500))]
        steps_per_run: u32,
        /// Compare reconstructions without loading model weights.
        #[arg(long)]
        structural_only: bool,
    },
    /// Independently sample policy-guided Ironclad A0 campaigns.
    HeldOut {
        #[arg(long, default_value_t = 4, value_parser = clap::value_parser!(u8).range(1..=16))]
        runs: u8,
        #[arg(long, default_value_t = 1500, value_parser = clap::value_parser!(u32).range(10..=4000))]
        steps_per_run: u32,
        /// Use explicit new seeds after adapter development is complete.
        #[arg(long, value_parser = clap::value_parser!(i64).range(0..i64::MAX - 16))]
        first_seed: Option<i64>,
    },
    /// Compare the three combat advisor architectures offline on a frozen cohort.
    Architecture {
        #[arg(value_enum)]
        panel: live::compatibility::ArchitecturePanel,
        /// First of eight new seeds, required for the validation panel.
        #[arg(long, value_parser = clap::value_parser!(i64).range(0..simulation::SEED_LIMIT - 8))]
        first_seed: Option<i64>,
    },
    /// List observation compatibility for the pinned upstream power registry.
    PowerMatrix,
    /// Compare both possible Ruined Helmet latch values in bounded offline fixtures.
    UncertaintyPoc,
    /// Show read-only recommendations from Spire Mentor's local Advisor lane.
    Live {
        #[arg(long, default_value_t = 9002, value_parser = clap::value_parser!(u16).range(1..))]
        port: u16,
        #[arg(long, default_value_t = 1500, value_parser = clap::value_parser!(u64).range(1000..=5000))]
        ttl_ms: u64,
        /// Verify the installed game's version and commit before connecting.
        #[arg(long)]
        release_info: PathBuf,
        /// Save sanitized observations in this local directory.
        #[arg(long)]
        capture_dir: Option<PathBuf>,
        /// Write a bounded, redacted blocker corpus to a new directory.
        #[arg(long)]
        blocked_corpus: Option<PathBuf>,
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
    if matches!(cli.command, Mode::PowerMatrix) {
        return Ok((live::compatibility::power_matrix(), true));
    }
    if let Mode::Compatibility {
        runs,
        steps_per_run,
        structural_only: true,
    } = cli.command
    {
        model::check_build(cli.game_version.as_deref().unwrap_or(BUILD.as_str()))?;
        return Ok((
            live::compatibility::run(None, runs, steps_per_run as usize)?,
            true,
        ));
    }
    if matches!(
        cli.command,
        Mode::Recommend { .. }
            | Mode::Benchmark { .. }
            | Mode::Compatibility { .. }
            | Mode::HeldOut { .. }
            | Mode::Architecture { .. }
            | Mode::UncertaintyPoc
    ) {
        model::check_build(cli.game_version.as_deref().unwrap_or(BUILD.as_str()))?;
    }
    let path = cli.weights.unwrap_or_else(model::default_weights);
    let net = model::load(&path)?;
    match cli.command {
        Mode::Architecture { panel, first_seed } => Ok((
            live::compatibility::architecture(&net, &path, panel, first_seed)?,
            true,
        )),
        Mode::UncertaintyPoc => Ok((live::compatibility::uncertainty_poc(&net)?, true)),
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
            combat_v2,
        } => Ok(simulation::benchmark(
            &net,
            runs,
            timeout_seconds,
            combat_v2,
        )),
        Mode::Compatibility {
            runs,
            steps_per_run,
            ..
        } => Ok((
            live::compatibility::run(Some(&net), runs, steps_per_run as usize)?,
            true,
        )),
        Mode::HeldOut {
            runs,
            steps_per_run,
            first_seed,
        } => Ok((
            live::compatibility::held_out(&net, runs, steps_per_run as usize, first_seed)?,
            true,
        )),
        Mode::Live {
            port,
            ttl_ms,
            release_info,
            capture_dir,
            blocked_corpus,
            json,
        } => {
            let installed = live::release_build(&release_info)?;
            if let Some(requested) = cli.game_version.as_deref() {
                live::check_version(requested)?;
                if installed != requested {
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
                blocked_corpus,
                &installed,
            )?;
            Ok((json!({"status":"stopped"}), true))
        }
        Mode::Replay { captures } => live::replay(&net, &captures),
        Mode::Sanitize { .. } | Mode::PowerMatrix => unreachable!(),
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
