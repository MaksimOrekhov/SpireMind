use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sts2_core::{MonsterKind, PowerKind, cards::VarKey, run_manager::RunManager};
use sts2_e2e::{
    build::Scope,
    env::{ApplyKind, Driver, Next, new_run_at},
    net::{E2eNet, PolicyRow, argmax},
    tokens::TokCache,
};
use sts2_wire::emit;

use super::{
    advisor_experiment::Experiment,
    engine, observation,
    powers::{self, Owner},
    sim_observation,
};
use crate::{Failure, Result, contract::Context, model, simulation::FIRST_SEED};

type Outcomes = BTreeMap<&'static str, usize>;
type Breakdown = BTreeMap<String, Outcomes>;

pub fn power_matrix() -> Value {
    let ironclad = sts2_core::powers::ironclad::ironclad_power_kinds();
    // The wire parser resolves the first kind with a given public ID.
    let mut seen_ids = BTreeSet::new();
    let rows = PowerKind::all_for(model::BUILD)
        .into_iter()
        .filter(|kind| seen_ids.insert(kind.python_name()))
        .map(|kind| {
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
    seen: BTreeMap<String, BTreeSet<String>>,
    semantic_positions: usize,
    public_collisions: usize,
    duplicates: usize,
    outcomes: Outcomes,
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
    positions_by_act: BTreeMap<u8, usize>,
    positions_by_room: BTreeMap<String, usize>,
    private_card_variables: BTreeMap<String, usize>,
    legacy_card_marker_positions: usize,
    reevaluated_legacy_card_blocks: BTreeMap<String, usize>,
    importer_warnings: BTreeMap<String, usize>,
    first_failure_by_encounter: BTreeMap<String, BTreeMap<String, usize>>,
    invariance_strata: BTreeSet<(u8, bool, bool, bool)>,
    outcomes_by_act: Breakdown,
    outcomes_by_room: Breakdown,
    outcomes_by_encounter: Breakdown,
    outcomes_by_relic: Breakdown,
    relic_counter_diffs: BTreeMap<String, usize>,
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

pub(super) fn different_token_fields(a: &PolicyRow, b: &PolicyRow) -> BTreeSet<String> {
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

fn oracle_signature(rm: &RunManager, actions: &Value) -> Result<String> {
    let combat = rm
        .combat
        .as_ref()
        .ok_or_else(|| mismatch("sim_projection", "No oracle combat"))?;
    let combat = sts2_e2e::snapshot::belief_view(combat);
    let wire =
        sts2_e2e::snapshot::state_dict(&combat).map_err(|e| mismatch("sim_projection", &e))?;
    // The snapshot plus upstream previews cover the policy-visible combat
    // distinctions. Scratch values are used only for the offline audit,
    // making ties conservative even if a previous encoder ignored a field.
    let snap = sts2_e2e::snapshot::honest_snapshot(&combat, sts2_e2e::env::fight_room(rm))
        .map_err(|e| mismatch("sim_projection", &e))?;
    let live = sts2_e2e::snapshot::live_snapshot(&snap, &combat, &wire);
    let scratch = ["hand", "draw_pile", "discard_pile", "exhaust_pile"].map(|pile| {
        let mut cards = rows(&wire["deck"][pile])
            .iter()
            .map(|c| json!([c["id"], c["combat_vars"], c["cost_mods"]]))
            .collect::<Vec<_>>();
        if pile == "draw_pile" {
            cards.sort_by_cached_key(Value::to_string);
        }
        cards
    });
    Ok(format!(
        "{:x}",
        Sha256::digest(json!([actions, live, scratch]).to_string())
    ))
}

fn record_position(report: &mut Report, public: String, semantic: String) -> bool {
    let signatures = report.seen.entry(public).or_default();
    let new_public = signatures.is_empty();
    if !signatures.insert(semantic) {
        report.duplicates += 1;
        return false;
    }
    report.semantic_positions += 1;
    report.public_collisions += usize::from(!new_public);
    true
}

// A partial PoC: build the policy class partition from the public dialect
// and an already independently verified action list. No combat graft is
// needed for this substage. A full row still needs current preview inputs.
fn direct_classes(
    obs: &observation::Observation,
    actions: &[Value],
    ascension: u8,
) -> Result<(Vec<usize>, Vec<Vec<usize>>)> {
    use sts2_e2e::contracts::{classes_from_keys, combat_class_keys};
    let (wire, warnings) = engine::combat_wire(&obs.board, ascension)
        .map_err(|_| mismatch("direct_classes", "Public card spec translation failed"))?;
    if !warnings.is_empty() {
        return Err(mismatch(
            "direct_classes",
            "Public card spec translation reported warnings",
        ));
    }
    let keys = combat_class_keys(
        actions,
        rows(&wire["deck"]["hand"]),
        rows(&wire["player"]["potions"]),
        None,
        &mut |_| unreachable!("combat turn actions contain no choices"),
        &wire["deck"],
    )
    .map_err(|_| {
        mismatch(
            "direct_classes",
            "Public class encoding refused the action list",
        )
    })?;
    Ok(classes_from_keys(&keys))
}

fn native_observed_row(net: &E2eNet, mut rm: RunManager) -> Result<Option<PolicyRow>> {
    let cs = rm.combat.as_ref().expect("simulator combat");
    rm.run_state.current_hp = cs.player.current_hp;
    rm.run_state.max_hp = cs.player.max_hp;
    let Next::Decision(d) = Driver::new(0).peek(&mut rm) else {
        return Ok(None);
    };
    model::observed_combat_row(net, &d, &rm).map(Some)
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
    for relic in rows(&obs.board["run_state"]["relics"]) {
        if let Some(id) =
            super::relics::id(relic).filter(|&id| super::relics::has_private_latch(id))
        {
            let counter = |wire: &Value| {
                rows(&wire["player"]["relics"])
                    .iter()
                    .find(|r| r["id"] == id.to_python_name())
                    .and_then(|r| r["counter"].as_i64())
                    .unwrap_or_default()
            };
            if counter(&base) != counter(&rebuilt) {
                return Err(mismatch(
                    "sim_relic_parity",
                    "Public relic witness differs from native combat",
                ));
            }
            *report
                .checks
                .entry("exact_public_relic_counters")
                .or_default() += 1;
        }
    }
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
    let direct = direct_classes(&obs, &reference.actions, oracle.ascension_level);
    let direct_label = match direct {
        Ok((classes, members)) if classes == reference.classes && members == reference.members => {
            "direct_public_class_match"
        }
        Ok(_) => "direct_public_class_mismatch",
        Err(_) => "direct_public_class_unavailable",
    };
    *report.checks.entry(direct_label).or_default() += 1;
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
        let fields = different_token_fields(&expected, &actual);
        if fields.contains("RELIC.R_CTR") {
            use sts2_e2e::snapshot::display_counter;
            for row in rows(&base["player"]["relics"]) {
                let Some(id) = row["id"].as_str() else {
                    continue;
                };
                let left = display_counter(id, row["counter"].as_i64().unwrap_or_default());
                let right = rows(&rebuilt["player"]["relics"])
                    .iter()
                    .find(|r| r["id"] == id)
                    .and_then(|r| display_counter(id, r["counter"].as_i64().unwrap_or_default()));
                if left != right {
                    *report.relic_counter_diffs.entry(id.to_owned()).or_default() += 1;
                }
            }
        }
        for field in fields {
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
    fn evaluate(
        &mut self,
        rm: &RunManager,
        actions: &Value,
        net: Option<&E2eNet>,
    ) -> Option<&'static str> {
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
                return Some("projection_error");
            }
        };
        let public = comparable_board(raw.clone());
        let semantic = match oracle_signature(rm, actions) {
            Ok(signature) => signature,
            Err(err) => {
                *self.outcomes.entry("projection_error").or_default() += 1;
                *self
                    .blocked
                    .entry(format!("{}: {}", err.code, err.message))
                    .or_default() += 1;
                return Some("projection_error");
            }
        };
        if !record_position(
            self,
            format!("{:x}", Sha256::digest(public.to_string())),
            semantic,
        ) {
            return None;
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
        *self
            .positions_by_act
            .entry(rm.run_state.act_index)
            .or_default() += 1;
        if let Some(room) = raw["room_type"].as_str() {
            *self.positions_by_room.entry(room.to_owned()).or_default() += 1;
        }
        let vars = sim_observation::card_variables(rm);
        let legacy_card_marker = !vars.is_empty();
        self.legacy_card_marker_positions += usize::from(legacy_card_marker);
        let mut unproven = false;
        let mut keys = BTreeSet::new();
        for (id, key, value, published) in vars {
            let name = sts2_wire::state_dict::var_key_wire_name(key);
            keys.insert(format!("{id} | {name}"));
            // Generation provenance is read by the thief and Fool handlers,
            // outside the admitted solo Ironclad board.
            let known = match key {
                VarKey::GeneratedInCombat => enemies
                    .iter()
                    .all(|e| observation::monster_kind(e) != Some(MonsterKind::ThievingHopper)),
                VarKey::ReturnIfPlayedRound => matches!(id, "BOLAS" | "THRUMMING_HATCHET"),
                // This lifespan is updated by RunManager at combat exit.
                VarKey::Generic => id == "GUILTY",
                _ => false,
            };
            unproven |= value != 0 && !known && !published;
        }
        for key in keys {
            *self.private_card_variables.entry(key).or_default() += 1;
        }
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
        if let Some(net) = net
            && self.invariance_strata.insert((
                rm.run_state.act_index,
                !ids.is_empty(),
                enemies.iter().filter(|e| e["is_alive"] == true).count() >= 2,
                rows(&raw["hand"])
                    .iter()
                    .any(|c| !c["enchantment"].is_null() || c["base_cost"] != c["effective_cost"]),
            ))
        {
            let mut changed = rm.clone();
            let cs = changed.combat.as_mut().expect("simulator combat");
            sts2_core::blindfold::blindfold_root(cs, 202_608);
            let original = native_observed_row(net, rm.clone());
            let redealt = native_observed_row(net, changed);
            let label = match (original, redealt) {
                (Ok(Some(a)), Ok(Some(b))) => {
                    if a == b
                        && net
                            .try_logits(&a)
                            .ok()
                            .zip(net.try_logits(&b).ok())
                            .is_some_and(|(left, right)| left == right)
                    {
                        "same_policy_and_logits_after_redeal"
                    } else {
                        "different_policy_or_logits_after_redeal"
                    }
                }
                (Ok(None), Ok(None)) => "redeal_forced_position",
                _ => "redeal_oracle_unavailable",
            };
            *self.checks.entry(label).or_default() += 1;
        }
        let outcome = match compare(&raw, rm, actions, net, self).and_then(|status| {
            if unproven {
                Err(mismatch(
                    "sim_card_state_unproven",
                    "A relevant card instance field has no audited public carrier",
                ))
            } else {
                Ok(status)
            }
        }) {
            Ok(outcome) => outcome,
            Err(err) => {
                if err.message == "Upstream combat translation dropped a board item"
                    && let Ok(obs) = observation::sanitize_live(&raw, Context::Combat)
                    && let Ok((_, warnings)) = engine::combat_wire(&obs.board, rm.ascension_level)
                {
                    for warning in warnings.into_iter().collect::<BTreeSet<_>>() {
                        *self.importer_warnings.entry(warning).or_default() += 1;
                    }
                }
                if let Some(encounter) = raw["encounter_id"].as_str() {
                    *self
                        .first_failure_by_encounter
                        .entry(encounter.to_owned())
                        .or_default()
                        .entry(err.message.clone())
                        .or_default() += 1;
                }
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
        if legacy_card_marker {
            *self
                .reevaluated_legacy_card_blocks
                .entry(outcome.to_owned())
                .or_default() += 1;
        }
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
        Some(outcome)
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
    sampling: Sampling,
    mut experiment: Option<&mut Experiment>,
) -> Result<()> {
    let mut driver = Driver::new(seed);
    let mut cache = TokCache::new();
    let mut ending = json!({"seed":seed,"status":"step_limit"});
    let mut acts = BTreeSet::new();
    let mut outcomes = Outcomes::new();
    for step in 0..steps {
        acts.insert(rm.run_state.act_index);
        let next = driver.peek(&mut rm);
        if let Some(cs) = &mut rm.combat {
            cs.combat_log.event_log.set_recording(true);
        }
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
            if let Some(outcome) = report.evaluate(&rm, &actions, net) {
                if let (Some(exp), Some(net)) = (experiment.as_deref_mut(), net) {
                    exp.evaluate(&rm, &actions, net);
                }
                let record = |by: &mut Breakdown, id: String| {
                    *by.entry(id).or_default().entry(outcome).or_default() += 1;
                };
                *outcomes.entry(outcome).or_default() += 1;
                record(
                    &mut report.outcomes_by_act,
                    rm.run_state.act_index.to_string(),
                );
                if let Some(room) = rm.run_state.current_room {
                    record(
                        &mut report.outcomes_by_room,
                        format!("{:?}", room.room_type),
                    );
                }
                if let Some(encounter) = sts2_wire::emit::run_manager::combat_encounter_id(&rm) {
                    record(&mut report.outcomes_by_encounter, encounter.to_owned());
                }
                for relic in rm.run_state.relics.iter().copied().collect::<BTreeSet<_>>() {
                    record(
                        &mut report.outcomes_by_relic,
                        relic.to_python_name().to_owned(),
                    );
                }
            }
        }
        let (action, kind) = match next {
            Next::Decision(d) => {
                if let Sampling::Instinct(model) = sampling {
                    let (logits, _) = model::infer(model, &d, &rm, &mut cache)?;
                    d.class_action(argmax(&logits))
                        .map_err(|e| mismatch("sim_generation", &e))?
                } else {
                    d.member_action(choose(&d, step, seed))
                        .map_err(|e| mismatch("sim_generation", &e))?
                }
            }
            Next::Forced(f) => (f.action, f.kind),
            Next::Terminal(t) => {
                ending = json!({"seed":seed,"status":t.outcome,"error":t.error,"cap":t.cap});
                break;
            }
        };
        if let Some(exp) = experiment.as_deref_mut() {
            exp.on_action(&rm, &action, kind);
        }
        driver
            .apply(&mut rm, &action, kind)
            .map_err(|e| mismatch("sim_generation", &e))?;
        if matches!(sampling, Sampling::ConfiguredFight)
            && rm.combat.as_ref().is_none_or(|cs| cs.is_over)
        {
            ending = json!({"seed":seed,"status":"completed_combat"});
            break;
        }
    }
    ending["acts"] = json!(acts);
    ending["outcomes"] = json!(outcomes);
    ending["denominator"] = json!(outcomes.values().sum::<usize>());
    let covered = ["ok", "forced", "structural_ok"]
        .iter()
        .filter_map(|k| outcomes.get(k))
        .sum::<usize>();
    ending["coverage"] = json!(
        ending["denominator"]
            .as_u64()
            .filter(|n| *n != 0)
            .map(|n| covered as f64 / n as f64)
    );
    report.runs.push(ending);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dedup_keeps_distinct_oracle_signatures_for_one_public_board() {
        let mut report = Report::default();
        assert!(record_position(
            &mut report,
            "public".into(),
            "unused".into()
        ));
        assert!(record_position(
            &mut report,
            "public".into(),
            "consumed".into()
        ));
        assert!(!record_position(
            &mut report,
            "public".into(),
            "unused".into()
        ));
        assert_eq!(report.seen.len(), 1);
        assert_eq!(report.semantic_positions, 2);
        assert_eq!(report.public_collisions, 1);
        assert_eq!(report.duplicates, 1);
    }

    #[test]
    #[ignore = "requires the pinned Instinct weights"]
    fn architecture_model_smoke_uses_native_advice_and_masks_candidate() {
        let _scope = Scope::enter(model::BUILD);
        let weights = model::default_weights();
        let net = model::load(&weights).unwrap();
        let mut strict = Report::default();
        let mut experiment = Experiment::new(&weights);
        let rm = targeted_run("NIBBITS_WEAK", FIRST_SEED).unwrap();
        walk(
            &mut strict,
            rm,
            Some(&net),
            FIRST_SEED,
            20,
            Sampling::ConfiguredFight,
            Some(&mut experiment),
        )
        .unwrap();
        let strict = summarize(strict, true);
        let result = experiment.export();
        assert_eq!(result["stock"]["denominator"], strict["denominator"]);
        assert_eq!(result["candidate"]["denominator"], strict["denominator"]);
        assert_eq!(
            result["candidate_wire_roundtrip"]["denominator"],
            strict["denominator"]
        );
        assert!(
            result["stock_checks"]["instinct_response"]
                .as_u64()
                .unwrap_or(0)
                > 0
        );
        assert!(
            result["stock"]["outcomes"]["ok"].as_u64().unwrap_or(0) > 0,
            "{result}"
        );
        assert!(result["candidate"]["outcomes"]["ok"].as_u64().unwrap_or(0) > 0);
        assert!(
            result["candidate_hidden_redeal"]["identical"]
                .as_u64()
                .unwrap_or(0)
                > 0
        );
        assert!(result["candidate_hidden_redeal"]["different"].is_null());
        assert_eq!(result["stock_mirror"]["searched_selects"], 0);
    }
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
    let denominator = report.semantic_positions
        + report
            .outcomes
            .get("projection_error")
            .copied()
            .unwrap_or_default();
    json!({"source":"simulator_derived","live_validate_count":0,"synthetic_fixture_count":0,
        "weighted":weighted,
        "unique":report.semantic_positions,"unique_public_fingerprints":report.seen.len(),
        "distinct_semantic_positions_at_matching_public_fingerprint":report.public_collisions,
        "duplicate":report.duplicates,"projection_errors_deduplicated":false,"denominator":denominator,
        "outcomes":report.outcomes,"coverage": (denominator != 0).then(|| covered as f64/denominator as f64),
        "ranked_blockers":blockers,"ranked_blocker_shapes":blocker_details,"powers":report.powers,"relics":report.relics,"potions":report.potions,"cards":report.cards,
        "encounters":report.encounters,"intent_kinds":report.intent_kinds,"multiple_enemies":report.multiple_enemies,"combined_powers":report.combined_powers,
        "modified_cards":report.modified_cards,"historical_power_gate_rejects":report.historical_power_gate_rejects,
        "recovered_after_historical_power_gate":report.recovered_after_historical_power_gate,
        "parity_checks":report.checks,"different_policy_fields":report.token_diff,
        "positions_by_act":report.positions_by_act,"positions_by_room":report.positions_by_room,
        "outcomes_by_act":report.outcomes_by_act,"outcomes_by_room":report.outcomes_by_room,
        "outcomes_by_encounter":report.outcomes_by_encounter,"outcomes_by_relic":report.outcomes_by_relic,
        "relic_counter_diffs":report.relic_counter_diffs,
        "oracle_card_variable_shapes":report.private_card_variables,
        "simulator_importer_warnings":report.importer_warnings,
        "first_failure_by_encounter":report.first_failure_by_encounter,
        "positions_with_old_projector_card_marker":report.legacy_card_marker_positions,
        "outcomes_for_old_projector_card_marker":report.reevaluated_legacy_card_blocks,
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

#[derive(Clone, Copy)]
enum Sampling<'a> {
    Development,
    ConfiguredFight,
    Instinct(&'a E2eNet),
}

pub fn uncertainty_poc(net: &E2eNet) -> Result<Value> {
    use sts2_core::{Relic, RelicId};

    let _scope = Scope::enter(model::BUILD);
    let mut outcomes = BTreeMap::<_, usize>::new();
    let cases = ["CORPSE_SLUGS_NORMAL", "NIBBITS_WEAK", "BYGONE_EFFIGY_ELITE"];
    let variants = [(RelicId::RuinedHelmet, 0), (RelicId::RuinedHelmet, 1)];
    let mut forwards = 0;
    for (index, encounter) in cases.into_iter().enumerate() {
        let seed = FIRST_SEED + index as i64;
        let mut original = targeted_run(encounter, seed)?;
        let mut driver = Driver::new(seed);
        for step in 0..16 {
            let next = driver.peek(&mut original);
            let Some(cs) = &original.combat else { break };
            if matches!(&next, Next::Decision(d) if d.is_combat())
                && emit::pending_choice(cs).is_none()
            {
                let result = (|| {
                    let raw = sim_observation::combat(&original, &emit::legal_actions(cs))?;
                    let obs = observation::sanitize_live(&raw, Context::Combat)?;
                    let base = engine::reconstruct(&obs)?;
                    let mut rows = Vec::with_capacity(variants.len());
                    for (id, counter) in variants {
                        // The public fixture declares the same equipped relic in
                        // both worlds, with no public witness of its activation.
                        // The exhaustive two-value domain comes from the rules,
                        // never from the source combat's native hidden values.
                        let mut world = base.clone();
                        world.run_state.relics.push(id);
                        let combat = world.combat.as_mut().expect("reconstructed combat");
                        combat.relics.push(Relic {
                            id: combat
                                .relics
                                .iter()
                                .map(|r| r.id)
                                .max()
                                .unwrap()
                                .checked_add(1)
                                .expect("fixture relic ID"),
                            kind: id.to_relic_kind().expect("combat relic"),
                            counter,
                            is_melted: false,
                            is_wax: false,
                        });
                        let (actions, _) = engine::checked_actions(&obs, &world)?;
                        let decision = engine::checked_decision(&obs, &mut world, &actions)?;
                        let row = model::observed_combat_row(net, &decision, &world)?;
                        let logits = net
                            .try_logits(&row)
                            .map_err(|e| mismatch("inference", &e))?;
                        model::validate_logits(decision.members.len(), &logits)?;
                        forwards += 1;
                        rows.push((actions, decision.classes, decision.members, row, logits));
                    }
                    Ok::<_, Failure>(rows)
                })();
                match result {
                    Ok(rows) if rows[0] == rows[1] => {
                        *outcomes.entry("equivalent").or_default() += 1;
                    }
                    Ok(rows) => {
                        *outcomes.entry("different").or_default() += 1;
                        for (different, label) in [
                            (rows[0].0 != rows[1].0, "legal_actions_and_targets"),
                            (rows[0].1 != rows[1].1 || rows[0].2 != rows[1].2, "classes"),
                            (rows[0].3 != rows[1].3, "tokens"),
                            (rows[0].4 != rows[1].4, "logits"),
                        ] {
                            if different {
                                *outcomes.entry(label).or_default() += 1;
                            }
                        }
                    }
                    Err(err) => *outcomes.entry(err.code).or_default() += 1,
                }
            }
            let (action, kind) = match next {
                Next::Decision(d) => d
                    .member_action(choose(&d, step, seed))
                    .map_err(|e| mismatch("sim_generation", &e))?,
                Next::Forced(f) => (f.action, f.kind),
                Next::Terminal(_) => break,
            };
            driver
                .apply(&mut original, &action, kind)
                .map_err(|e| mismatch("sim_generation", &e))?;
        }
    }
    Ok(
        json!({"schema_version":1,"source":"configured_simulator_fights","scope":"Ruined Helmet only",
        "variant_domain":[0,1],"max_positions":48,"max_forwards":96,"forwards":forwards,"results":outcomes,
        "model":model::pin()}),
    )
}

const REGRESSION_FIRST_SEED: i64 = 182_003_700;

#[derive(Clone, Copy, clap::ValueEnum)]
pub enum ArchitecturePanel {
    Development,
    Regression,
    Expanded,
    Validation,
    /// Diagnose the existing native-wire mismatches in one Act 2 run.
    WireDiagnostic,
}

pub fn architecture(
    net: &E2eNet,
    weights: &std::path::Path,
    panel: ArchitecturePanel,
    new_first_seed: Option<i64>,
) -> Result<Value> {
    let _scope = Scope::enter(model::BUILD);
    let (name, first, runs, steps, sampling, expected) = match panel {
        ArchitecturePanel::Development => (
            "development",
            FIRST_SEED,
            8,
            180,
            Sampling::Development,
            Some(701),
        ),
        ArchitecturePanel::Regression => (
            "regression",
            REGRESSION_FIRST_SEED,
            4,
            1500,
            Sampling::Instinct(net),
            Some(2269),
        ),
        ArchitecturePanel::Expanded => (
            "expanded",
            754_255_491,
            8,
            1500,
            Sampling::Instinct(net),
            Some(4333),
        ),
        ArchitecturePanel::WireDiagnostic => (
            "wire-diagnostic",
            754_255_493,
            1,
            1500,
            Sampling::Instinct(net),
            Some(610),
        ),
        ArchitecturePanel::Validation => {
            let first = new_first_seed.ok_or_else(|| {
                mismatch(
                    "architecture_seed",
                    "Validation panel requires --first-seed",
                )
            })?;
            if [
                FIRST_SEED..FIRST_SEED + 8,
                REGRESSION_FIRST_SEED..REGRESSION_FIRST_SEED + 4,
                754_255_491..754_255_499,
            ]
            .into_iter()
            .any(|known| first < known.end && first + 8 > known.start)
            {
                return Err(mismatch(
                    "architecture_seed",
                    "Validation seeds overlap a frozen panel",
                ));
            }
            ("validation", first, 8, 1500, Sampling::Instinct(net), None)
        }
    };
    let mut strict = Report::default();
    let mut comparison = Experiment::new(weights);
    if matches!(panel, ArchitecturePanel::WireDiagnostic) {
        comparison.enable_native_diagnostic();
    }
    for seed in first..first + runs {
        comparison.run_boundary();
        walk(
            &mut strict,
            new_run_at(seed, 0),
            Some(net),
            seed,
            steps,
            sampling,
            Some(&mut comparison),
        )?;
    }
    let strict = summarize(strict, true);
    let comparison = comparison.export();
    let denominator = strict["denominator"].as_u64();
    for n in [
        expected,
        comparison["stock"]["denominator"].as_u64(),
        comparison["candidate"]["denominator"].as_u64(),
        comparison["candidate_wire_roundtrip"]["denominator"].as_u64(),
    ]
    .into_iter()
    .flatten()
    {
        if denominator != Some(n) {
            return Err(mismatch(
                "architecture_cohort",
                "Architecture denominators differ from the frozen panel",
            ));
        }
    }
    Ok(
        json!({"schema_version":1,"source":"simulator_derived", "panel":name, "first_seed":first,
        "upstream_revision":model::REVISION,"engine_build":model::BUILD.as_str(),"model":model::pin(),
        "search_fallback":false,"production_data_policy_changed":false,
        "strict":strict,"comparison":comparison}),
    )
}

pub fn held_out(net: &E2eNet, runs: u8, steps: usize, first_seed: Option<i64>) -> Result<Value> {
    let _scope = Scope::enter(model::BUILD);
    let mut campaign = Report::default();
    let first_seed = first_seed.unwrap_or(REGRESSION_FIRST_SEED);
    for seed in first_seed..first_seed + i64::from(runs) {
        walk(
            &mut campaign,
            new_run_at(seed, 0),
            Some(net),
            seed,
            steps,
            Sampling::Instinct(net),
            None,
        )?;
    }
    Ok(
        json!({"schema_version":1,"upstream_revision":model::REVISION,"engine_build":model::BUILD.as_str(),
        "model":model::pin(),"sampling_policy":model::OBSERVATION,"evaluation_policy":model::OBSERVED_COMBAT,
        "first_seed":first_seed,"held_out":summarize(campaign,true)}),
    )
}

pub fn run(net: Option<&E2eNet>, runs: u8, steps: usize) -> Result<Value> {
    let _scope = Scope::enter(model::BUILD);
    let mut campaign = Report::default();
    for seed in FIRST_SEED..FIRST_SEED + i64::from(runs) {
        walk(
            &mut campaign,
            new_run_at(seed, 0),
            net,
            seed,
            steps,
            Sampling::Development,
            None,
        )?;
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
            Sampling::ConfiguredFight,
            None,
        )?;
        targeted.insert(encounter, summarize(report, net.is_some()));
    }
    Ok(
        json!({"schema_version":1,"upstream_revision":model::REVISION,"engine_build":model::BUILD.as_str(),
        "model":net.map(|_|model::pin()),"campaign":summarize(campaign,net.is_some()),"configured_fights":targeted}),
    )
}
