use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{Value, json};
use sts2_e2e::{
    build::Scope,
    env::{E2eEnv, Step},
    net::{E2eNet, argmax},
    tokens::TokCache,
};

use crate::{
    Failure, Result,
    contract::{self, Context},
    model::{self, BUILD},
    provenance,
};

pub const FIRST_SEED: i64 = 160_000_000;
pub const SEED_LIMIT: i64 = 1 << 31;

pub fn recommend(net: &E2eNet, context: Context, seed: i64, timeout: u64) -> Result<Value> {
    let _scope = Scope::enter(BUILD);
    let deadline = Instant::now() + Duration::from_secs(timeout);
    let mut env = E2eEnv::new(seed);
    let mut cache = TokCache::new();
    let mut step = env.reset();
    loop {
        if Instant::now() >= deadline {
            return Err(Failure::new(
                "timeout",
                "Simulator decision walk exceeded its time budget",
            ));
        }
        let d = match step {
            Step::Decision(d) => d,
            Step::Terminal(t) => {
                return Err(Failure::new(
                    "state_unavailable",
                    format!(
                        "Simulation reached {} before finding {context:?}: {}",
                        t.outcome,
                        t.error
                            .or_else(|| t.cap.map(str::to_owned))
                            .unwrap_or_default()
                    ),
                ));
            }
        };
        let (logits, ms) = model::infer(net, &d, env.rm(), &mut cache)?;
        if Context::from_dtype(d.dtype).ok() == Some(context) {
            let legal = contract::legal(&d, env.rm(), context)?;
            let ranked = contract::rank(&d.classes, &d.members, &logits)?;
            return Ok(json!({
                "schema_version": 1,
                "status": "ok",
                "provenance": provenance(),
                "state": {
                    "id": format!("sim:{seed}:{}", d.row),
                    "simulation_seed": seed,
                    "decision_index": d.row,
                    "act": d.act,
                    "floor": d.floor,
                    "turn": d.turn(),
                    "context": context,
                },
                "legal_actions": legal,
                "recommendations": ranked,
                "latency_ms": {"forward": ms},
            }));
        }
        // Only the local simulator advances. The requested decision remains untouched.
        step = env.step(argmax(&logits));
    }
}

#[derive(Serialize)]
struct Run {
    seed: i64,
    outcome: &'static str,
    decisions: usize,
    detail: Option<String>,
}

#[derive(Default)]
struct Samples {
    all: Vec<f64>,
    combat: Vec<f64>,
    card_reward: Vec<f64>,
    map: Vec<f64>,
}

impl Samples {
    fn add(&mut self, dtype: u8, ms: f64) {
        self.all.push(ms);
        match Context::from_dtype(dtype) {
            Ok(Context::Combat) => self.combat.push(ms),
            Ok(Context::CardReward) => self.card_reward.push(ms),
            Ok(Context::Map) => self.map.push(ms),
            Err(_) => {}
        }
    }
}

fn measure(samples: &mut [f64]) -> Value {
    if samples.is_empty() {
        return json!({"count": 0});
    }
    samples.sort_by(f64::total_cmp);
    let percentile = |p: usize| samples[(p * samples.len()).div_ceil(100) - 1];
    json!({"count": samples.len(), "mean": samples.iter().sum::<f64>() / samples.len() as f64,
        "p50": percentile(50), "p95": percentile(95), "max": samples[samples.len() - 1]})
}

fn play(net: &E2eNet, seed: i64, deadline: Instant, samples: &mut Samples) -> Run {
    let mut env = E2eEnv::new(seed);
    let mut cache = TokCache::new();
    let mut step = env.reset();
    let before = samples.all.len();
    let (outcome, detail) = loop {
        if Instant::now() >= deadline {
            break ("timeout", None);
        }
        match step {
            Step::Terminal(t) => break (t.outcome, t.error.or_else(|| t.cap.map(str::to_owned))),
            Step::Decision(d) => match model::infer(net, &d, env.rm(), &mut cache) {
                Ok((logits, ms)) => {
                    samples.add(d.dtype, ms);
                    step = env.step(argmax(&logits));
                }
                Err(error) => break ("error", Some(format!("{}: {}", error.code, error.message))),
            },
        }
    };
    Run {
        seed,
        outcome,
        decisions: samples.all.len() - before,
        detail,
    }
}

pub fn benchmark(net: &E2eNet, runs: u8, timeout: u64) -> (Value, bool) {
    let _scope = Scope::enter(BUILD);
    let start = Instant::now();
    let deadline = start + Duration::from_secs(timeout);
    let mut samples = Samples::default();
    let mut results = Vec::new();
    for seed in FIRST_SEED..FIRST_SEED + i64::from(runs) {
        if Instant::now() >= deadline {
            break;
        }
        results.push(play(net, seed, deadline, &mut samples));
    }
    let count = |outcome| results.iter().filter(|r| r.outcome == outcome).count();
    let completed = count("won") + count("lost");
    let ok = completed == usize::from(runs);
    (
        json!({
            "schema_version": 1,
            "status": if ok { "ok" } else { "partial" },
            "provenance": provenance(),
            "requested": runs,
            "completed": completed,
            "wins": count("won"),
            "errors": count("error"),
            "capped": count("capped"),
            "timed_out": count("timeout"),
            "not_started": usize::from(runs) - results.len(),
            "timeout_seconds": timeout,
            "wall_seconds": start.elapsed().as_secs_f64(),
            "forward_latency_ms": {"all": measure(&mut samples.all), "combat": measure(&mut samples.combat),
                "card_reward": measure(&mut samples.card_reward), "map": measure(&mut samples.map)},
            "runs": results,
        }),
        ok,
    )
}
