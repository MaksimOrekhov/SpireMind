//! Test-suite projection of the simulator's public combat surface.
//! This module has no access to the live connection.
use serde_json::{Value, json};
use sts2_core::{
    Card, CombatState, LocalCostMod, RelicId, card_by_uid,
    cards::{CardType, TargetType, VarKey, effective_target_type},
    creature_by_id,
    event_log::EventLogEntry,
    potion_effects,
    potions::PotionTargetType,
    run_manager::RunManager,
};
use sts2_e2e::snapshot;
use sts2_wire::emit::{self, run_manager as run_wire};

use super::relics;
use crate::{Failure, Result, model::BUILD};

fn projection(message: &str) -> Failure {
    Failure::new("sim_projection", message)
}

fn card_model(c: &Card) -> Value {
    let id = c.card_id.to_python_name();
    let mut result = json!({"id":id,"name":id,"upgrade_level":c.upgrade_level()});
    if let Some((enchantment, amount)) = run_wire::card_enchantment(&c.enchantments) {
        result["enchantment"] = json!(enchantment);
        result["enchantment_amount"] = json!(amount);
    }
    result
}

fn combat_card(cs: &CombatState, c: &Card, wire: &Value) -> Value {
    let mut result = card_model(c);
    result["type"] = json!(format!("{:?}", c.card_type));
    result["target_type"] = json!(match effective_target_type(cs, c) {
        TargetType::Enemy => "AnyEnemy",
        TargetType::AllEnemies => "AllEnemies",
        TargetType::Self_ => "Self",
        TargetType::None => "None",
        TargetType::Random => "RandomEnemy",
    });
    result["base_cost"] = wire["base_cost"].clone();
    result["cost"] = wire["cost_override"].clone();
    result["effective_cost"] = wire["cost_override"].clone();
    if !c.cost.mods().is_empty() && c.base_cost() >= 0 {
        for (key, expiry) in [
            ("persistent_cost", LocalCostMod::END_OF_TURN),
            ("played_cost", LocalCostMod::WHEN_PLAYED),
            (
                "stable_cost",
                LocalCostMod::END_OF_TURN | LocalCostMod::WHEN_PLAYED,
            ),
        ] {
            let cost = c
                .cost
                .mods()
                .iter()
                .filter(|m| m.flags() & expiry == 0)
                .fold(i32::from(c.base_cost()), |cost, m| m.modify(cost));
            result[key] = json!(cost.max(0));
        }
    }
    let mut stats = serde_json::Map::new();
    for (key, field) in [("damage", "base_damage"), ("block", "base_block")] {
        if wire[field].as_i64().is_some_and(|n| n != 0) {
            stats.insert(key.to_owned(), wire[field].clone());
        }
    }
    if !stats.is_empty() {
        result["stats"] = Value::Object(stats);
    }
    if let Some(mask) = c
        .enchantments
        .first()
        .and_then(|(id, _)| super::engine::enchantment_used_var(id))
    {
        result["observer_enchantment_status"] = if c.combat_var(VarKey::EnchantmentUsed) & mask == 0
        {
            json!("Normal")
        } else {
            json!("Disabled")
        };
    }
    for key in ["affliction", "affliction_stacks", "base_replay_count"] {
        if let Some(value) = wire.get(key) {
            result[key] = value.clone();
        }
    }
    // These are the only card/field pairs SerializeCard actually emits.
    // Its payload is a round, while the engine's value counts player turns.
    let played = c.combat_var(VarKey::ReturnIfPlayedRound);
    if matches!(result["id"].as_str(), Some("BOLAS" | "THRUMMING_HATCHET"))
        && cs.turn_number() == cs.round
        && played > 0
        && (i32::from(cs.round) - 1..=i32::from(cs.round)).contains(&played)
    {
        result["combat_vars"] = json!({"return_if_played_round":played});
    }
    if let Some(flags) = wire["flags"].as_array() {
        for key in ["ethereal", "innate", "retain"] {
            if flags.iter().any(|v| v == key) {
                result[key] = json!(true);
            }
        }
    }
    result
}

// This audit stays outside the projected Advisor data. The producer cannot
// send a marker for a field it does not serialize.
pub(super) fn card_variables(rm: &RunManager) -> Vec<(&'static str, VarKey, i32, bool)> {
    let Some(cs) = &rm.combat else {
        return Vec::new();
    };
    cs.player
        .hand
        .iter()
        .chain(&cs.player.draw)
        .chain(&cs.player.discard)
        .chain(&cs.player.exhaust)
        .chain(&cs.player.play)
        .filter_map(|&uid| card_by_uid(&cs.cards, uid))
        .flat_map(|c| {
            c.combat_vars.iter().map(move |(k, value)| {
                let published = k == VarKey::EnchantmentUsed
                    && c.enchantments
                        .first()
                        .and_then(|(id, _)| super::engine::enchantment_used_var(id))
                        == Some(value);
                (c.card_id.to_python_name(), k, value, published)
            })
        })
        .collect()
}

fn powers(wire: &Value) -> Vec<Value> {
    rows(wire)
        .iter()
        .map(|p| {
            let mut public = json!({
                "id":p["name"],"name":p["name"],"amount":p["amount"],
                "display_amount":p["display_amount"],"observer_visible":true
            });
            if p["name"] == "CRIMSON_MANTLE" {
                public["observer_self_damage"] = p["counter"].clone();
            }
            public
        })
        .collect()
}

fn rows(wire: &Value) -> &[Value] {
    wire.as_array().map_or(&[], Vec::as_slice)
}

fn map(rm: &RunManager) -> Result<Value> {
    let run = &rm.run_state;
    let full = run_wire::full_map(rm);
    let graph = run
        .current_act_map
        .as_ref()
        .ok_or_else(|| projection("No map"))?;
    let point = |v: &Value| json!({"col":v["col"], "row":v["row"], "id":v["id"]});
    let boss = &full["boss"];
    let boss_row = boss["row"]
        .as_i64()
        .ok_or_else(|| projection("No visible boss"))?;
    let nodes = rows(&full["rows"])
        .iter()
        .flat_map(rows)
        .filter(|n| n["type"] != "Boss")
        .map(|n| json!({"col":n["col"],"row":n["row"],"type":n["type"],"children":n["children"]}))
        .collect::<Vec<_>>();
    let visited = run
        .visited_map_coords
        .iter()
        .map(|p| json!({"col":p.col,"row":p.row}))
        .collect::<Vec<_>>();
    let columns = graph.grid.keys().map(|p| p.col).max().unwrap_or(6) + 1;
    Ok(json!({
        "current":full["current_coord"],"boss":point(boss),
        "row_count":boss_row+1,"col_count":columns,"nodes":nodes,"visited":visited
    }))
}

pub(super) fn combat(rm: &RunManager, actions: &Value) -> Result<Value> {
    let cs = rm.combat.as_ref().ok_or_else(|| projection("No combat"))?;
    // Upstream belief view removes the unknown draw order and redeals RNG.
    // Only an explicit public projection of its wire leaves this function.
    let cs = snapshot::belief_view(cs);
    let wire =
        snapshot::state_dict(&cs).map_err(|_| projection("Simulator state export failed"))?;
    let telegraphs = emit::combat::enemy_telegraphs(&cs)
        .map_err(|_| projection("Simulator telegraph export failed"))?;
    let run = &rm.run_state;
    let player = &wire["player"];
    let mut public_player = json!({
        "hp":player["hp"],"max_hp":player["max_hp"],"energy":player["energy"],
        "max_energy":player["max_energy"],"block":player["block"],"powers":powers(&player["powers"]),
        "cards_played_this_turn":[0,0,0,0,0],
        "card_block_gains_this_turn":0,"extra_turns_taken_this_combat":0
    });
    for field in [
        "cards_played_this_turn",
        "bound_card_played_this_turn",
        "hp_lost_this_turn",
        "unblocked_hits_this_combat",
        "powered_hits_this_turn",
        "cards_exhausted_this_combat",
        "cards_exhausted_this_turn",
        "card_block_gains_this_turn",
        "discards_this_turn",
        "cards_drawn_this_combat",
        "cards_generated_this_combat",
        "extra_turns_taken_this_combat",
    ] {
        if let Some(v) = player.get(field) {
            public_player[field] = v.clone();
        }
    }
    let belt = (0..run.potion_belt_cap())
        .map(|slot| {
            let Some(p) = cs.player.potions.get(slot).copied() else {
                return json!({"slot":slot,"empty":true});
            };
            let target = match potion_effects::lookup_target_type(p) {
                Some(PotionTargetType::Self_) => "Self",
                Some(PotionTargetType::AnyEnemy) => "AnyEnemy",
                Some(PotionTargetType::AllEnemies) => "AllEnemies",
                Some(PotionTargetType::AnyPlayer) => "AnyPlayer",
                None => "Unknown",
            };
            let usable = rows(actions).iter().any(|a| a["slot"] == slot && matches!(a["action"].as_str(), Some("potion" | "drink_potion")));
            let mut entry = json!({"slot":slot,"empty":false,"id":p.python_name(),"name":p.python_name(),"usage":"CombatOnly",
                "can_use":usable,"target_type":target,"requires_target":target=="AnyEnemy"});
            if target == "AnyEnemy" {
                entry["valid_target_indices"] = json!(rows(actions).iter()
                    .filter(|a| a["slot"] == slot && matches!(a["action"].as_str(), Some("potion" | "drink_potion")))
                    .filter_map(|a| a["target_index"].as_u64()).collect::<Vec<_>>());
            }
            entry
        })
        .collect::<Vec<_>>();
    let relics = run
        .relics
        .iter()
        .map(|kind| {
            let id = kind.to_python_name();
            let combat_relic = rows(&player["relics"]).iter().find(|r| r["id"] == id);
            let run_counter = i64::from(run.relic_counters.get(kind).copied().unwrap_or_default());
            let counter = combat_relic.map_or(run_counter, |row| {
                row["counter"].as_i64().unwrap_or_default()
            });
            let mut relic = json!({"id":id,"name":id});
            if let Some(v) = snapshot::display_counter(id, counter) {
                relic["counter"] = json!(v);
            }
            if let Some(values) = relics::status_values(*kind)
                && let Some((status, _)) =
                    values.into_iter().find(|(_, c)| i64::from(*c) == counter)
            {
                relic["observer_relic_status"] = json!(status);
            }
            match kind {
                RelicId::CentennialPuzzle => {
                    relic["observer_trigger_seen"] =
                        json!(cs.combat_log.unblocked_damage_received_this_combat > 0);
                }
                RelicId::Permafrost
                    if cs.combat_log.event_log.recording()
                        && cs.combat_log.event_log.dropped() == 0 =>
                {
                    relic["observer_trigger_seen"] =
                        json!(cs.combat_log.event_log.rows().iter().any(|row|
                        matches!(row.entry, EventLogEntry::CardPlayFinished { card, upgraded }
                            if sts2_content::cards::spec_index::card_pod_from_id(card, upgraded)
                                .is_some_and(|card| card.card_type == CardType::Power))));
                }
                _ => {}
            }
            for key in ["is_wax", "is_melted"] {
                if let Some(v) = combat_relic.and_then(|row| row.get(key)) {
                    relic[key] = v.clone();
                }
            }
            if run.melted_relics.contains(kind) {
                relic["is_melted"] = json!(true);
            }
            relic
        })
        .collect::<Vec<_>>();
    let mut enemies = Vec::new();
    for (i, uid) in cs.enemy_uids.iter().enumerate() {
        let e = creature_by_id(&cs.creatures, uid.0)
            .ok_or_else(|| projection("Missing enemy instance"))?;
        let id = e
            .species
            .ok_or_else(|| projection("Missing enemy species"))?
            .python_name();
        let alive = e.current_hp > 0;
        let visible_revive = !alive
            && rows(&wire["encounter"][i]["powers"])
                .iter()
                .any(|p| p["name"] == "ILLUSION")
            && rows(&telegraphs[i]).len() == 1
            && telegraphs[i][0]["kind"] == "Heal";
        let intents = if alive || visible_revive {
            rows(&telegraphs[i])
                .iter()
                .map(|intent| {
                    let mut v = json!({"intent_type":intent["kind"]});
                    if let Some(damage) = intent.get("effective_amount") {
                        v["damage"] = damage.clone();
                        v["hits"] = intent.get("times").cloned().unwrap_or(json!(1));
                    }
                    v
                })
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        enemies.push(json!({"id":id,"name":id,"combat_id":uid.0.get(),"hp":e.current_hp.max(0),
            "max_hp":e.max_hp,"block":e.block,"is_alive":alive,"is_hittable":cs.is_hittable_enemy(*uid),
            "powers":powers(&wire["encounter"][i]["powers"]),"intents":intents}));
    }
    let room = format!(
        "{:?}",
        run.current_room
            .ok_or_else(|| projection("No room"))?
            .room_type
    );
    let act = match run.act_index {
        1 => format!("{:?}", run.act1),
        2 => "Hive".to_owned(),
        _ => "Glory".to_owned(),
    };
    let mut available = vec!["END_TURN"];
    if rows(actions).iter().any(|a| a["action"] == "play") {
        available.push("PLAY");
    }
    if belt.iter().any(|p| p["can_use"] == true) {
        available.push("POTION");
    }
    let mut board = json!({
        "type":"advice_request","advice_for":"combat_action","request_id":"a1","state_id":1,
        "game_version":BUILD.as_str(),"mod_version":super::observation::MOD,
        "player_count":1,"act":run.act_index,"act_name":act,"floor":run.game_floor(),
        "current_room":room,"room_type":room,"encounter_id":run_wire::combat_encounter_id(rm),
        "capabilities":{"spiremind_lease":1,"spiremind_power_visibility":1,"spiremind_card_status":1,"spiremind_potion_targets":1,
            "spiremind_relic_witness":1,"spiremind_power_tooltip":1},
        "map":map(rm)?,"player":public_player,"enemies":enemies,
        "run_state":{"character":"IRONCLAD","game_mode":"standard","ascension":rm.ascension_level,
            "hp":player["hp"],"max_hp":player["max_hp"],"gold":run_wire::gold(rm),"relics":relics,
            "master_deck":run.master_deck.iter().map(card_model).collect::<Vec<_>>(),
            "potions":belt,"potion_slot_count":belt.len()},
        "round":cs.round,"orbs":[],"allies":[],"pets":[],"potions":belt,"available_actions":available,
    });
    for (wire_key, board_key, uids) in [
        ("hand", "hand", cs.player.hand.as_slice()),
        ("draw_pile", "draw_pile", cs.player.draw.as_slice()),
        ("discard_pile", "discard_pile", cs.player.discard.as_slice()),
        ("exhaust_pile", "exhaust_pile", cs.player.exhaust.as_slice()),
        ("play_pile", "play_pile", cs.player.play.as_slice()),
    ] {
        let source = rows(&wire["deck"][wire_key]);
        if source.len() != uids.len() {
            return Err(projection("Simulator pile and wire differ"));
        }
        let mut cards = Vec::with_capacity(source.len());
        for (i, (&uid, w)) in uids.iter().zip(source).enumerate() {
            let c =
                card_by_uid(&cs.cards, uid).ok_or_else(|| projection("Missing card instance"))?;
            let mut value = combat_card(&cs, c, w);
            if wire_key == "hand" {
                let plays: Vec<_> = rows(actions)
                    .iter()
                    .filter(|a| a["action"] == "play" && a["card_index"] == i)
                    .collect();
                value["playable"] = json!(!plays.is_empty());
                value["valid_target_indices"] = json!(
                    plays
                        .iter()
                        .filter_map(|a| a["target_index"].as_u64())
                        .collect::<Vec<_>>()
                );
            }
            cards.push(value);
        }
        if wire_key == "hand" {
            board[board_key] = json!(cards);
        } else {
            board[format!("{board_key}_count")] = json!(cards.len());
            board[board_key] = json!({"cards":cards,"count":cards.len()});
        }
    }
    board
        .as_object_mut()
        .expect("constructed object")
        .remove("play_pile_count");
    Ok(board)
}
