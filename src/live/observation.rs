use std::collections::BTreeSet;

use serde_json::{Map, Value, json};

use crate::{Failure, Result, contract::Context};

pub const MOD: &str = "0.11.10";

// The pinned engine documents this exact stable release (cc1f9462) as
// decompiling identically to 41cef1ea. This is an explicit pair, not hotfix matching.
pub fn live_build(version: &str) -> Result<()> {
    match version {
        "v0.111.0" | "v0.111.1" => Ok(()),
        _ => Err(Failure::new(
            "incompatible_game_version",
            "Live bridge expects v0.111.0 or the documented stable v0.111.1 equivalent",
        )),
    }
}

pub struct Observation {
    pub context: Context,
    pub board: Value,
}

pub fn schema(field: &str) -> Failure {
    Failure::new(
        "schema_mismatch",
        format!("Missing or unsupported {field} in the pinned observation schema"),
    )
}

pub fn string<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value[key]
        .as_str()
        .filter(|s| !s.is_empty() && s.len() <= 256)
        .ok_or_else(|| schema(key))
}

pub fn number(value: &Value, key: &str, max: u64) -> Result<u64> {
    value[key]
        .as_u64()
        .filter(|n| *n <= max)
        .ok_or_else(|| schema(key))
}

pub fn boolean(value: &Value, key: &str) -> Result<bool> {
    value[key].as_bool().ok_or_else(|| schema(key))
}

pub fn list<'a>(value: &'a Value, key: &str, max: usize) -> Result<&'a [Value]> {
    value[key]
        .as_array()
        .filter(|a| a.len() <= max)
        .map(Vec::as_slice)
        .ok_or_else(|| schema(key))
}

fn object(value: &Value, fields: &str, ignored: &str, label: &str) -> Result<Value> {
    let input = value.as_object().ok_or_else(|| schema(label))?;
    let mut out = Map::new();
    for (key, value) in input {
        if fields.split_ascii_whitespace().any(|k| k == key) {
            out.insert(key.clone(), value.clone());
        } else if !ignored.split_ascii_whitespace().any(|k| k == key) {
            let field =
                if key.len() <= 60 && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
                    key.as_str()
                } else {
                    "<invalid key>"
                };
            return Err(Failure::new(
                "unknown_field",
                format!("Unsupported field {label}.{field}"),
            ));
        }
    }
    Ok(Value::Object(out))
}

fn map_list(values: &[Value], f: impl Fn(&Value) -> Result<Value>) -> Result<Value> {
    Ok(Value::Array(
        values.iter().map(f).collect::<Result<Vec<_>>>()?,
    ))
}

fn ints(value: &Value, key: &str, range: std::ops::RangeInclusive<i64>) -> Result<()> {
    if value[key].is_null() || value[key].as_i64().is_some_and(|n| range.contains(&n)) {
        Ok(())
    } else {
        Err(schema(key))
    }
}

fn primitive_fields(value: &Value, names: &[&str], accepts: fn(&Value) -> bool) -> Result<()> {
    for &key in names {
        if !value[key].is_null() && !accepts(&value[key]) {
            return Err(schema(key));
        }
    }
    Ok(())
}

fn short_text(value: &Value) -> bool {
    value.as_str().is_some_and(|s| s.len() <= 256)
}

fn integer_array(value: &Value, size: usize, maximum: u64) -> bool {
    value.as_array().is_some_and(|values| {
        values.len() <= size
            && values
                .iter()
                .all(|v| v.as_u64().is_some_and(|n| n <= maximum))
    })
}

fn stat_map(value: &Value, label: &str) -> Result<()> {
    if value.is_null()
        || value.as_object().is_some_and(|m| {
            m.len() <= 32
                && m.iter().all(|(k, v)| {
                    k.len() <= 48
                        && k.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                        && v.as_i64()
                            .is_some_and(|n| (-100_000..=100_000).contains(&n))
                })
        })
    {
        Ok(())
    } else {
        Err(schema(label))
    }
}

fn card(value: &Value) -> Result<Value> {
    let c = object(
        value,
        "id name index upgrade_level upgraded cost base_cost persistent_cost played_cost stable_cost effective_cost costs_x type target_type exhaust innate retain ethereal will_exhaust will_retain sly sly_this_turn stats affliction affliction_stacks enchantment enchantment_amount playable unplayable_reason valid_target_indices base_replay_count star_cost persistent_star_cost",
        "description rarity is_upgradable is_removable is_curse is_status pile combat_vars forecast stable_idx uid uid_hint position",
        "card",
    )?;
    string(&c, "id")?;
    string(&c, "name")?;
    number(&c, "upgrade_level", 1)?;
    stat_map(&c["stats"], "card stats")?;
    primitive_fields(
        &c,
        &[
            "type",
            "target_type",
            "affliction",
            "enchantment",
            "unplayable_reason",
        ],
        short_text,
    )?;
    primitive_fields(
        &c,
        &[
            "upgraded",
            "costs_x",
            "exhaust",
            "innate",
            "retain",
            "ethereal",
            "will_exhaust",
            "will_retain",
            "sly",
            "sly_this_turn",
            "playable",
        ],
        Value::is_boolean,
    )?;
    if !c["valid_target_indices"].is_null() && !integer_array(&c["valid_target_indices"], 32, 31) {
        return Err(schema("valid_target_indices"));
    }
    for k in [
        "cost",
        "base_cost",
        "persistent_cost",
        "played_cost",
        "stable_cost",
        "effective_cost",
        "star_cost",
        "persistent_star_cost",
        "affliction_stacks",
        "enchantment_amount",
        "base_replay_count",
        "index",
    ] {
        ints(&c, k, -1..=1000)?;
    }
    if value.get("combat_vars").is_some_and(|v| !v.is_null()) {
        return Err(Failure::new(
            "unsupported_mechanic",
            "Card exposes unsupported per-instance combat state",
        ));
    }
    Ok(c)
}

fn relic(value: &Value) -> Result<Value> {
    let r = object(
        value,
        "id name counter stack_count is_wax is_melted status",
        "description rarity",
        "relic",
    )?;
    let id = string(&r, "id")?;
    primitive_fields(&r, &["name", "status"], short_text)?;
    primitive_fields(&r, &["is_wax", "is_melted"], Value::is_boolean)?;
    if matches!(id, "TOY_BOX" | "POCKETWATCH" | "GOLDEN_COMPASS")
        || r["is_wax"] == true
        || r["is_melted"] == true
    {
        return Err(Failure::new(
            "unsupported_mechanic",
            "Relic needs additional observation or mapping",
        ));
    }
    ints(&r, "counter", -1000..=1000)?;
    ints(&r, "stack_count", 0..=1000)?;
    Ok(r)
}

fn potion(value: &Value) -> Result<Value> {
    let p = object(
        value,
        "slot empty id name usage can_use target_type requires_target",
        "description rarity",
        "potion",
    )?;
    number(&p, "slot", 15)?;
    primitive_fields(&p, &["id", "name", "usage", "target_type"], short_text)?;
    primitive_fields(&p, &["can_use", "requires_target"], Value::is_boolean)?;
    if !boolean(&p, "empty")? {
        string(&p, "id")?;
        string(&p, "name")?;
        string(&p, "usage")?;
        string(&p, "target_type")?;
        boolean(&p, "can_use")?;
        boolean(&p, "requires_target")?;
    }
    Ok(p)
}

fn power(value: &Value) -> Result<Value> {
    let p = object(
        value,
        "id name amount display_amount",
        "description type stack_type target applier player_target vars instance_id",
        "power",
    )?;
    string(&p, "id")?;
    string(&p, "name")?;
    if !p["amount"]
        .as_i64()
        .is_some_and(|n| (-10_000..=10_000).contains(&n))
    {
        return Err(schema("power amount"));
    }
    ints(&p, "display_amount", -10_000..=10_000)?;
    // Upstream serializes private counters in vars for some powers. Only powers
    // expressible by their shown amount enter this stateless rebuild.
    let id = p["id"].as_str().unwrap();
    let id = id.strip_suffix("_POWER").unwrap_or(id);
    if !matches!(
        id,
        "STRENGTH"
            | "DEXTERITY"
            | "VULNERABLE"
            | "WEAK"
            | "FRAIL"
            | "ARTIFACT"
            | "RITUAL"
            | "THORNS"
            | "METALLICIZE"
            | "CURL_UP"
            | "ANGRY"
            | "INTANGIBLE"
            | "BUFFER"
            | "PLATED_ARMOR"
            | "ENTANGLED"
            | "VIGOR"
    ) {
        return Err(Failure::new(
            "unsupported_mechanic",
            "Power is outside the audited amount-only set",
        ));
    }
    Ok(p)
}

fn intent(value: &Value) -> Result<Value> {
    let i = object(
        value,
        "kind intent_type damage hits total_damage",
        "title description next_move_id forecast",
        "intent",
    )?;
    let kind = string(&i, "intent_type")?;
    primitive_fields(&i, &["kind"], short_text)?;
    ints(&i, "damage", 0..=100_000)?;
    ints(&i, "hits", 1..=100)?;
    ints(&i, "total_damage", 0..=10_000_000)?;
    if kind.eq_ignore_ascii_case("attack") || kind.eq_ignore_ascii_case("deathblow") {
        number(&i, "damage", 100_000)?;
        number(&i, "hits", 100)?;
    }
    Ok(i)
}

fn enemy(value: &Value) -> Result<Value> {
    let mut e = object(
        value,
        "id name combat_id hp max_hp block is_alive is_hittable slot slot_name powers intents",
        "side is_player is_monster next_move_id move_state move_history ai forecast fabricator_last_spawned curse_of_knowledge_counter",
        "enemy",
    )?;
    let id = string(&e, "id")?;
    if matches!(id, "FABRICATOR" | "KNOWLEDGE_DEMON") {
        return Err(Failure::new(
            "unsupported_mechanic",
            "Encounter needs private counters",
        ));
    }
    string(&e, "name")?;
    number(&e, "hp", 100_000)?;
    number(&e, "max_hp", 100_000)?;
    number(&e, "block", 100_000)?;
    ints(&e, "combat_id", 0..=i64::from(u32::MAX))?;
    ints(&e, "slot", 0..=64)?;
    primitive_fields(&e, &["slot_name"], short_text)?;
    boolean(&e, "is_alive")?;
    boolean(&e, "is_hittable")?;
    if e["is_alive"] == true && e["is_hittable"] != true {
        return Err(Failure::new(
            "unsupported_mechanic",
            "A living enemy is temporarily untargetable",
        ));
    }
    e["powers"] = map_list(list(&e, "powers", 64)?, power)?;
    e["intents"] = map_list(list(&e, "intents", 8)?, intent)?;
    Ok(e)
}

fn coord(value: &Value) -> Result<Value> {
    let p = object(value, "col row", "", "map coordinate")?;
    number(&p, "col", 15)?;
    number(&p, "row", 40)?;
    Ok(p)
}

fn node(value: &Value, graph: bool) -> Result<Value> {
    let mut n = object(
        value,
        if graph {
            "col row type children"
        } else {
            "col row type index"
        },
        "",
        "map node",
    )?;
    number(&n, "col", 15)?;
    number(&n, "row", 40)?;
    let kind = string(&n, "type")?;
    if !matches!(
        kind,
        "Unknown" | "Monster" | "Elite" | "Boss" | "Shop" | "RestSite" | "Treasure" | "Ancient"
    ) {
        return Err(schema("map node type"));
    }
    if graph {
        n["children"] = map_list(list(&n, "children", 16)?, coord)?;
    } else {
        number(&n, "index", 255)?;
    }
    Ok(n)
}

pub fn point(v: &Value) -> Result<(u64, u64)> {
    Ok((number(v, "col", 15)?, number(v, "row", 40)?))
}

fn map(value: &Value) -> Result<Value> {
    let mut m = object(
        value,
        "row_count col_count current boss second_boss visited nodes",
        "encounter_queue event_queue",
        "act map",
    )?;
    number(&m, "row_count", 41)?;
    number(&m, "col_count", 16)?;
    if m.get("current").is_none() {
        return Err(schema("map current"));
    }
    if !m["current"].is_null() {
        m["current"] = coord(&m["current"])?;
    }
    for name in ["boss", "second_boss"] {
        if name == "second_boss" && m[name].is_null() {
            continue;
        }
        let b = object(&m[name], "col row id", "", "boss node")?;
        point(&b)?;
        if name == "boss" || !b["id"].is_null() {
            string(&b, "id")?;
        }
        m[name] = b;
    }
    m["visited"] = map_list(list(&m, "visited", 64)?, coord)?;
    m["nodes"] = map_list(list(&m, "nodes", 256)?, |n| node(n, true))?;
    let graph = list(&m, "nodes", 256)?;
    let coords = graph.iter().map(point).collect::<Result<BTreeSet<_>>>()?;
    if coords.len() != graph.len() || coords.is_empty() {
        return Err(schema("unique map nodes"));
    }
    let boss = point(&m["boss"])?;
    for n in graph {
        let (_, row) = point(n)?;
        for child in list(n, "children", 16)? {
            let p = point(child)?;
            if p.1 <= row || (!coords.contains(&p) && p != boss) {
                return Err(schema("map edge"));
            }
        }
    }
    Ok(m)
}

fn run_state(value: &Value) -> Result<Value> {
    let mut run = object(
        value,
        "gold hp max_hp potions potion_slot_count relics master_deck ascension game_mode character is_last_act awaiting_second_boss",
        "unlocked_epochs dominion seed rng encounter_queue event_queue",
        "run_state",
    )?;
    if string(&run, "game_mode")? != "standard" || string(&run, "character")? != "IRONCLAD" {
        return Err(Failure::new(
            "unsupported_run",
            "Only solo standard Ironclad is supported",
        ));
    }
    if !matches!(number(&run, "ascension", 10)?, 0 | 10) {
        return Err(Failure::new(
            "unsupported_ascension",
            "Only A0 and A10 are admitted by this bridge",
        ));
    }
    number(&run, "gold", 100_000)?;
    number(&run, "hp", 100_000)?;
    number(&run, "max_hp", 100_000)?;
    let slots = number(&run, "potion_slot_count", 16)?;
    primitive_fields(
        &run,
        &["is_last_act", "awaiting_second_boss"],
        Value::is_boolean,
    )?;
    run["master_deck"] = map_list(list(&run, "master_deck", 128)?, card)?;
    run["relics"] = map_list(list(&run, "relics", 128)?, relic)?;
    run["potions"] = map_list(list(&run, "potions", 16)?, potion)?;
    if slots != list(&run, "potions", 16)?.len() as u64
        || run["master_deck"].as_array().is_none_or(Vec::is_empty)
        || run["max_hp"] == 0
    {
        return Err(schema("run deck, HP or potion slots"));
    }
    Ok(run)
}

pub fn context(value: &Value) -> Result<Context> {
    match string(value, "advice_for")? {
        "combat_action" => Ok(Context::Combat),
        "card_reward" => Ok(Context::CardReward),
        "map_select" => Ok(Context::Map),
        _ => Err(Failure::new(
            "unsupported_state",
            "This screen is outside SM-02",
        )),
    }
}

pub fn sanitize(raw: &Value, context: Context) -> Result<Observation> {
    let mut b = object(
        raw,
        "type advice_for state_id request_id game_version mod_version player_count act act_name floor current_room run_state map surface nodes cards can_skip player hand draw_pile discard_pile exhaust_pile play_pile draw_pile_count discard_pile_count exhaust_pile_count enemies potions available_actions round room_type encounter_id allies orbs orb_capacity pets combat turn",
        "seed seedless seedless_seed rng coop_rng combat_log capabilities brain language forecast encounter_queue event_queue",
        "advice_request",
    )?;
    if b["type"] != "advice_request" || b["mod_version"] != MOD {
        return Err(schema("advisor type or mod_version"));
    }
    live_build(string(&b, "game_version")?)?;
    primitive_fields(&b, &["request_id", "current_room"], short_text)?;
    if b["player_count"] != 1 {
        return Err(Failure::new(
            "unsupported_run",
            "Only one player is supported",
        ));
    }
    if number(&b, "act", 3)? == 0 || number(&b, "state_id", i64::MAX as u64)? == 0 {
        return Err(schema("act or state_id"));
    }
    number(&b, "floor", 100)?;
    string(&b, "act_name")?;
    b["run_state"] = run_state(&b["run_state"])?;
    if !b["map"].is_null() {
        b["map"] = map(&b["map"])?;
    }
    match context {
        Context::Combat => {
            if !matches!(string(&b, "room_type")?, "Monster" | "Elite" | "Boss")
                || !b["combat"].is_null()
            {
                return Err(schema("combat surface"));
            }
            string(&b, "encounter_id")?;
            if number(&b, "round", 1000)? == 0 {
                return Err(schema("combat round"));
            }
            for k in ["orbs", "pets", "allies"] {
                if !list(&b, k, 16)?.is_empty() {
                    return Err(Failure::new(
                        "unsupported_mechanic",
                        "Orbs, pets or allies require a wider observation adapter",
                    ));
                }
            }
            let player_fields = "hp max_hp block energy max_energy powers cards_played_this_turn bound_card_played_this_turn hp_lost_this_turn unblocked_hits_this_combat powered_hits_this_turn cards_exhausted_this_combat cards_exhausted_this_turn card_block_gains_this_turn discards_this_turn cards_drawn_this_combat cards_generated_this_combat extra_turns_taken_this_combat";
            let mut p = object(
                &b["player"],
                player_fields,
                "net_id seat_index stars",
                "combat player",
            )?;
            for k in ["hp", "max_hp", "block", "energy", "max_energy"] {
                number(&p, k, 100_000)?;
            }
            ints(&b, "orb_capacity", 0..=16)?;
            ints(&b, "turn", 0..=1000)?;
            let played = &p["cards_played_this_turn"];
            if !played.as_array().is_some_and(|values| values.len() == 5)
                || !integer_array(played, 5, 1000)
            {
                return Err(schema("cards_played_this_turn"));
            }
            primitive_fields(
                &p,
                &["bound_card_played_this_turn", "hp_lost_this_turn"],
                Value::is_boolean,
            )?;
            for name in [
                "unblocked_hits_this_combat",
                "cards_exhausted_this_combat",
                "cards_exhausted_this_turn",
                "card_block_gains_this_turn",
                "discards_this_turn",
                "cards_drawn_this_combat",
                "cards_generated_this_combat",
                "extra_turns_taken_this_combat",
            ] {
                ints(&p, name, 0..=100_000)?;
            }
            p["powers"] = map_list(list(&p, "powers", 64)?, power)?;
            if p.get("powered_hits_this_turn").is_some() {
                p["powered_hits_this_turn"] =
                    map_list(list(&p, "powered_hits_this_turn", 32)?, |hit| {
                        let clean = object(hit, "enemy hits", "", "powered hits")?;
                        number(&clean, "enemy", 32)?;
                        number(&clean, "hits", 100_000)?;
                        Ok(clean)
                    })?;
            }
            b["player"] = p;
            b["hand"] = map_list(list(&b, "hand", 32)?, |raw| {
                let c = card(raw)?;
                number(&c, "effective_cost", 1000)?;
                boolean(&c, "playable")?;
                string(&c, "target_type")?;
                list(&c, "valid_target_indices", 32)?;
                Ok(c)
            })?;
            for (pile, count) in [
                ("draw_pile", Some("draw_pile_count")),
                ("discard_pile", Some("discard_pile_count")),
                ("exhaust_pile", Some("exhaust_pile_count")),
                ("play_pile", None),
            ] {
                let raw = object(
                    &b[pile],
                    "cards count",
                    "type known_top known_bottom rng order",
                    "combat pile",
                )?;
                let mut cards = list(&raw, "cards", 128)?
                    .iter()
                    .map(card)
                    .collect::<Result<Vec<_>>>()?;
                if number(&raw, "count", 128)? != cards.len() as u64 {
                    return Err(schema("serialized pile count"));
                }
                if let Some(k) = count
                    && number(&b, k, 128)? != cards.len() as u64
                {
                    return Err(schema("pile count"));
                }
                if pile == "play_pile" && !cards.is_empty() {
                    return Err(schema("settled combat turn"));
                }
                if pile == "draw_pile" {
                    for c in &mut cards {
                        c.as_object_mut().unwrap().remove("index");
                    }
                    cards.sort_by_cached_key(Value::to_string);
                }
                b[pile] = json!({"cards":cards,"count":cards.len()});
            }
            b["enemies"] = map_list(list(&b, "enemies", 32)?, enemy)?;
            b["potions"] = map_list(list(&b, "potions", 16)?, potion)?;
            let available = list(&b, "available_actions", 4)?;
            if !available.contains(&json!("END_TURN"))
                || available
                    .iter()
                    .any(|v| !matches!(v.as_str(), Some("PLAY" | "END_TURN" | "POTION")))
            {
                return Err(schema("available action kinds"));
            }
        }
        Context::CardReward => {
            if !b["surface"].is_null() || !b["combat"].is_null() {
                return Err(Failure::new(
                    "unsupported_state",
                    "Only the normal card reward screen is supported",
                ));
            }
            string(&b, "current_room")?;
            boolean(&b, "can_skip")?;
            b["cards"] = map_list(list(&b, "cards", 16)?, |c| {
                let c = card(c)?;
                number(&c, "index", 255)?;
                Ok(c)
            })?;
        }
        Context::Map => {
            b["nodes"] = map_list(list(&b, "nodes", 16)?, |n| node(n, false))?;
        }
    }
    // The fields above allow the pinned shared envelope. Remove fields from
    // other contexts even when an input redundantly includes them.
    let mandatory = "type advice_for state_id request_id game_version mod_version player_count act act_name floor current_room run_state map";
    let local = match context {
        Context::Combat => {
            "player hand draw_pile discard_pile exhaust_pile play_pile draw_pile_count discard_pile_count exhaust_pile_count enemies potions available_actions round room_type encounter_id allies orbs orb_capacity pets turn"
        }
        Context::CardReward => "cards can_skip",
        Context::Map => "nodes",
    };
    b.as_object_mut().unwrap().retain(|k, _| {
        mandatory
            .split_ascii_whitespace()
            .chain(local.split_ascii_whitespace())
            .any(|x| x == k)
    });
    Ok(Observation { context, board: b })
}
