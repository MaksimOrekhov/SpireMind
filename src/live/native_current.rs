//! Versioned current-state observation shared with the opt-in C# producer.
use std::{collections::BTreeSet, sync::OnceLock};

use serde_json::{Map, Value, json};
use sts2_advisor::{board, combat_mirror, instinct_combat};
use sts2_core::{cards::EnchantmentId, run_manager::RunManager};
use sts2_e2e::snapshot::{engine_counter, engine_counter_unshown};

use super::{
    engine,
    observation::{self, MOD, Observation, boolean, list, number, schema, string},
    relics,
};
use crate::{Failure, Result, contract::Context};

pub(super) const VERSION: u8 = 1;
pub(super) const PROFILE: &str = "native-current-v1";
pub(super) const CAPABILITY: &str = "spiremind_native_current";

fn project(value: &Value, shape: &Value, schema_root: &Value) -> Result<Value> {
    if value.is_null() {
        return Ok(Value::Null);
    }
    let invalid = || schema("native current-state field type");
    match shape {
        Value::String(name) => match name.as_str() {
            "s" if value.as_str().is_some_and(|s| s.len() <= 512) => Ok(value.clone()),
            "i" if value.as_i64().is_some() => Ok(value.clone()),
            "b" if value.is_boolean() => Ok(value.clone()),
            "s" | "i" | "b" => Err(invalid()),
            object => project(value, &schema_root[object], schema_root),
        },
        Value::Object(fields) => {
            let raw = value.as_object().ok_or_else(invalid)?;
            fields
                .iter()
                .filter_map(|(k, s)| raw.get(k).map(|v| (k, s, v)))
                .map(|(k, s, v)| Ok((k.clone(), project(v, s, schema_root)?)))
                .collect::<Result<Map<_, _>>>()
                .map(Value::Object)
        }
        Value::Array(items) => {
            let raw = value
                .as_array()
                .filter(|items| items.len() <= 512 && items.iter().all(|v| !v.is_null()))
                .ok_or_else(invalid)?;
            let mut rows = raw
                .iter()
                .map(|v| project(v, &items[0], schema_root))
                .collect::<Result<Vec<_>>>()?;
            if items.len() == 2 {
                rows.sort_by_cached_key(Value::to_string);
            }
            Ok(Value::Array(rows))
        }
        _ => unreachable!("invalid embedded native-current schema"),
    }
}

fn fail(code: &'static str, message: impl Into<String>) -> Failure {
    Failure::new(code, message)
}

pub(super) fn sanitize(raw: &Value) -> Result<Observation> {
    let mut b = project_frame(raw)?;
    if !b["map"].is_null() {
        b["map"] = observation::map(&b["map"])?;
    }
    verify_board(&b)?;
    Ok(Observation {
        context: Context::Combat,
        board: b,
    })
}

pub(super) fn project_frame(raw: &Value) -> Result<Value> {
    static SHAPE: OnceLock<Value> = OnceLock::new();
    let shape = SHAPE.get_or_init(|| {
        serde_json::from_str(include_str!("../../patches/native-current-v1.json"))
            .expect("native-current schema")
    });
    if raw["native_current"] != VERSION {
        return Err(fail(
            "native_current_version",
            "Combat must use the versioned native-current producer contract",
        ));
    }
    let mut b = project(raw, &shape["frame"], shape)?;
    b.as_object_mut()
        .ok_or_else(|| schema("native current-state frame"))?
        .remove("capabilities");
    b["native_current"] = json!(VERSION);
    if b["type"] != "advice_request"
        || b["advice_for"] != "combat_action"
        || b["mod_version"] != MOD
        || b["player_count"] != 1
        || !matches!(b["room_type"].as_str(), Some("Monster" | "Elite" | "Boss"))
    {
        return Err(schema("native combat envelope"));
    }
    observation::live_build(string(&b, "game_version")?)?;
    if number(&b, "act", 3)? == 0
        || number(&b, "round", 1000)? == 0
        || number(&b, "state_id", i64::MAX as u64)? == 0
    {
        return Err(schema("native combat act, round or state"));
    }
    number(&b, "floor", 100)?;
    string(&b, "act_name")?;
    string(&b, "encounter_id")?;
    Ok(b)
}

fn verify_board(b: &Value) -> Result<()> {
    let run = &b["run_state"];
    if run["game_mode"] != "standard"
        || run["character"] != "IRONCLAD"
        || !matches!(run["ascension"].as_u64(), Some(0 | 10))
        || !list(b, "allies", 16)?.is_empty()
    {
        return Err(fail(
            "unsupported_run",
            "Native current supports solo standard Ironclad at A0 or A10",
        ));
    }
    let max_hp = number(run, "max_hp", 100_000)?;
    if max_hp == 0
        || number(run, "hp", max_hp)? != number(&b["player"], "hp", max_hp)?
        || b["player"]["max_hp"] != max_hp
    {
        return Err(schema("native current run and combat HP"));
    }
    number(run, "gold", 100_000)?;
    number(&b["player"], "block", 100_000)?;
    number(&b["player"], "energy", 100_000)?;
    number(&b["player"], "max_energy", 100_000)?;
    if list(&b["player"], "cards_played_this_turn", 5)?.len() != 5 {
        return Err(schema("native current card play history"));
    }
    verify_powers(&b["player"])?;
    let deck = list(run, "master_deck", 128)?;
    let slots = number(run, "potion_slot_count", 16)?;
    let belt = list(b, "potions", 16)?;
    if slots != belt.len() as u64 || belt != list(run, "potions", 16)? || deck.is_empty() {
        return Err(schema("native current run deck or belt"));
    }
    for potion in belt {
        observation::potion(potion)?;
    }
    let available = list(b, "available_actions", 3)?;
    let actions: BTreeSet<_> = available.iter().filter_map(Value::as_str).collect();
    if available.len() != actions.len()
        || !actions.contains("PLAY")
        || !actions.contains("END_TURN")
        || !actions.is_subset(&BTreeSet::from(["PLAY", "END_TURN", "POTION"]))
    {
        return Err(schema("native current available actions"));
    }
    for relic in list(run, "relics", 128)? {
        string(relic, "id")?;
        if relics::id(relic).is_some_and(relics::has_private_latch)
            && relic["counter"].as_i64().is_none()
        {
            return Err(fail(
                "native_current_relic_counter",
                "A current relic latch is missing from the producer frame",
            ));
        }
    }
    let hand = list(b, "hand", 32)?;
    let mut piles = Vec::with_capacity(4);
    for name in ["draw_pile", "discard_pile", "exhaust_pile", "play_pile"] {
        let cards = list(&b[name], "cards", 256)?;
        if number(&b[name], "count", 256)? != cards.len() as u64 {
            return Err(schema("native current pile count"));
        }
        if name == "play_pile" && !cards.is_empty() {
            return Err(fail(
                "native_current_unsettled",
                "A card is still resolving",
            ));
        }
        piles.push(cards);
    }
    for card in deck.iter().chain(hand).chain(piles.into_iter().flatten()) {
        string(card, "id")?;
        number(card, "upgrade_level", 1)?;
    }
    let mut hand_ids = BTreeSet::new();
    for card in hand {
        boolean(card, "playable")?;
        list(card, "valid_target_indices", 32)?;
        let handle = number(card, "instance_id", i64::MAX as u64)?;
        if handle == 0 || !hand_ids.insert(handle) {
            return Err(schema("native current distinct hand instances"));
        }
    }
    let mut ids = BTreeSet::new();
    let mut alive = false;
    for enemy in list(b, "enemies", 32)? {
        if !ids.insert(number(enemy, "combat_id", u32::MAX.into())?) {
            return Err(schema("native current distinct enemy combat IDs"));
        }
        string(enemy, "id")?;
        string(enemy, "name")?;
        if observation::monster_kind(enemy) == Some(sts2_core::MonsterKind::ThievingHopper) {
            return Err(fail(
                "native_current_card_provenance",
                "The producer does not identify the thief's card provenance",
            ));
        }
        alive |= boolean(enemy, "is_alive")?;
        boolean(enemy, "is_hittable")?;
        let max_hp = number(enemy, "max_hp", 100_000)?;
        number(enemy, "hp", max_hp)?;
        number(enemy, "block", 100_000)?;
        for intent in list(enemy, "intents", 8)? {
            observation::intent(intent)?;
        }
        verify_powers(enemy)?;
    }
    if !alive {
        return Err(fail(
            "native_current_unsettled",
            "No living enemy on the current frame",
        ));
    }
    Ok(())
}

fn verify_powers(creature: &Value) -> Result<()> {
    for power in list(creature, "powers", 64)? {
        string(power, "id")?;
        string(power, "name")?;
        if power["amount"].as_i64().is_none() {
            return Err(schema("native current power amount"));
        }
    }
    Ok(())
}

pub(super) fn reconstruct(board: &Value, rm: &mut RunManager) -> Result<()> {
    let mut current = board.clone();
    let relics = current["run_state"]["relics"]
        .as_array_mut()
        .expect("admitted relics");
    for relic in relics.iter_mut() {
        if let Some(id) = relics::id(relic) {
            let name = id.to_python_name();
            let counter = match relic["counter"].as_i64() {
                Some(shown) => engine_counter(name, shown),
                None => engine_counter_unshown(name),
            };
            if let Some(counter) = counter {
                relic["counter"] = json!(counter);
            }
        }
    }
    engine::remove_pickup_only_relics(relics);
    for enemy in current["enemies"].as_array_mut().expect("admitted enemies") {
        enemy["move_state"] = enemy["current_move"].take();
        // The native restore accepts completed bookkeeping. Construct the
        // subobject explicitly. The rolled-state log and resumed move are
        // deliberately absent, including in an archival replay.
        if enemy["completed_move_flags"].is_object() {
            enemy["ai"] = enemy["completed_move_flags"].take();
        }
    }
    let stamp = |cards: &mut Value| {
        for card in cards.as_array_mut().into_iter().flatten() {
            if card["observer_enchantment_status"] != "Disabled"
                || !card["combat_vars"]["enchantment_used"].is_null()
            {
                continue;
            }
            if let Some(used) = card["enchantment"]
                .as_str()
                .and_then(EnchantmentId::from_wire_name)
                .and_then(engine::enchantment_used_var)
            {
                card["combat_vars"]["enchantment_used"] = json!(used);
            }
        }
    };
    stamp(&mut current["hand"]);
    for pile in ["draw_pile", "discard_pile", "exhaust_pile", "play_pile"] {
        stamp(&mut current[pile]["cards"]);
    }
    let (wire, warnings) =
        board::combat_wire(&current, Some("Ironclad"), rm.ascension_level.into(), 0, 0)
            .map_err(|e| fail("native_current_board", e.to_string()))?;
    if !warnings.is_empty() {
        return Err(fail("native_current_board_warning", warnings.join(" | ")));
    }
    let mirror = combat_mirror::build_state(&wire)
        .map_err(|e| fail("native_current_mirror", e.to_string()))?;
    if !mirror.intent_damage_known {
        return Err(fail(
            "native_current_intent",
            "The game did not provide all current attack damage",
        ));
    }
    let mut fight = mirror.state;
    fight.run_rng = rm.run_state.rng.clone();
    fight.enemy_side_capture = None;
    fight.enemy_side_replay = None;
    fight.player.pinned_top.clear();
    fight.player.pinned_bottom.clear();
    sts2_core::blindfold::belief_root(&mut fight, 0);
    if let Some(reason) = instinct_combat::enter_live_combat(
        rm,
        fight,
        &current,
        instinct_combat::room_kind(&current),
    ) {
        return Err(fail("native_current_encounter", reason));
    }
    engine::check_telegraphs(board, rm, false)
}
