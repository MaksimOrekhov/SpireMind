use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Value, json};
use sts2_advisor::{board, combat_mirror, graft, instinct_combat, instinct_screens};
use sts2_core::{card_by_uid, creature_by_id, hooks::effective_card_cost, run_manager::RunManager};
use sts2_e2e::{
    build::Scope,
    env::{Driver, Next},
    net::E2eNet,
    tokens::TokCache,
};
use sts2_wire::emit::{self, run_manager as run_wire};

use super::observation::{Observation, boolean, list, number, point, schema, string};
use crate::{
    Failure, Result,
    contract::{self, Context},
    model::{self, BUILD},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Action {
    Play(u64, Option<u64>),
    Potion(u64, Option<u64>),
    EndTurn,
    Reward(Option<u64>),
    Move(u64),
}

impl Action {
    fn from_engine(value: &Value) -> Result<Self> {
        let index = |field| number(value, field, 255);
        let optional = |field: &str| {
            if value.get(field) == Some(&Value::Null) {
                Ok(None)
            } else {
                number(value, field, 255).map(Some)
            }
        };
        match value["action"].as_str() {
            Some("play") => Ok(Self::Play(index("card_index")?, optional("target_index")?)),
            Some("end_turn") => Ok(Self::EndTurn),
            Some("potion" | "drink_potion") => {
                Ok(Self::Potion(index("slot")?, optional("target_index")?))
            }
            Some("pick_card_reward") => Ok(Self::Reward(optional("idx")?)),
            Some("move") => Ok(Self::Move(index("col")?)),
            _ => Err(coverage("Engine action has no supported mapping")),
        }
    }
}

fn coverage(message: &str) -> Failure {
    Failure::new("action_coverage", message)
}
fn rebuild(message: &str) -> Failure {
    Failure::new("reconstruction", message)
}

fn targets(b: &Value, card: &Value) -> Result<Vec<Option<u64>>> {
    let indexes = list(card, "valid_target_indices", 32)?;
    let kind = string(card, "target_type")?;
    if kind != "AnyEnemy" {
        if !indexes.is_empty()
            || !matches!(
                kind,
                "None" | "Self" | "AllEnemies" | "RandomEnemy" | "TargetedNoCreature"
            )
        {
            return Err(coverage("Playable card uses an unsupported target type"));
        }
        return Ok(vec![None]);
    }
    let enemies = list(b, "enemies", 32)?;
    let mut sorted = BTreeSet::new();
    for index in indexes {
        let i = index
            .as_u64()
            .filter(|&i| i < enemies.len() as u64)
            .ok_or_else(|| coverage("Invalid game target index"))?;
        if enemies[i as usize]["is_alive"] != true
            || enemies[i as usize]["is_hittable"] != true
            || !sorted.insert(Some(i))
        {
            return Err(coverage(
                "The game target list is inconsistent with its enemy list",
            ));
        }
    }
    if sorted.is_empty() {
        return Err(coverage("Playable targeted card has no legal target"));
    }
    Ok(sorted.into_iter().collect())
}

fn enemy_detail(b: &Value, target: Option<u64>) -> Value {
    target.map_or(Value::Null, |i| {
        let enemy = &b["enemies"][i as usize];
        json!({"enemy_index": i, "id": enemy["id"], "name": enemy["name"], "combat_id": enemy["combat_id"]})
    })
}

fn screen_actions(obs: &Observation, rm: &RunManager) -> Result<BTreeMap<Action, Value>> {
    let b = &obs.board;
    let mut actions = BTreeMap::new();
    match obs.context {
        Context::Combat => {
            let hand = list(b, "hand", 32)?;
            let cs = rm
                .combat
                .as_ref()
                .ok_or_else(|| rebuild("Combat state missing after graft"))?;
            if cs.player.hand.len() != hand.len()
                || cs.enemy_uids.len() != list(b, "enemies", 32)?.len()
            {
                return Err(coverage(
                    "Hand or enemy indexes changed during reconstruction",
                ));
            }
            for (i, (card, &uid)) in hand.iter().zip(&cs.player.hand).enumerate() {
                let engine = card_by_uid(&cs.cards, uid)
                    .ok_or_else(|| rebuild("Card instance missing after graft"))?;
                if engine.card_id.to_python_name() != instinct_screens::registry_id(&card["id"])
                    || engine.upgraded != (card["upgrade_level"] == 1)
                    || card["effective_cost"].as_i64()
                        != Some(i64::from(effective_card_cost(cs, uid)))
                {
                    return Err(coverage(
                        "Card identity, upgrade or effective cost changed during reconstruction",
                    ));
                }
                if boolean(card, "playable")? {
                    for target in targets(b, card)? {
                        actions.insert(Action::Play(i as u64,target),json!({"kind":"play","card":{"hand_index":i,"id":card["id"],"name":card["name"],"effective_cost":card["effective_cost"],"upgrade_level":card["upgrade_level"],"enchantment":card["enchantment"],"affliction":card["affliction"]},"target":enemy_detail(b,target)}));
                    }
                }
            }
            for (e, uid) in list(b, "enemies", 32)?.iter().zip(&cs.enemy_uids) {
                let enemy = creature_by_id(&cs.creatures, uid.0)
                    .ok_or_else(|| rebuild("Enemy missing after graft"))?;
                if instinct_screens::norm_id(&e["id"])
                    != instinct_screens::norm_id(&json!(enemy.species.map(|s| s.python_name())))
                    || e["hp"] != enemy.current_hp
                    || e["block"] != enemy.block
                    || e["max_hp"] != enemy.max_hp
                {
                    return Err(coverage(
                        "Enemy identity or ordering changed during reconstruction",
                    ));
                }
            }
            actions.insert(Action::EndTurn, json!({"kind":"end_turn"}));
            let mut use_potions = false;
            let belt = list(b, "potions", 16)?;
            if belt != list(&b["run_state"], "potions", 16)? {
                return Err(coverage("Combat belt differs from the run belt"));
            }
            for (i, p) in belt.iter().enumerate() {
                if p["slot"] != i {
                    return Err(coverage("Potion slots are not in game order"));
                }
                if p["empty"] == true || p["can_use"] == false {
                    continue;
                }
                use_potions = true;
                // The session only admits one player. An AnyPlayer potion
                // with no target picker can only address that player.
                if p["requires_target"] == true
                    || !matches!(
                        p["target_type"].as_str(),
                        Some(
                            "None"
                                | "Self"
                                | "AnyPlayer"
                                | "AllEnemies"
                                | "RandomEnemy"
                                | "TargetedNoCreature"
                        )
                    )
                {
                    return Err(coverage(
                        "The producer cannot prove all targets of this potion",
                    ));
                }
                actions.insert(
                    Action::Potion(i as u64, None),
                    json!({"kind":"potion","slot":i,"id":p["id"],"name":p["name"]}),
                );
            }
            if use_potions != list(b, "available_actions", 4)?.contains(&json!("POTION")) {
                return Err(coverage(
                    "Potion availability differs from the game actions",
                ));
            }
        }
        Context::CardReward => {
            let cards = list(b, "cards", 16)?;
            if cards.is_empty() || rm.card_reward_options().len() != cards.len() {
                return Err(coverage(
                    "Card reward cannot be matched to the engine offer",
                ));
            }
            let mut game_indexes = BTreeSet::new();
            for (i, c) in cards.iter().enumerate() {
                let index = number(c, "index", 255)?;
                if !game_indexes.insert(index)
                    || rm.card_reward_options()[i].to_python_name()
                        != instinct_screens::registry_id(&c["id"])
                    || rm.card_reward_upgraded_flags().get(i).copied()
                        != Some(c["upgrade_level"] == 1)
                {
                    return Err(coverage(
                        "Reward card identity or index changed during reconstruction",
                    ));
                }
                actions.insert(Action::Reward(Some(i as u64)),json!({"kind":"reward_card","offer_index":index,"id":c["id"],"name":c["name"],"upgrade_level":c["upgrade_level"],"enchantment":c["enchantment"]}));
            }
            if boolean(b, "can_skip")? {
                actions.insert(Action::Reward(None), json!({"kind":"skip"}));
            }
        }
        Context::Map => {
            let mut game_indexes = BTreeSet::new();
            let grid = rm
                .run_state
                .current_act_map
                .as_ref()
                .ok_or_else(|| rebuild("Missing reconstructed map grid"))?;
            if list(b, "nodes", 16)?.is_empty() {
                return Err(coverage("The game reported no reachable map nodes"));
            }
            for n in list(b, "nodes", 16)? {
                let (col, row) = point(n)?;
                let index = number(n, "index", 255)?;
                let position = sts2_core::map::MapCoord::new(col as i32, row as i32);
                let visible_type_matches = list(&b["map"], "nodes", 256)?
                    .iter()
                    .find(|node| node["col"] == col && node["row"] == row)
                    .map_or_else(
                        || n["type"] == "Boss" && point(&b["map"]["boss"]).ok() == Some((col, row)),
                        |node| node["type"] == n["type"],
                    );
                if !game_indexes.insert(index) || !visible_type_matches || (!grid.grid.contains_key(&position) && !grid.boss.is_some_and(|boss| grid.point(boss).coord == position))
                    || actions.insert(Action::Move(col),json!({"kind":"move","node_index":index,"col":col,"row":row,"room_type":n["type"]})).is_some() {
                    return Err(coverage("Ambiguous reachable node or unmatched map coordinate"));
                }
            }
            // Comparing columns alone would silently accept a node on the wrong row.
            let map_obs = &b["map"];
            let current = if map_obs["current"].is_null() {
                (3, 0)
            } else {
                point(&map_obs["current"])?
            };
            let row = current.1 + 1;
            if actions.values().any(|n| n["row"] != row) {
                return Err(coverage("Map choice does not lead to the next row"));
            }
        }
    }
    Ok(actions)
}

fn check_telegraphs(b: &Value, rm: &RunManager) -> Result<()> {
    let cs = rm
        .combat
        .as_ref()
        .ok_or_else(|| rebuild("Missing combat state"))?;
    let telegraphs = emit::combat::enemy_telegraphs(cs)
        .map_err(|_| rebuild("Could not read reconstructed telegraphs"))?;
    let emitted = telegraphs
        .as_array()
        .ok_or_else(|| rebuild("Expected telegraph array"))?;
    let enemies = list(b, "enemies", 32)?;
    if emitted.len() != enemies.len() {
        return Err(rebuild("Reconstructed enemy count differs"));
    }
    for (e, row) in enemies.iter().zip(emitted) {
        if e["is_alive"] == false {
            continue;
        }
        let actual = list(e, "intents", 8)?;
        let rebuilt = row
            .as_array()
            .ok_or_else(|| rebuild("Missing enemy telegraph"))?;
        if rebuilt.len() != actual.len()
            || actual.iter().zip(rebuilt).any(|(known, sim)| {
                instinct_screens::norm_id(&known["intent_type"])
                    != instinct_screens::norm_id(&sim["kind"])
                    || known.get("damage").is_some_and(|d| {
                        sim.get("effective_amount").or_else(|| sim.get("amount")) != Some(d)
                    })
                    || known.get("hits").is_some_and(|h| {
                        // The wire omits `times` for a single attack. The mod
                        // sends `hits: 1` for the same visible intent.
                        let hits = sim.get("times").and_then(Value::as_u64).unwrap_or(1);
                        h.as_u64() != Some(hits)
                    })
            })
        {
            return Err(rebuild(
                "A visible intent or its shown damage could not be reconstructed",
            ));
        }
    }
    Ok(())
}

fn reconstruct(obs: &Observation) -> Result<RunManager> {
    let mut board = obs.board.clone();
    if board["map"].is_null() {
        return Err(Failure::new(
            "missing_visible_map",
            "No current map was observed for this position",
        ));
    }
    board["seedless"] = json!(true);
    if obs.context == Context::Map && board["map"]["current"].is_null() {
        let map = &board["map"];
        let start = list(map, "nodes", 256)?
            .iter()
            .filter(|node| node["row"] == 0)
            .collect::<Vec<_>>();
        if !list(map, "visited", 64)?.is_empty()
            || start.len() != 1
            || start[0]["type"] != "Ancient"
            || start[0]["col"] != 3
        {
            return Err(rebuild(
                "The visible map does not prove which entrance is current",
            ));
        }
        // An unvisited entrance maps to the visible Ancient node. Upstream
        // treats row 0 as parked at the act entrance.
        board["map"]["current"] = json!({"col":3,"row":0});
    }
    if board["act"] == 1 {
        board["act1"] = match board["act_name"].as_str() {
            Some("Overgrowth" | "OVERGROWTH") => json!("overgrowth"),
            Some("Underdocks" | "UNDERDOCKS") => json!("underdocks"),
            _ => return Err(schema("first act variant")),
        };
    }
    let (mut rm, notes) = graft::graft_run(&board, BUILD.as_str())
        .map_err(|_| rebuild("Upstream seedless run graft did not reconstruct the state"))?;
    if !notes.is_empty() {
        return Err(rebuild(
            "Upstream run graft reported missing data or approximated placement",
        ));
    }
    match obs.context {
        Context::Combat => {
            let (wire, warnings) = board::combat_wire(
                &board,
                Some("Ironclad"),
                i64::from(rm.ascension_level),
                0,
                0,
            )
            .map_err(|_| rebuild("Upstream could not translate the combat board"))?;
            if !warnings.is_empty() {
                return Err(rebuild("Upstream combat translation dropped a board item"));
            }
            let fight = combat_mirror::build_state(&wire)
                .map_err(|_| rebuild("Upstream could not reconstruct combat"))?;
            if !fight.intent_damage_known {
                return Err(rebuild("Combat attack damage is incomplete"));
            }
            if instinct_combat::enter_live_combat(
                &mut rm,
                fight.state,
                &board,
                instinct_combat::room_kind(&board),
            )
            .is_some()
            {
                return Err(rebuild("The observed encounter is unsupported"));
            }
            check_telegraphs(&board, &rm)?;
        }
        Context::CardReward => {
            let room = graft::live_room(&board)
                .ok_or_else(|| rebuild("Missing current room for reward"))?;
            sts2_wire::graft::set_room_type(&mut rm, room)
                .map_err(|_| rebuild("Upstream could not reconstruct the current room"))?;
            let cards = list(&board, "cards", 16)?
                .iter()
                .map(graft::card_spec)
                .collect();
            sts2_wire::graft::open_card_offer(
                &mut rm,
                &Value::Array(cards),
                boolean(&board, "can_skip")?,
                false,
                None,
            )
            .map_err(|_| rebuild("Upstream could not reconstruct this card offer"))?;
        }
        Context::Map => {}
    }
    Ok(rm)
}

// No apply/step APIs on this path. The only changes are upstream debug grafts
// on private, newly allocated state before a single peek.
pub fn advise(net: &E2eNet, obs: &Observation) -> Result<Value> {
    let _scope = Scope::enter(BUILD);
    if let Some(result) = entrance_action(obs)? {
        return Ok(result);
    }
    let mut rm = reconstruct(obs)?;
    let screen = screen_actions(obs, &rm)?;
    let engine = match obs.context {
        Context::Combat => emit::legal_actions(
            rm.combat
                .as_ref()
                .ok_or_else(|| rebuild("Missing combat after graft"))?,
        ),
        _ => run_wire::available_actions(&rm),
    };
    let rows = engine
        .as_array()
        .ok_or_else(|| coverage("Expected engine action array"))?;
    let keys = rows
        .iter()
        .map(Action::from_engine)
        .collect::<Result<Vec<_>>>()?;
    let set = keys.iter().copied().collect::<BTreeSet<_>>();
    if rows.is_empty() || set.len() != rows.len() || !set.iter().copied().eq(screen.keys().copied())
    {
        return Err(coverage("Engine and visible game action sets differ"));
    }
    let actions = keys
        .iter()
        .enumerate()
        .map(|(i, key)| json!({"action_id":i,"action":screen[key]}))
        .collect::<Vec<_>>();
    if actions.len() == 1 {
        return Ok(json!({"status":"forced","legal_actions":actions,"forced_action_id":0}));
    }
    let Next::Decision(decision) = Driver::new(0).peek(&mut rm) else {
        return Err(coverage(
            "Upstream did not expose a scored decision for the full action set",
        ));
    };
    if Context::from_dtype(decision.dtype).ok() != Some(obs.context) || &decision.actions != rows {
        return Err(coverage(
            "Model decision and visible game action sets differ",
        ));
    }
    let (logits, forward) = model::infer(net, &decision, &rm, &mut TokCache::new())?;
    let ranked = contract::rank(&decision.classes, &decision.members, &logits)?;
    Ok(json!({"status":"ok","legal_actions":actions,"recommendations":ranked,"forward_ms":forward}))
}

fn entrance_action(obs: &Observation) -> Result<Option<Value>> {
    let board = &obs.board;
    let map = &board["map"];
    if obs.context != Context::Map
        || board["floor"] != 0
        || !map["current"].is_null()
        || !list(map, "visited", 64)?.is_empty()
        || list(map, "nodes", 256)?.iter().any(|node| node["row"] == 0)
    {
        return Ok(None);
    }
    let nodes = list(board, "nodes", 16)?;
    // The pinned producer presents a separate initial room click. Its
    // visible graph starts at row 1, so this click has no policy decision.
    if let [node] = nodes
        && point(node)? == (3, 0)
        && node["type"] == "Monster"
    {
        let index = number(node, "index", 255)?;
        return Ok(Some(
            json!({"status":"forced","forced_action_id":0,"legal_actions":[
                {"action_id":0,"action":{"kind":"move","node_index":index,"col":3,"row":0,"room_type":"Monster"}}
            ]}),
        ));
    }
    Ok(None)
}
