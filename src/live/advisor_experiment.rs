//! Simulator-only architecture comparison. No socket or live entry point.
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    time::Instant,
};

use serde_json::{Value, json};
use sts2_advisor::{
    advisor_mirror::AdvisorMirror,
    board_targets::BoardTargets,
    display_names::Names,
    graft,
    instinct::combat_form,
    instinct_advisor::{Context as AdvisorContext, InstinctAdvisor},
    instinct_combat,
};
use sts2_core::event_log::{EventLogEntry, LogCreature};
use sts2_core::monster_ai::WireInstanceState;
use sts2_core::{CombatState, card_by_uid, creature_by_id, run_manager::RunManager};
use sts2_e2e::{
    env::{ApplyKind, Driver, Next},
    net::{E2eNet, PolicyRow, argmax},
};
use sts2_wire::{emit, emit::run_manager as run_wire};

use super::{engine::Action, relics, sim_observation};
use crate::{Failure, Result, model};

fn rows(value: &Value) -> &[Value] {
    value.as_array().map_or(&[], Vec::as_slice)
}

fn failure(code: &'static str, detail: impl ToString) -> Failure {
    Failure::new(code, detail.to_string())
}

fn remove(v: &mut Value, keys: &[&str]) {
    if let Some(obj) = v.as_object_mut() {
        for key in keys {
            obj.remove(*key);
        }
    }
}

fn seedless(board: &mut Value) {
    remove(board, &["seed", "rng", "coop_rng", "combat_log", "brain"]);
    board["seedless"] = json!(true);
    board["act1"] = json!(match board["act_name"].as_str() {
        Some("Underdocks") => "underdocks",
        _ => "overgrowth",
    });
}

type Actions = BTreeSet<Action>;

fn action(value: &Value) -> Result<Action> {
    let mut value = value.clone();
    if value.get("target_index").is_none() {
        value["target_index"] = Value::Null;
    }
    Action::from_engine(&value)
}

fn combat_action(value: &Value) -> Value {
    // Decision members use combat form. Class resolution and the action
    // stream use run form, with the same verb but a different belt-slot key.
    if value["action"] == "drink_potion" && value["slot"].is_u64() {
        value.clone()
    } else {
        combat_form(value)
    }
}

fn board_actions(cs: &CombatState) -> Result<Actions> {
    let targets = BoardTargets::of(cs);
    rows(&emit::legal_actions(cs))
        .iter()
        .map(|a| {
            targets
                .board_action(a)
                .ok_or_else(|| failure("target_mapping", "No active board target"))
                .and_then(|a| action(&a))
        })
        .collect()
}

fn visible_hand(cs: &CombatState) -> Vec<(sts2_core::cards::CardId, u8)> {
    cs.player
        .hand
        .iter()
        .filter_map(|&uid| card_by_uid(&cs.cards, uid))
        .map(|c| (c.card_id, c.upgrade_level()))
        .collect()
}

fn visible_enemies(cs: &CombatState) -> Vec<(Option<sts2_core::MonsterKind>, i32, i32)> {
    cs.enemy_uids
        .iter()
        .enumerate()
        .filter(|(i, _)| cs.active_ordinal_of_enemy(*i).is_some())
        .filter_map(|(_, uid)| creature_by_id(&cs.creatures, uid.0))
        .map(|c| (c.species, c.current_hp, c.block))
        .collect()
}

// Projection of the native simulator into a stock producer frame, not a
// capture. These are the pinned importer's own named DynamicVar carriers.
fn stock_power_vars(board: &mut Value, wire: &Value) {
    for (p, native) in board.as_array_mut().into_iter().flatten().zip(rows(wire)) {
        let key = match p["id"].as_str().unwrap_or_default() {
            "CRIMSON_MANTLE" | "INFERNO" => "SelfDamage",
            "SKITTISH" => "HasGainedBlockThisTurn",
            "SURROUNDED" => "Facing",
            "TORIC_TOUGHNESS" => "Block",
            "POSSESS_STRENGTH" | "POSSESS_SPEED" => "Stolen",
            "THE_BOMB" => "Damage",
            "PAINFUL_STABS" => "ExtraMultiClawCount",
            _ => continue,
        };
        p["vars"][key] = native["counter"].clone();
    }
}

fn stock_board(
    rm: &RunManager,
    cs: &CombatState,
    actions: &Value,
    log_offset: usize,
) -> Result<Value> {
    let wire = emit::combat_state_to_value(cs).map_err(|e| failure("stock_projection", e))?;
    let mut frame = sim_observation::combat_from(rm, cs, actions)?;
    // These numeric simulator seeds do not have a producer's text seed.
    // Upstream grafts the real visible map through its seedless entry point.
    // The stock COMBAT importer still sees the simulator's actual cursors.
    seedless(&mut frame);
    frame["seed"] = json!(format!("SIM{}", rm.seed));
    frame["rng"] = wire["rng"].clone();
    frame["brain"] = json!("instinct");
    frame["combat_log"] = json!(
        cs.combat_log
            .event_log
            .rows()
            .iter()
            .skip(log_offset)
            .map(|r| run_wire::event_row_to_value(r, Some(rm.character_id)))
            .collect::<Vec<_>>()
    );
    stock_power_vars(&mut frame["player"]["powers"], &wire["player"]["powers"]);
    if let Some(relics) = frame["run_state"]["relics"].as_array_mut() {
        for row in relics {
            if relics::id(row).is_some_and(relics::has_private_latch)
                && let Some(native) = rows(&wire["player"]["relics"])
                    .iter()
                    .find(|r| r["id"] == row["id"])
            {
                row["counter"] = native.get("counter").cloned().unwrap_or(json!(0));
            }
        }
    }
    if let Some(enemies) = frame["enemies"].as_array_mut() {
        for (enemy, native) in enemies.iter_mut().zip(rows(&wire["encounter"])) {
            // C# names the CURRENT rolled move and the completed moves, not
            // the Rust AI's internal full state dict.
            enemy["next_move_id"] = native["move_state"].clone();
            let uid = enemy["combat_id"].as_u64();
            let mut moves: Vec<_> = cs
                .combat_log
                .event_log
                .rows()
                .iter()
                .rev()
                .filter_map(|r| match r.entry {
                    EventLogEntry::MonsterMove {
                        monster: LogCreature::Monster { uid: id, .. },
                        move_id,
                    } if Some(u64::from(id)) == uid => Some(move_id),
                    _ => None,
                })
                .take(5)
                .collect();
            moves.reverse();
            enemy["move_history"] = json!(moves);
            for key in ["fabricator_last_spawned", "curse_of_knowledge_counter"] {
                if let Some(value) = native.get(key) {
                    enemy[key] = value.clone();
                }
            }
            stock_power_vars(&mut enemy["powers"], &native["powers"]);
        }
        let targets = BoardTargets::of(cs);
        let mut i = 0;
        enemies.retain(|_| {
            let present = targets.to_board(i).is_some();
            i += 1;
            present
        });
        for card in frame["hand"].as_array_mut().into_iter().flatten() {
            card["valid_target_indices"] = json!(
                rows(&card["valid_target_indices"])
                    .iter()
                    .filter_map(|t| targets.to_board(t.as_i64()?))
                    .collect::<Vec<_>>()
            );
        }
    }
    Ok(frame)
}

/// A fresh run context from the public map. The only rich input is the
/// current combat; no real run manager or future run queue reaches policy.
fn candidate_run(frame: &Value, source: &CombatState) -> Result<RunManager> {
    let mut visible = frame.clone();
    seedless(&mut visible);
    let (mut rm, notes) = graft::graft_run(&visible, model::BUILD.as_str())
        .map_err(|e| failure("candidate_graft", format!("{e:?}")))?;
    if !notes.is_empty() {
        return Err(failure("candidate_graft_notes", notes.join(" | ")));
    }
    let mut current = source.clone();
    for ai in current.creatures.iter_mut().filter_map(|c| c.ai.as_mut()) {
        if ai.forced_next_state().is_none() {
            continue;
        }
        if ai.current_move_opt().is_none() {
            return Err(failure(
                "candidate_future_override",
                "No settled current move to retain",
            ));
        }
        let log = ai.wire_move_log();
        let state = WireInstanceState {
            log: &log,
            performed_first_move: Some(ai.performed_first_move()),
            performed: Some(ai.performed_at_least_once()),
            forced: Some(ai.current_forced()),
            counter: Some(ai.counter()),
            forced_next: None,
        };
        // Reinstalling the current move clears the queued override. Restore
        // only the bookkeeping for the current and already performed moves.
        ai.force_current_state(ai.current_state_id());
        ai.restore_wire_instance_state(&state);
    }
    // Replace the whole run RNG, including cursors that are not reset by
    // belief_root. Clear pins so no committed draw order enters this test.
    current.run_rng = rm.run_state.rng.clone();
    current.enemy_side_capture = None;
    current.enemy_side_replay = None;
    current.player.pinned_top.clear();
    current.player.pinned_bottom.clear();
    sts2_core::blindfold::belief_root(&mut current, 0);
    if let Some(note) = instinct_combat::enter_live_combat(
        &mut rm,
        current,
        &visible,
        instinct_combat::room_kind(&visible),
    ) {
        return Err(failure("candidate_encounter", note));
    }
    Ok(rm)
}

#[derive(PartialEq)]
struct FirstPolicy {
    action: Action,
    classes: Vec<usize>,
    members: Vec<Vec<usize>>,
    row: Option<PolicyRow>,
    logits: Vec<f32>,
}

fn candidate_policy(net: &E2eNet, mut rm: RunManager, expected: &Actions) -> Result<FirstPolicy> {
    let cs = rm
        .combat
        .as_ref()
        .ok_or_else(|| failure("candidate_board", "No combat"))?;
    if board_actions(cs)? != *expected {
        return Err(failure(
            "candidate_legality",
            "Full action/target set differs from producer",
        ));
    }
    let targets = BoardTargets::of(cs);
    let board_action = |a: &Value| {
        targets
            .board_action(&combat_action(a))
            .ok_or_else(|| failure("target_mapping", "No active board target"))
            .and_then(|a| action(&a))
    };
    let result = match Driver::new(0).peek(&mut rm) {
        Next::Forced(f) if f.kind == ApplyKind::Combat && expected.len() == 1 => FirstPolicy {
            action: board_action(&f.action)?,
            classes: vec![0],
            members: vec![vec![0]],
            row: None,
            logits: vec![],
        },
        Next::Forced(_) => {
            return Err(failure(
                "candidate_unscored",
                "Upstream did not score multiple actions",
            ));
        }
        Next::Decision(d) if d.is_combat() => {
            let decoded = d
                .actions
                .iter()
                .map(board_action)
                .collect::<Result<Actions>>()?;
            if decoded != *expected {
                return Err(failure(
                    "candidate_legality",
                    "Policy decision differs from producer",
                ));
            }
            let row = model::observed_combat_row(net, &d, &rm)?;
            let (logits, _) = model::forward(net, &d, &row)?;
            let (best, _) = d
                .class_action(argmax(&logits))
                .map_err(|e| failure("candidate_class", e))?;
            FirstPolicy {
                action: board_action(&best)?,
                classes: d.classes,
                members: d.members,
                row: Some(row),
                logits,
            }
        }
        _ => return Err(failure("candidate_screen", "Not a combat policy decision")),
    };
    if !expected.contains(&result.action) {
        return Err(failure(
            "candidate_illegal_best",
            "Best action is not on the producer board",
        ));
    }
    Ok(result)
}

#[derive(Default)]
struct Stats {
    outcomes: BTreeMap<&'static str, usize>,
    by_act: BTreeMap<u8, BTreeMap<&'static str, usize>>,
    by_room: BTreeMap<String, BTreeMap<&'static str, usize>>,
    by_run: BTreeMap<i64, BTreeMap<&'static str, usize>>,
    blockers: BTreeMap<&'static str, usize>,
    examples: BTreeMap<&'static str, String>,
    elapsed: Vec<f64>,
}

impl Stats {
    fn add(&mut self, rm: &RunManager, result: Result<&'static str>, millis: f64) {
        let outcome = result.unwrap_or_else(|e| {
            *self.blockers.entry(e.code).or_default() += 1;
            self.examples.entry(e.code).or_insert(e.message);
            "blocked"
        });
        let target = rm
            .run_state
            .current_room
            .map(|r| format!("{:?}", r.room_type))
            .unwrap_or_else(|| "Unknown".into());
        *self.outcomes.entry(outcome).or_default() += 1;
        *self
            .by_act
            .entry(rm.run_state.act_index)
            .or_default()
            .entry(outcome)
            .or_default() += 1;
        *self
            .by_room
            .entry(target)
            .or_default()
            .entry(outcome)
            .or_default() += 1;
        *self
            .by_run
            .entry(i64::from(rm.seed))
            .or_default()
            .entry(outcome)
            .or_default() += 1;
        if matches!(outcome, "ok" | "forced") {
            self.elapsed.push(millis);
        }
    }

    fn export(self) -> Value {
        let mut ms = self.elapsed;
        ms.sort_by(f64::total_cmp);
        let quantile =
            |q: usize| ms.get(ms.len().saturating_mul(q).div_ceil(100).saturating_sub(1));
        json!({
            "outcomes":self.outcomes,"denominator":self.outcomes.values().sum::<usize>(),
            "by_act": self.by_act,"by_room":self.by_room,"by_run":self.by_run,
            "blockers":self.blockers,"examples":self.examples,
            "local_elapsed_ms":{"count":ms.len(),"p50":quantile(50),"p95":quantile(95),"max":ms.last()}
        })
    }
}

pub(super) struct Experiment {
    advisor: InstinctAdvisor,
    cold_advisor: InstinctAdvisor,
    missing_advisor: InstinctAdvisor,
    mirror: AdvisorMirror,
    names: Names,
    stock: Stats,
    candidate: Stats,
    candidate_wire: Stats,
    cold: Stats,
    missing: Stats,
    missing_mirror: AdvisorMirror,
    mirrors: BTreeMap<String, i64>,
    seen_fight: Option<(u8, i32)>,
    log_offset: usize,
    snapshots: usize,
    stock_checks: BTreeMap<&'static str, usize>,
    invariant_strata: BTreeSet<(u8, String, bool)>,
    invariance: BTreeMap<&'static str, usize>,
    cold_positions: usize,
    missing_active: bool,
}

impl Experiment {
    pub(super) fn new(weights: &Path) -> Self {
        Self {
            advisor: InstinctAdvisor::new(weights.to_owned()),
            cold_advisor: InstinctAdvisor::new(weights.to_owned()),
            missing_advisor: InstinctAdvisor::new(weights.to_owned()),
            mirror: AdvisorMirror::new(true, false),
            names: Names::default(),
            stock: Stats::default(),
            candidate: Stats::default(),
            candidate_wire: Stats::default(),
            cold: Stats::default(),
            missing: Stats::default(),
            missing_mirror: AdvisorMirror::new(true, false),
            mirrors: BTreeMap::new(),
            seen_fight: None,
            log_offset: 0,
            snapshots: 0,
            stock_checks: BTreeMap::new(),
            invariant_strata: BTreeSet::new(),
            invariance: BTreeMap::new(),
            cold_positions: 0,
            missing_active: false,
        }
    }

    pub(super) fn run_boundary(&mut self) {
        self.collect_mirror_stats();
        self.mirror = AdvisorMirror::new(true, false);
        self.missing_mirror = AdvisorMirror::new(true, false);
        self.advisor.reset("offline run boundary");
        self.missing_advisor.reset("offline run boundary");
        self.seen_fight = None;
        self.log_offset = 0;
    }

    fn collect_mirror_stats(&mut self) {
        if let Some(stats) = self.mirror.stats().as_object() {
            for (key, value) in stats {
                *self.mirrors.entry(key.clone()).or_default() += value.as_i64().unwrap_or(0);
            }
        }
    }

    pub(super) fn on_action(&mut self, rm: &RunManager, world: &Value, kind: ApplyKind) {
        if kind != ApplyKind::Combat {
            return;
        }
        let Some(cs) = &rm.combat else {
            return;
        };
        if self.seen_fight != Some((rm.run_state.act_index, rm.run_state.game_floor())) {
            return;
        }
        let a = combat_action(world);
        let targets = BoardTargets::of(cs);
        let Some(a) = targets.board_action(&a) else {
            return;
        };
        let mut msg = json!({
            "type":"human_action","decision_type":"combat_action",
            "action":a["action"],"target_index":a["target_index"],"advice_request_id":self.snapshots.to_string()
        });
        match a["action"].as_str() {
            Some("play") => {
                if let Some(c) = a["card_index"]
                    .as_u64()
                    .and_then(|i| cs.player.hand.get(i as usize))
                    .and_then(|&uid| card_by_uid(&cs.cards, uid))
                {
                    msg["card"] = json!({"id":c.card_id.to_python_name(),"upgraded":c.upgrade_level()>0,
                        "index":a["card_index"]});
                }
            }
            Some("drink_potion" | "potion") => {
                if let Some(p) = a["slot"]
                    .as_u64()
                    .and_then(|i| cs.player.potions.get(i as usize))
                {
                    msg["potion"] = json!({"id":p.python_name(),"slot":a["slot"]});
                }
            }
            Some("end_turn") => {}
            // A pending generated choice has no equivalent card/potion
            // human_action. Reconcile it on the next settled request as
            // the real Advisor does across a missed announcement.
            _ => return,
        }
        self.mirror.on_world_action(&msg);
        self.advisor.on_human_action(&msg);
    }

    pub(super) fn evaluate(&mut self, rm: &RunManager, actions: &Value, net: &E2eNet) {
        self.snapshots += 1;
        let Some(cs) = &rm.combat else {
            return;
        };
        let key = (rm.run_state.act_index, rm.run_state.game_floor());
        if self.seen_fight != Some(key) {
            self.seen_fight = Some(key);
            self.log_offset = 0;
            // Cold and missing-history controls on a bounded cross-section,
            // separate from B's steady-state measurements.
            self.missing_active = self.cold_positions < 96;
            if self.missing_active {
                self.missing_mirror = AdvisorMirror::new(true, false);
            }
        }
        let expected = board_actions(cs);
        let base = sim_observation::combat(rm, actions);
        let started = Instant::now();
        let full = (|| {
            let expected = expected.as_ref().map_err(|e| failure(e.code, &e.message))?;
            let mut frame = stock_board(rm, cs, actions, self.log_offset)?;
            frame["request_id"] = json!(self.snapshots.to_string());
            frame["state_id"] = json!(self.snapshots);
            self.log_offset = cs.combat_log.event_log.rows().len();
            let result = stock_result(
                &self.advisor,
                &self.names,
                &mut self.mirror,
                &frame,
                cs,
                expected,
                &mut self.stock_checks,
            );
            let millis = started.elapsed().as_secs_f64() * 1000.0;
            if self.missing_active && self.cold_positions < 96 {
                self.cold_positions += 1;
                let mut frame = frame.clone();
                remove(&mut frame, &["combat_log"]);
                let sense = |advisor: &InstinctAdvisor, mirror: &mut AdvisorMirror| {
                    let start = Instant::now();
                    let res = stock_result(
                        advisor,
                        &self.names,
                        mirror,
                        &frame,
                        cs,
                        expected,
                        &mut BTreeMap::new(),
                    );
                    (res, start.elapsed().as_secs_f64() * 1000.0)
                };
                // Primary Advisor's driver is not reused by the controls.
                let (missing, missing_ms) = sense(&self.missing_advisor, &mut self.missing_mirror);
                self.cold_advisor.reset("offline reconnect");
                let (cold, cold_ms) =
                    sense(&self.cold_advisor, &mut AdvisorMirror::new(true, false));
                self.missing.add(rm, missing, missing_ms);
                self.cold.add(rm, cold, cold_ms);
            }
            Ok::<_, Failure>((result, millis))
        })();
        let (stock, millis) = full.unwrap_or_else(|e| (Err(e), 0.0));
        self.stock.add(rm, stock, millis);
        let started = Instant::now();
        let result = (|| {
            let base = base?;
            let expected = expected?;
            let candidate = candidate_run(&base, cs)?;
            let result = candidate_policy(net, candidate.clone(), &expected)?;
            let millis = started.elapsed().as_secs_f64() * 1000.0;
            let room = rm
                .run_state
                .current_room
                .map(|r| format!("{:?}", r.room_type))
                .unwrap_or_default();
            let stratum = (rm.run_state.act_index, room, cs.enemy_uids.len() > 1);
            if self.invariant_strata.insert(stratum) {
                let mut changed = cs.clone();
                changed.player.pinned_top.clear();
                changed.player.pinned_bottom.clear();
                sts2_core::blindfold::blindfold_root(&mut changed, 973_611);
                for ai in changed.creatures.iter_mut().filter_map(|c| c.ai.as_mut()) {
                    if ai.current_move_opt().is_some() {
                        ai.force_next_state(ai.current_state_id());
                    }
                }
                // Use the changed native state and its independently
                // projected envelope. Candidate must erase their real dice.
                let alternate_board = sim_observation::combat_from(rm, &changed, actions)?;
                let same = candidate_run(&alternate_board, &changed)
                    .and_then(|rm| candidate_policy(net, rm, &expected))
                    .is_ok_and(|other| other == result);
                *self
                    .invariance
                    .entry(if same { "identical" } else { "different" })
                    .or_default() += 1;
                if !same {
                    return Err(failure(
                        "candidate_future_invariance",
                        "Action/row/logits changed after hidden redeal",
                    ));
                }
            }
            let outcome = if result.row.is_some() { "ok" } else { "forced" };
            let wire_start = Instant::now();
            let roundtrip = (|| {
                let wire = emit::combat_state_to_value(
                    candidate.combat.as_ref().expect("candidate combat"),
                )
                .map_err(|e| failure("candidate_wire_export", e))?;
                let replayed = sts2_advisor::combat_mirror::build_state(&wire)
                    .map_err(|e| failure("candidate_wire_import", e))?;
                let mut transported = candidate;
                transported.combat = Some(replayed.state);
                if candidate_policy(net, transported, &expected)? != result {
                    return Err(failure(
                        "candidate_wire_parity",
                        "Wire roundtrip changed classes, actions, tokens or logits",
                    ));
                }
                Ok(outcome)
            })();
            self.candidate_wire
                .add(rm, roundtrip, wire_start.elapsed().as_secs_f64() * 1000.0);
            Ok((outcome, millis))
        })();
        if let Err(err) = &result {
            self.candidate_wire
                .add(rm, Err(failure(err.code, &err.message)), 0.0);
        }
        let (candidate, millis) = match result {
            Ok((outcome, millis)) => (Ok(outcome), millis),
            Err(err) => (Err(err), 0.0),
        };
        self.candidate.add(rm, candidate, millis);
    }

    pub(super) fn export(mut self) -> Value {
        self.collect_mirror_stats();
        json!({"stock":self.stock.export(),"candidate":self.candidate.export(),
            "candidate_wire_roundtrip":self.candidate_wire.export(),
            "stock_mirror":self.mirrors,"stock_checks":self.stock_checks,
            "stock_missing_history_control":self.missing.export(),"stock_reconnect_control":self.cold.export(),
            "candidate_hidden_redeal":self.invariance})
    }
}

fn stock_result(
    advisor: &InstinctAdvisor,
    names: &Names,
    mirror: &mut AdvisorMirror,
    frame: &Value,
    source: &CombatState,
    expected: &Actions,
    checks: &mut BTreeMap<&'static str, usize>,
) -> Result<&'static str> {
    let mut log = Vec::new();
    let result = advisor.advise(
        frame,
        "combat_action",
        AdvisorContext {
            names,
            cfg: None,
            mirror: Some(mirror),
            session: None,
            sims: None,
        },
        &mut log,
    );
    let _ = mirror.take_log();
    let _ = mirror.take_divergences();
    let result = result.map_err(|e| failure("stock_unavailable", e))?;
    if result["brain"] != "instinct" {
        return Err(failure("stock_wrong_brain", "Expected stock Instinct"));
    }
    *checks.entry("instinct_response").or_default() += 1;
    *checks.entry("has_local_plan").or_default() += usize::from(!rows(&result["plan"]).is_empty());
    let ranked = rows(&result["options"]);
    let valid = |a: &Value| action(a).is_ok_and(|a| expected.contains(&a));
    if !valid(&result["best"]) {
        return Err(failure(
            "stock_illegal_best",
            "Best action or target is not on the producer board",
        ));
    }
    *checks.entry("legal_best").or_default() += 1;
    if ranked.is_empty() || ranked.iter().any(|a| !valid(a)) {
        return Err(failure(
            "stock_illegal_ranked",
            "At least one ranked action/target is not on the producer board",
        ));
    }
    *checks.entry("legal_ranked").or_default() += 1;
    if ranked.iter().any(|a| {
        !a["prob"]
            .as_f64()
            .is_some_and(|p| p.is_finite() && (0.0..=1.0).contains(&p))
            || !a["pick_rank"].is_u64()
    }) {
        return Err(failure(
            "stock_missing_score",
            "An option has no finite probability or rank",
        ));
    }
    *checks.entry("finite_probabilities_and_ranks").or_default() += 1;
    let fallback;
    let cached = mirror
        .live_state()
        .filter(|cs| emit::pending_choice(cs).is_none());
    let rebuilt = if let Some(cached) = cached {
        cached
    } else {
        fallback = instinct_combat::combat_fight(frame, &Default::default(), None)
            .map_err(|e| failure("stock_audit_board", e))?
            .0;
        &fallback
    };
    if visible_hand(rebuilt) != visible_hand(source)
        || visible_enemies(rebuilt) != visible_enemies(source)
    {
        return Err(failure(
            "stock_instance_mapping",
            "Rebuilt hand or target identities differ",
        ));
    }
    if board_actions(rebuilt)? != *expected {
        return Err(failure(
            "stock_full_legality",
            "Full reconstructed action/target set differs",
        ));
    }
    let forced = result["note"]
        .as_str()
        .is_some_and(|s| s.split(" · ").any(|p| p == "forced"));
    if forced && expected.len() != 1 {
        return Err(failure(
            "stock_unscored",
            "Stock merged several legal actions into an unscored choice",
        ));
    }
    Ok(if forced { "forced" } else { "ok" })
}
