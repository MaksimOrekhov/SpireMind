use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sts2_core::{MonsterKind, PowerKind, run_manager::RunManager};
use sts2_e2e::{
    build::Scope,
    env::{ApplyKind, Driver, Next, new_run_at},
    net::{E2eNet, PolicyRow, argmax},
};
use sts2_wire::emit;

use super::{
    engine, observation,
    powers::{self, Owner},
    sim_observation,
};
use crate::{Failure, Result, contract::Context, model, simulation::FIRST_SEED};

pub fn power_matrix() -> Value {
    let ironclad = sts2_core::powers::ironclad::ironclad_power_kinds();
    let rows = PowerKind::all_for(model::BUILD)
        .into_iter()
        .collect::<BTreeSet<_>>()
        .iter()
        .map(|&kind| {
            let spawn_sources = |enemy: bool| {
                MonsterKind::ALL_KNOWN
                    .iter()
                    .filter(|m| !MonsterKind::V0113_ONLY.contains(m))
                    .filter(|m| {
                        let grants = if enemy {
                            m.on_spawn_self_buffs()
                        } else {
                            m.on_spawn_opponent_debuffs()
                        };
                        grants.iter().any(|(power, _)| *power == kind)
                    })
                    .map(|m| m.python_name())
                    .collect::<Vec<_>>()
            };
            let profile = |owner| {
                let (class, basis) = powers::requirement(kind, owner);
                json!({"classification":class,"basis":basis})
            };
            json!({"id":kind.python_name(),"ironclad_owned":ironclad.contains(&kind),
            "enemy_spawn_sources":spawn_sources(true),"player_spawn_sources":spawn_sources(false),
            "player":profile(Owner::Player),"enemy":profile(Owner::Enemy)})
        })
        .collect::<Vec<_>>();
    json!({"upstream_revision":model::REVISION,"engine_build":model::BUILD.as_str(),
        "scope":"pinned game build; ownership and spawn sources are lower bounds for Ironclad reachability",
        "powers":rows})
}

#[derive(Default)]
struct Report {
    seen: BTreeSet<String>,
    duplicates: usize,
    outcomes: BTreeMap<&'static str, usize>,
    blocked: BTreeMap<String, usize>,
    blocker_details: BTreeMap<String, usize>,
    powers: BTreeMap<String, BTreeMap<&'static str, usize>>,
    encounters: BTreeSet<String>,
    cards: BTreeSet<String>,
    potions: BTreeSet<String>,
    relics: BTreeSet<String>,
    intent_kinds: BTreeSet<String>,
    runs: Vec<Value>,
    multiple_enemies: usize,
    combined_powers: usize,
    modified_cards: usize,
    historical_power_gate_rejects: usize,
    recovered_after_historical_power_gate: usize,
    checks: BTreeMap<&'static str, usize>,
    max_logit_abs_delta: f32,
    token_diff: BTreeMap<String, usize>,
}

fn rows(value: &Value) -> &[Value] {
    value.as_array().map_or(&[], Vec::as_slice)
}

fn list_powers(board: &Value) -> impl Iterator<Item = (&Value, Owner)> {
    rows(&board["player"]["powers"])
        .iter()
        .map(|p| (p, Owner::Player))
        .chain(
            rows(&board["enemies"])
                .iter()
                .flat_map(|e| rows(&e["powers"]).iter().map(|p| (p, Owner::Enemy))),
        )
}

fn mismatch(code: &'static str, detail: &str) -> Failure {
    Failure::new(code, detail)
}

fn different_token_fields(a: &PolicyRow, b: &PolicyRow) -> BTreeSet<String> {
    use sts2_e2e::schema::{N_CAT, N_CONT, cat_name, cont_name, section_name};
    let mut fields = BTreeSet::new();
    if a.sec != b.sec || a.tok_flags != b.tok_flags {
        fields.insert("token sections or flags".to_owned());
    }
    if a.pool_vid != b.pool_vid || a.pool_w != b.pool_w || a.pool_tok != b.pool_tok {
        fields.insert("pooled tokens".to_owned());
    }
    if a.cls_tok != b.cls_tok || a.cls_kind != b.cls_kind {
        fields.insert("class tokens".to_owned());
    }
    if a.sec != b.sec {
        return fields;
    }
    for (i, &section) in a.sec.iter().enumerate() {
        let section = section_name(section);
        for j in 0..N_CAT {
            if a.cat[i * N_CAT + j] != b.cat[i * N_CAT + j] {
                fields.insert(format!("{section}.{}", cat_name(j)));
            }
        }
        for j in 0..N_CONT {
            if a.cont[i * N_CONT + j] != b.cont[i * N_CONT + j] {
                fields.insert(format!("{section}.{}", cont_name(j)));
            }
        }
    }
    fields
}

fn comparable_board(mut raw: Value) -> Value {
    if let Some(pile) = raw["draw_pile"]["cards"].as_array_mut() {
        pile.sort_by_cached_key(Value::to_string);
    }
    // Position-local handles do not change the semantics or target indices.
    for enemy in raw["enemies"].as_array_mut().expect("projected enemies") {
        enemy
            .as_object_mut()
            .expect("projected enemy")
            .remove("combat_id");
    }
    raw
}

fn compare(
    raw: &Value,
    oracle: &RunManager,
    original_actions: &Value,
    net: Option<&E2eNet>,
    report: &mut Report,
) -> Result<&'static str> {
    let obs = observation::sanitize_live(raw, Context::Combat)?;
    let mut candidate = engine::reconstruct(&obs)?;
    let (actions, _) = engine::checked_actions(&obs, &candidate)?;
    if &actions != original_actions {
        return Err(mismatch(
            "sim_action_parity",
            "Full simulator action list differs",
        ));
    }
    *report
        .checks
        .entry("exact_legal_actions_and_targets")
        .or_default() += 1;
    let mut oracle = oracle.clone();
    let original_combat = oracle.combat.as_mut().expect("simulator combat");
    // The game's run envelope reads the current Creature HP even during
    // combat. The simulator's run-side HP is updated at combat exit.
    oracle.run_state.current_hp = original_combat.player.current_hp;
    oracle.run_state.max_hp = original_combat.player.max_hp;
    *original_combat = sts2_e2e::snapshot::belief_view(original_combat);
    let base = sts2_e2e::snapshot::state_dict(original_combat)
        .map_err(|_| mismatch("sim_projection", "Cannot export reference combat"))?;
    let rebuilt =
        sts2_e2e::snapshot::state_dict(candidate.combat.as_ref().expect("checked combat"))
            .map_err(|_| mismatch("sim_projection", "Cannot export reconstructed combat"))?;
    let power_signature = |wire: &Value| {
        let collect = |holder: &Value| {
            rows(&holder["powers"])
                .iter()
                .map(|p| json!([p["name"], p["amount"], p["counter"]]))
                .collect::<Vec<_>>()
        };
        let mut powers = vec![collect(&wire["player"])];
        powers.extend(rows(&wire["encounter"]).iter().map(collect));
        powers
    };
    if power_signature(&base) != power_signature(&rebuilt) {
        return Err(mismatch(
            "sim_power_parity",
            "Power identity, amount or reconstructed counter differs",
        ));
    }
    *report
        .checks
        .entry("exact_power_amounts_and_counters")
        .or_default() += 1;
    if rows(&actions).len() == 1 {
        return Ok("forced");
    }
    let reconstructed = engine::checked_decision(&obs, &mut candidate, &actions)?;
    let Next::Decision(reference) = Driver::new(0).peek(&mut oracle) else {
        return Err(mismatch(
            "sim_decision_parity",
            "Reference did not expose a policy decision",
        ));
    };
    if reconstructed.members != reference.members || reconstructed.actions != reference.actions {
        return Err(mismatch(
            "sim_decision_parity",
            "Reference policy action groups differ",
        ));
    }
    *report
        .checks
        .entry("exact_policy_action_groups")
        .or_default() += 1;
    let Some(net) = net else {
        return Ok("structural_ok");
    };
    let expected = model::observed_combat_row(net, &reference, &oracle)?;
    let actual = model::observed_combat_row(net, &reconstructed, &candidate)?;
    // Compare the arrays forwarded to the policy, including public previews.
    // The native encoder already uses a belief view and both sides mask
    // future enemy intents in model::policy_tokens.
    let same_tokens = expected == actual;
    if same_tokens {
        *report.checks.entry("exact_masked_tokens").or_default() += 1;
    } else {
        *report.checks.entry("different_masked_tokens").or_default() += 1;
        for field in different_token_fields(&expected, &actual) {
            *report.token_diff.entry(field).or_default() += 1;
        }
    }
    let expected = net
        .try_logits(&expected)
        .map_err(|e| mismatch("inference", &e))?;
    let actual = net
        .try_logits(&actual)
        .map_err(|e| mismatch("inference", &e))?;
    model::validate_logits(reference.members.len(), &expected)?;
    model::validate_logits(reconstructed.members.len(), &actual)?;
    let delta = expected
        .iter()
        .zip(&actual)
        .map(|(a, b)| (a - b).abs())
        .fold(0.0_f32, f32::max);
    report.max_logit_abs_delta = report.max_logit_abs_delta.max(delta);
    if argmax(&expected) == argmax(&actual) {
        *report.checks.entry("same_top_policy_class").or_default() += 1;
    }
    if delta <= 1e-5 {
        *report.checks.entry("logits_within_1e_5").or_default() += 1;
    }
    if !same_tokens {
        return Err(mismatch("sim_token_parity", "Masked policy tokens differ"));
    }
    if delta > 1e-5 {
        return Err(mismatch("sim_logit_parity", "Native policy logits differ"));
    }
    Ok("ok")
}

impl Report {
    fn evaluate(&mut self, rm: &RunManager, actions: &Value, net: Option<&E2eNet>) {
        let raw = match sim_observation::combat(rm, actions) {
            Ok(raw) => raw,
            Err(err) => {
                // A projector error is a separate quality signal, with no
                // public fingerprint available for deduplication.
                *self
                    .blocked
                    .entry(format!("{}: {}", err.code, err.message))
                    .or_default() += 1;
                *self.outcomes.entry("projection_error").or_default() += 1;
                return;
            }
        };
        let public = comparable_board(raw.clone());
        if !self
            .seen
            .insert(format!("{:x}", Sha256::digest(public.to_string())))
        {
            self.duplicates += 1;
            return;
        }
        let ids = list_powers(&raw)
            .filter_map(|(p, _)| p["id"].as_str())
            .collect::<BTreeSet<_>>();
        self.combined_powers += usize::from(ids.len() >= 2);
        let historical_gate_rejects = ids
            .iter()
            .any(|id| !powers::registry_id(id).is_some_and(powers::legacy));
        self.historical_power_gate_rejects += usize::from(historical_gate_rejects);
        let enemies = rows(&raw["enemies"]);
        self.multiple_enemies +=
            usize::from(enemies.iter().filter(|e| e["is_alive"] == true).count() >= 2);
        self.intent_kinds.extend(
            enemies
                .iter()
                .flat_map(|e| rows(&e["intents"]))
                .filter_map(|i| i["intent_type"].as_str())
                .map(str::to_owned),
        );
        if let Some(id) = raw["encounter_id"].as_str() {
            self.encounters.insert(id.to_owned());
        }
        self.cards.extend(
            rows(&raw["hand"])
                .iter()
                .filter_map(|c| c["id"].as_str())
                .map(str::to_owned),
        );
        self.modified_cards += usize::from(rows(&raw["hand"]).iter().any(|c| {
            c["upgrade_level"] != 0
                || !c["enchantment"].is_null()
                || !c["affliction"].is_null()
                || c["base_cost"] != c["effective_cost"]
        }));
        self.potions.extend(
            rows(&raw["potions"])
                .iter()
                .filter_map(|c| c["id"].as_str())
                .map(str::to_owned),
        );
        self.relics.extend(
            rows(&raw["run_state"]["relics"])
                .iter()
                .filter_map(|c| c["id"].as_str())
                .map(str::to_owned),
        );
        if self.seen.len() <= 8 {
            let mut changed = rm.clone();
            let cs = changed.combat.as_mut().expect("simulator combat");
            sts2_core::blindfold::blindfold_root(cs, 202_607);
            let legal = emit::legal_actions(cs);
            let same = legal == *actions
                && sim_observation::combat(&changed, &legal)
                    .is_ok_and(|board| comparable_board(board) == public);
            let label = if same {
                "same_public_projection_after_redeal"
            } else {
                "different_public_projection_after_redeal"
            };
            *self.checks.entry(label).or_default() += 1;
        }
        let outcome = match compare(&raw, rm, actions, net, self) {
            Ok(outcome) => outcome,
            Err(err) => {
                let diagnostic = super::diagnostics::record(
                    &raw,
                    super::session::Position {
                        session: 0,
                        state_id: 1,
                        request_id: 1,
                    },
                    "sim_comparison",
                    err.code,
                    None,
                );
                let shapes = rows(&diagnostic["blockers"])
                    .iter()
                    .map(|row| {
                        let id = row["id"].as_str().unwrap_or("-");
                        let path = row["path"]
                            .as_str()
                            .unwrap_or("$")
                            .split('[')
                            .map(|part| part.split_once(']').map_or(part, |(_, rest)| rest))
                            .collect::<String>();
                        format!(
                            "{path} | {id} | {}",
                            row["reason"].as_str().unwrap_or("unknown")
                        )
                    })
                    .collect::<BTreeSet<_>>();
                for shape in shapes {
                    *self.blocker_details.entry(shape).or_default() += 1;
                }
                *self
                    .blocked
                    .entry(format!("{}: {}", err.code, err.message))
                    .or_default() += 1;
                "blocked"
            }
        };
        self.recovered_after_historical_power_gate +=
            usize::from(historical_gate_rejects && outcome != "blocked");
        *self.outcomes.entry(outcome).or_default() += 1;
        for id in ids {
            *self
                .powers
                .entry(id.to_owned())
                .or_default()
                .entry(outcome)
                .or_default() += 1;
        }
    }
}

// A fixed choices schedule makes the unweighted and weighted suites use the
// same upstream positions. The adapter's predictions never select samples.
fn choose(d: &sts2_e2e::env::Decision, step: usize, seed: i64) -> usize {
    let index = step.wrapping_mul(1_103_515_245).wrapping_add(seed as usize);
    let preferred = d
        .actions
        .iter()
        .enumerate()
        .filter(|(_, a)| {
            if d.is_combat() {
                a["action"] == "play" || (index.is_multiple_of(7) && a["action"] == "potion")
            } else {
                !a["idx"].is_null() || matches!(a["action"].as_str(), Some("move" | "neow_choice"))
            }
        })
        .map(|(i, _)| i)
        .collect::<Vec<_>>();
    if preferred.is_empty() {
        index % d.actions.len()
    } else {
        preferred[index % preferred.len()]
    }
}

fn walk(
    report: &mut Report,
    mut rm: RunManager,
    net: Option<&E2eNet>,
    seed: i64,
    steps: usize,
    stop_on_combat_end: bool,
) -> Result<()> {
    let mut driver = Driver::new(seed);
    let mut ending = json!({"seed":seed,"status":"step_limit"});
    for step in 0..steps {
        let next = driver.peek(&mut rm);
        let combat_decision = match &next {
            Next::Decision(d) => Context::from_dtype(d.dtype).ok() == Some(Context::Combat),
            Next::Forced(f) => matches!(f.kind, ApplyKind::Combat),
            Next::Terminal(_) => false,
        };
        if let Some(cs) = &rm.combat
            && combat_decision
            && emit::pending_choice(cs).is_none()
        {
            let actions = emit::legal_actions(cs);
            report.evaluate(&rm, &actions, net);
        }
        let (action, kind) = match next {
            Next::Decision(d) => d
                .member_action(choose(&d, step, seed))
                .map_err(|e| mismatch("sim_generation", &e))?,
            Next::Forced(f) => (f.action, f.kind),
            Next::Terminal(t) => {
                ending = json!({"seed":seed,"status":t.outcome,"error":t.error,"cap":t.cap});
                break;
            }
        };
        driver
            .apply(&mut rm, &action, kind)
            .map_err(|e| mismatch("sim_generation", &e))?;
        if stop_on_combat_end && rm.combat.as_ref().is_none_or(|cs| cs.is_over) {
            ending = json!({"seed":seed,"status":"completed_combat"});
            break;
        }
    }
    report.runs.push(ending);
    Ok(())
}

fn summarize(report: Report, weighted: bool) -> Value {
    let mut blockers = report.blocked.into_iter().collect::<Vec<_>>();
    blockers.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let mut blocker_details = report.blocker_details.into_iter().collect::<Vec<_>>();
    blocker_details.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let covered = ["ok", "forced", "structural_ok"]
        .iter()
        .map(|key| report.outcomes.get(key).copied().unwrap_or_default())
        .sum::<usize>();
    json!({"source":"simulator_derived","live_validate_count":0,"synthetic_fixture_count":0,
        "weighted":weighted,
        "unique":report.seen.len(),"duplicate":report.duplicates,"projection_errors_deduplicated":false,
        "outcomes":report.outcomes,"coverage": if report.seen.is_empty(){None}else{Some(covered as f64/report.seen.len() as f64)},
        "ranked_blockers":blockers,"ranked_blocker_shapes":blocker_details,"powers":report.powers,"relics":report.relics,"potions":report.potions,"cards":report.cards,
        "encounters":report.encounters,"intent_kinds":report.intent_kinds,"multiple_enemies":report.multiple_enemies,"combined_powers":report.combined_powers,
        "modified_cards":report.modified_cards,"historical_power_gate_rejects":report.historical_power_gate_rejects,
        "recovered_after_historical_power_gate":report.recovered_after_historical_power_gate,
        "parity_checks":report.checks,"different_policy_fields":report.token_diff,
        "max_logit_abs_delta":weighted.then_some(report.max_logit_abs_delta),"runs":report.runs})
}

fn targeted_run(encounter: &str, seed: i64) -> Result<RunManager> {
    use sts2_core::{map::MapPointType, runstate::RoomType};
    let mut rm = new_run_at(seed, 0);
    let mut driver = Driver::new(seed);
    // A real map entry supplies a visible graph and visited coordinate.
    for step in 0..25 {
        if rm.combat.is_some() {
            break;
        }
        let (action, kind) = match driver.peek(&mut rm) {
            Next::Decision(d) => d
                .member_action(choose(&d, step, seed))
                .map_err(|e| mismatch("sim_generation", &e))?,
            Next::Forced(f) => (f.action, f.kind),
            Next::Terminal(_) => break,
        };
        driver
            .apply(&mut rm, &action, kind)
            .map_err(|e| mismatch("sim_generation", &e))?;
    }
    if rm.combat.is_none() {
        return Err(mismatch(
            "sim_generation",
            "Targeted fixture could not enter its map",
        ));
    }
    let deck = [
        "JUGGLING_CARD+1",
        "CRUELTY_CARD",
        "INFLAME",
        "STRIKE_IRONCLAD",
        "STRIKE_IRONCLAD",
        "STRIKE_IRONCLAD",
        "DEFEND_IRONCLAD+1",
        "BASH+1",
        "BATTLE_TRANCE",
        "RUPTURE_CARD",
    ]
    .map(str::to_owned);
    let relics = ["BURNING_BLOOD", "BRONZE_SCALES", "VAJRA"].map(str::to_owned);
    let belt = ["BLOCK_POTION", "FLEX_POTION"].map(str::to_owned);
    let mut cs = sts2_wire::scenario::build_scenario_state_for_version(
        model::BUILD,
        seed as u64,
        "Ironclad",
        &deck,
        80,
        80,
        &relics,
        &belt,
        encounter,
        0,
        None,
        None,
    )
    .map_err(|e| mismatch("sim_generation", &e))?;
    rm.debug_set_master_deck(cs.cards.to_vec());
    rm.debug_set_player(Some(80), Some(80), None);
    rm.run_state.relics = cs
        .relics
        .iter()
        .map(|relic| {
            sts2_core::RelicId::all()
                .iter()
                .copied()
                .find(|id| id.to_relic_kind() == Some(relic.kind))
                .ok_or_else(|| mismatch("sim_generation", "Scenario relic has no run identity"))
        })
        .collect::<Result<Vec<_>>>()?;
    rm.run_state.potions = cs.player.potions.to_vec();
    let tier = sts2_content::encounters::find(encounter)
        .ok_or_else(|| mismatch("sim_generation", "Unknown configured encounter"))?
        .tier;
    cs.is_elite = matches!(tier, sts2_content::encounters::EncounterTier::Elite);
    cs.is_boss = matches!(tier, sts2_content::encounters::EncounterTier::Boss);
    if cs.is_elite {
        let graph = rm
            .run_state
            .current_act_map
            .as_ref()
            .ok_or_else(|| mismatch("sim_generation", "Scenario map is absent"))?;
        let start = graph
            .start
            .ok_or_else(|| mismatch("sim_generation", "Scenario map entrance is absent"))?;
        let mut queue = std::collections::VecDeque::from([vec![start]]);
        let mut seen = BTreeSet::new();
        let path = loop {
            let path = queue
                .pop_front()
                .ok_or_else(|| mismatch("sim_generation", "Scenario map has no reachable elite"))?;
            let idx = *path.last().expect("nonempty graph path");
            if graph.point(idx).point_type == MapPointType::Elite {
                break path
                    .iter()
                    .skip(1)
                    .map(|&i| graph.point(i).coord)
                    .collect::<Vec<_>>();
            }
            for &child in graph.children_idx_of(idx) {
                if seen.insert(child) {
                    let mut next = path.clone();
                    next.push(child);
                    queue.push_back(next);
                }
            }
        };
        let at = *path.last().expect("elite is not the entrance");
        rm.debug_place_on_map(1, at, path, at.row + 1, at.row)
            .map_err(|e| mismatch("sim_generation", &e))?;
    }
    let room = if cs.is_elite {
        RoomType::Elite
    } else if cs.is_boss {
        RoomType::Boss
    } else {
        RoomType::Monster
    };
    rm.debug_set_room_type(room);
    sts2_wire::graft::set_combat_encounter(&mut rm, encounter)
        .map_err(|e| mismatch("sim_generation", &e.to_string()))?;
    rm.debug_enter_combat(cs, room);
    Ok(rm)
}

pub fn run(net: Option<&E2eNet>, runs: u8, steps: usize) -> Result<Value> {
    let _scope = Scope::enter(model::BUILD);
    let mut campaign = Report::default();
    for seed in FIRST_SEED..FIRST_SEED + i64::from(runs) {
        walk(&mut campaign, new_run_at(seed, 0), net, seed, steps, false)?;
    }
    let mut targeted = BTreeMap::new();
    for (i, encounter) in ["NIBBITS_WEAK", "BYGONE_EFFIGY_ELITE", "CORPSE_SLUGS_NORMAL"]
        .into_iter()
        .enumerate()
    {
        let mut report = Report::default();
        let seed = FIRST_SEED + i as i64;
        walk(
            &mut report,
            targeted_run(encounter, seed)?,
            net,
            seed,
            40,
            true,
        )?;
        targeted.insert(encounter, summarize(report, net.is_some()));
    }
    Ok(
        json!({"schema_version":1,"upstream_revision":model::REVISION,"engine_build":model::BUILD.as_str(),
        "model":net.map(|_|model::pin()),"campaign":summarize(campaign,net.is_some()),"configured_fights":targeted}),
    )
}
