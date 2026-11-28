use serde_json::{Value, json};
use std::{
    io::{Cursor, Read, Write},
    sync::{Condvar, Mutex},
    time::{Duration, Instant},
};

use super::{
    Latest, Live, Terminal, engine, native_current,
    observation::{self, sanitize},
    session::{Ack, Event, Position, Session},
    transport::{Frames, MAX_FRAME, Receive},
};
use crate::{contract::Context, model};
use clap::Parser;

// Hand-authored, artificial Advisor wire, not a capture from Slay the Spire 2.
fn card(id: &str, name: &str, kind: &str, cost: u8, target: &str) -> Value {
    json!({"id":id,"name":name,"type":kind,"cost":cost,"base_cost":cost,"effective_cost":cost,"upgrade_level":0,"target_type":target,"stats":null})
}

fn attack(cost: u8, targets: &[u8]) -> Value {
    let mut c = card("STRIKE_IRONCLAD", "Strike", "Attack", cost, "AnyEnemy");
    c["playable"] = json!(true);
    c["valid_target_indices"] = json!(targets);
    c["stats"] = json!({"damage":6});
    c
}

fn base(context: Context) -> Value {
    let screen = match context {
        Context::Map => "map_select",
        Context::CardReward => "card_reward",
        Context::Combat => "combat_action",
    };
    let on_map = context == Context::Map;
    let mut b = json!({
        "type":"advice_request","request_id":"a4","state_id":11,"advice_for":screen,
        "game_version":"v0.111.1","mod_version":"0.11.10",
        "player_count":1,"act":1,"act_name":"Overgrowth","floor":if on_map {0}else{1},
        "capabilities":{"spiremind_lease":1,"spiremind_power_visibility":1},
        "current_room":if on_map {"Map"}else{"Monster"},
        "seed":"ignored","rng":{"run_seed": 512, "counters":{"shuffle":999}}, "brain":"instinct",
        "run_state":{"character":"IRONCLAD","game_mode":"standard","ascension":0,"hp":70,"max_hp":80,"gold":99,"potion_slot_count":2,
            "potions":[{"slot":0,"empty":true},{"slot":1,"empty":true}],"relics":[{"id":"BURNING_BLOOD","name":"Burning Blood"}],
            "master_deck":[card("STRIKE_IRONCLAD","Strike","Attack",1,"AnyEnemy"),card("DEFEND_IRONCLAD","Defend","Skill",1,"Self"),card("BASH","Bash","Attack",2,"AnyEnemy")]},
        "map":{"row_count":4,"col_count":7,"current":if on_map {json!({"col":3,"row":0})}else{json!({"col":2,"row":1})},
            "boss":{"col":3,"row":3,"id":"CEREMONIAL_BEAST_BOSS"},"visited":if on_map {json!([])}else{json!([{"col":2,"row":1}])},
            "nodes":[{"col":3,"row":0,"type":"Ancient","children":[{"col":2,"row":1},{"col":4,"row":1}]},
              {"col":2,"row":1,"type":"Monster","children":[{"col":3,"row":2}]},
              {"col":4,"row":1,"type":"Monster","children":[{"col":3,"row":2}]},
              {"col":3,"row":2,"type":"RestSite","children":[{"col":3,"row":3}]}]}
    });
    match context {
        Context::Map => {
            b["nodes"] = json!([{"index":0,"col":2,"row":1,"type":"Monster"},{"index":1,"col":4,"row":1,"type":"Monster"}])
        }
        Context::CardReward => {
            b["can_skip"] = json!(true);
            let mut cards = [
                card("ANGER", "Anger", "Attack", 0, "AnyEnemy"),
                card("SHRUG_IT_OFF", "Shrug It Off", "Skill", 1, "Self"),
            ];
            for (i, c) in cards.iter_mut().enumerate() {
                c["index"] = json!(i);
            }
            b["cards"] = json!(cards);
        }
        Context::Combat => {
            b["room_type"] = json!("Monster");
            b["round"] = json!(1);
            b["encounter_id"] = json!("NIBBITS_WEAK");
            b["allies"] = json!([]);
            b["pets"] = json!([]);
            b["orbs"] = json!([]);
            b["orb_capacity"] = json!(0);
            b["player"] = json!({"hp":70,"max_hp":80,"block":0,"energy":3,"max_energy":3,"powers":[],"cards_played_this_turn":[0,0,0,0,0]});
            b["hand"] = json!([attack(1, &[0, 2]), attack(0, &[0, 2])]);
            b["draw_pile"] = json!({"type":"Draw","count":2,"cards":[card("DEFEND_IRONCLAD","Defend","Skill",1,"Self"),card("BASH","Bash","Attack",2,"AnyEnemy")]});
            b["draw_pile_count"] = json!(2);
            for key in ["discard_pile", "exhaust_pile", "play_pile"] {
                b[key] = json!({"count":0,"cards":[]});
            }
            for key in ["discard_pile_count", "exhaust_pile_count"] {
                b[key] = json!(0);
            }
            b["potions"] = b["run_state"]["potions"].clone();
            b["available_actions"] = json!(["PLAY", "END_TURN"]);
            let enemy = |id, hp, alive| {
                json!({"id":"NIBBIT","name":"Nibbit","combat_id":id,"hp":hp,"max_hp":16,"block":0,
                "is_alive":alive,"is_hittable":alive,"powers":[],"intents":if alive {json!([{"kind":"AttackIntent","intent_type":"Attack","damage":5,"hits":1}])}else{json!([])},"next_move_id":"ignored"})
            };
            b["enemies"] = json!([
                enemy(91, 16, true),
                enemy(92, 0, false),
                enemy(93, 16, true)
            ]);
        }
    }
    b
}

#[test]
fn synthetic_projection_is_invariant_to_hidden_order_rng_and_forecasts() {
    let input = base(Context::Combat);
    let mut changed = input.clone();
    changed["seed"] = json!("another seed");
    changed["rng"] = json!({"run_seed":333});
    changed["draw_pile"]["cards"]
        .as_array_mut()
        .unwrap()
        .reverse();
    changed["draw_pile"]["cards"][0]["position"] = json!(42);
    changed["enemies"][0]["next_move_id"] = json!("secret");
    changed["enemies"][0]["forecast"] = json!(["secret"]);
    changed["map"]["encounter_queue"] = json!(["secret"]);
    let clean = sanitize(&input, Context::Combat).unwrap().board;
    assert_eq!(clean, sanitize(&changed, Context::Combat).unwrap().board);
    assert!(!clean.to_string().contains("secret"));
    assert!(clean.get("rng").is_none() && clean.get("seed").is_none());
    assert_eq!(clean, sanitize(&clean, Context::Combat).unwrap().board);
}

fn native_board() -> Value {
    let mut b = base(Context::Combat);
    b["native_current"] = json!(1);
    b["capabilities"]["spiremind_native_current"] = json!(1);
    // The live producer removes ordinary corpses from CombatState.Enemies.
    b["enemies"].as_array_mut().unwrap().remove(1);
    for (i, card) in b["hand"].as_array_mut().unwrap().iter_mut().enumerate() {
        card["valid_target_indices"] = json!([0, 1]);
        card["instance_id"] = json!(100 + i);
    }
    b
}

fn change_native_future(b: &mut Value, axis: usize) {
    match axis {
        0 => {
            b["seed"] = json!("hidden seed");
            b["rng"] = json!({"run_seed":-8668, "counters":{"shuffle":55,"monster_ai":3}});
            b["coop_rng"] = json!("coop rng");
            b["player"]["rng"] = json!(95);
            b["map"]["encounter_queue"] = json!(["hidden"]);
            b["run_state"]["future_rewards"] = json!(["hidden"]);
        }
        1 => {
            b["draw_pile"]["cards"].as_array_mut().unwrap().reverse();
            b["draw_pile"]["cards"][0]["uid"] = json!(99);
            b["draw_pile"]["cards"][1]["stable_idx"] = json!(99);
            b["draw_pile"]["pinned_top"] = json!([42]);
            b["draw_pile"]["known_order"] = json!([1, 0]);
        }
        2 => {
            b["enemies"][0]["ai"] = json!({"forced_next":"SECRET","log":["SECRET"],"future":[1]});
            b["enemies"][0]["completed_move_flags"]["forced_next"] = json!("SECRET");
            b["enemies"][0]["forecast"] = json!(["SECRET"]);
            b["enemies"][0]["intents"][0]["forecast"] = json!("SECRET");
            b["cached_combat"] = json!({"rng":3,"future_intent":"SECRET"});
            b["hand"][0]["combat_vars"]["some_future_result"] = json!(4);
        }
        _ => unreachable!(),
    }
}

#[test]
fn native_current_schema_and_session_require_the_current_contract() {
    let mut raw = native_board();
    raw["enemies"][0]["completed_move_flags"] = json!({"performed":true});
    raw["hand"][0]["combat_vars"] = json!({"return_if_played_round":2});
    let accepted = native_current::sanitize(&raw).unwrap().board;
    for axis in 0..3 {
        let mut dirty = raw.clone();
        change_native_future(&mut dirty, axis);
        assert_eq!(accepted, native_current::sanitize(&dirty).unwrap().board);
    }
    assert_eq!(native_current::sanitize(&accepted).unwrap().board, accepted);
    let mut session = Session::new(5);
    let Event::Observation(p) = session.accept(&raw).unwrap() else {
        panic!("expected advice frame");
    };
    assert_eq!(session.observe_native(&raw, p).unwrap().board, accepted);
    raw["native_current"] = json!(2);
    assert_eq!(
        session.observe_native(&raw, p).err().unwrap().code,
        "native_current_version"
    );
    raw["native_current"] = json!(1);
    raw["enemies"][1]["combat_id"] = raw["enemies"][0]["combat_id"].clone();
    assert!(session.observe_native(&raw, p).is_err());
    raw["enemies"][1]["combat_id"] = json!(93);
    raw["run_state"]["relics"] = json!([{"id":"LIZARD_TAIL", "status":"normal"}]);
    assert_eq!(
        session.observe_native(&raw, p).err().unwrap().code,
        "native_current_relic_counter"
    );
}

#[test]
#[ignore = "requires official Instinct weights"]
fn native_current_model_smoke_actions_tokens_and_logits_ignore_each_future_source() {
    use sts2_e2e::build::Scope;
    let _scope = Scope::enter(model::BUILD);
    let net = model::load(&model::default_weights()).unwrap();
    let mut raw = native_board();
    raw["enemies"][0]["completed_move_flags"] = json!({"performed":false});
    raw["hand"][0]["combat_vars"] = json!({});
    let infer = |board: &Value| {
        let roundtrip = serde_json::from_str(&board.to_string()).unwrap();
        let obs = native_current::sanitize(&roundtrip).unwrap();
        let mut rm = engine::reconstruct(&obs).unwrap();
        let (rows, actions) = engine::checked_actions(&obs, &rm).unwrap();
        let d = engine::checked_decision(&obs, &mut rm, &rows).unwrap();
        let tokens = model::observed_combat_row(&net, &d, &rm).unwrap();
        let (logits, ms) = model::forward(&net, &d, &tokens).unwrap();
        assert!(ms > 0.0);
        (actions, d.classes, d.members, tokens, logits)
    };
    let expected = infer(&raw);
    assert_eq!(expected.0.len(), 5);
    assert!(
        expected
            .0
            .iter()
            .any(|a| a["action"]["target"]["combat_id"] == 93)
    );
    assert!(
        !expected
            .0
            .iter()
            .any(|a| a["action"]["target"]["combat_id"] == 92)
    );
    for axis in 0..3 {
        let mut dirty = raw.clone();
        change_native_future(&mut dirty, axis);
        assert!(
            expected == infer(&dirty),
            "future axis {axis} changed advice"
        );
    }
    let obs = native_current::sanitize(&raw).unwrap();
    let result = engine::advise(&net, &obs).unwrap();
    assert_eq!(result["status"], "ok");
    assert_eq!(result["legal_actions"], json!(expected.0));
    raw["hand"][0]["valid_target_indices"] = json!([0]);
    let obs = native_current::sanitize(&raw).unwrap();
    assert_eq!(
        engine::advise(&net, &obs).unwrap_err().code,
        "action_coverage"
    );
}

#[test]
fn synthetic_blocker_panel_reports_multiple_shapes_and_redacts_hidden_inputs() {
    use super::diagnostics;
    let mut raw = base(Context::Combat);
    raw["capabilities"] = json!({"spiremind_power_visibility":1});
    raw["player"]["powers"] = json!([
        {"id":"SLOW_POWER","amount":1,"display_amount":3,"name":"Slow","observer_visible":true,"vars":{"SlowAmount":3}},
        {"id":"SECRET_POWER","amount":1,"observer_visible":false,"vars":{"private":19}}
    ]);
    raw["enemies"][0]["powers"] = json!([
        {"id":"JUGGLING_POWER","amount":1,"display_amount":2,"name":"Juggling","observer_visible":true,"vars":{"Counter":4}}
    ]);
    raw["hand"][0]["combat_vars"] = json!({"private_card":99});
    let position = Position {
        session: 1,
        state_id: 11,
        request_id: 4,
    };
    let e = sanitize(&raw, Context::Combat).err().unwrap();
    assert_eq!(e.code, "unsupported_mechanic");
    let audit = diagnostics::record(&raw, position, "observation", e.code, None);
    let blockers = audit["blockers"].as_array().unwrap();
    assert_eq!(blockers.len(), 4);
    assert!(
        blockers
            .iter()
            .any(|e| e["id"] == "SLOW" && e["field_types"]["vars"] == "object")
    );
    assert!(
        blockers
            .iter()
            .any(|e| e["path"] == "$.player.powers[1]" && e["id"].is_null())
    );
    assert!(blockers.iter().any(|e| e["id"] == "JUGGLING"));
    assert!(!audit.to_string().contains("SECRET_POWER"));
    assert!(!audit.to_string().contains("private"));
    raw["rng"] = json!({"run_seed": 987654});
    raw["player"]["powers"][1]["id"] = json!("OTHER_PRIVATE_POWER");
    raw["player"]["powers"][0]["vars"]["SlowAmount"] = json!(987654);
    raw["draw_pile"]["cards"].as_array_mut().unwrap().reverse();
    assert_eq!(
        audit,
        diagnostics::record(&raw, position, "observation", e.code, None)
    );
    raw["player"]["hp"] = json!(69);
    assert_ne!(
        audit["public_fingerprint"],
        diagnostics::record(&raw, position, "observation", e.code, None)["public_fingerprint"]
    );
    raw["capabilities"] = json!({});
    let old = diagnostics::record(&raw, position, "observation", e.code, None);
    assert!(!old.to_string().contains("SLOW"));
    assert!(!old.to_string().contains("JUGGLING"));
    let mut hidden_strength = base(Context::Combat);
    hidden_strength["player"]["powers"] = json!([
        {"id":"STRENGTH_POWER","amount":1,"name":"Strength","observer_visible":false}
    ]);
    assert_eq!(
        sanitize(&hidden_strength, Context::Combat)
            .err()
            .unwrap()
            .code,
        "unsupported_mechanic"
    );
}

#[test]
fn synthetic_generated_choice_does_not_become_a_deck_reward() {
    let mut raw = base(Context::CardReward);
    raw["surface"] = json!("choose_a_card");
    raw["prompt_key"] = json!("TO_OBTAIN");
    assert_eq!(
        sanitize(&raw, Context::CardReward).err().unwrap().code,
        "unsupported_state"
    );
}

#[test]
fn live_requires_visibility_while_legacy_replay_keeps_supported_effects() {
    let mut raw = base(Context::Combat);
    let rank = json!({"id":"STRENGTH_POWER","name":"Strength","amount":2,"display_amount":2});
    raw["player"]["powers"] = json!([rank.clone()]);
    raw["enemies"][0]["powers"] = json!([rank]);
    let live_code = |b: &Value| {
        observation::sanitize_live(b, Context::Combat)
            .err()
            .unwrap()
            .code
    };
    assert!(sanitize(&raw, Context::Combat).is_ok());
    assert_eq!(live_code(&raw), "power_visibility_unverified");
    raw["player"]["powers"][0]["observer_visible"] = json!(true);
    raw["enemies"][0]["powers"][0]["observer_visible"] = json!("true");
    assert_eq!(live_code(&raw), "power_visibility_unverified");
    raw["enemies"][0]["powers"][0]["observer_visible"] = json!(false);
    assert_eq!(live_code(&raw), "unsupported_mechanic");
    raw["enemies"][0]["powers"][0]["observer_visible"] = json!(true);
    let board = observation::sanitize_live(&raw, Context::Combat)
        .unwrap()
        .board;
    assert_eq!(board, sanitize(&board, Context::Combat).unwrap().board);
    raw["capabilities"]["spiremind_power_visibility"] = json!(true);
    assert_eq!(live_code(&raw), "power_visibility_not_supported");
    raw["capabilities"]
        .as_object_mut()
        .unwrap()
        .remove("spiremind_power_visibility");
    assert_eq!(live_code(&raw), "power_visibility_not_supported");
}

#[test]
fn synthetic_slow_uses_only_the_public_counter_carrier() {
    use super::powers::Owner;
    let mut row = json!({"id":"SLOW_POWER","name":"Slow","amount":1,"display_amount":20,
        "observer_visible":true,"vars":{"SlowAmount":2}});
    let projected = observation::live_power(&row, Owner::Enemy).unwrap();
    row["vars"]["SlowAmount"] = json!(813);
    assert_eq!(
        projected,
        observation::live_power(&row, Owner::Enemy).unwrap()
    );
    assert!(projected.get("vars").is_none());
    assert!(observation::live_power(&row, Owner::Player).is_err());
    row["display_amount"] = json!(23);
    assert_eq!(
        observation::live_power(&row, Owner::Enemy)
            .unwrap_err()
            .code,
        "power_public_parameter"
    );
    row.as_object_mut().unwrap().remove("display_amount");
    assert!(observation::live_power(&row, Owner::Enemy).is_err());
}

#[test]
fn live_enchantment_display_rules_out_wrong_card_preview_with_same_actions() {
    use sts2_e2e::build::Scope;
    use sts2_wire::emit;

    let _scope = Scope::enter(model::BUILD);
    let mut raw = base(Context::Combat);
    raw["round"] = json!(2);
    raw["player"]["extra_turns_taken_this_combat"] = json!(0);
    raw["hand"][0]["enchantment"] = json!("VIGOROUS");
    raw["hand"][0]["enchantment_amount"] = json!(5);
    assert_eq!(
        observation::sanitize_live(&raw, Context::Combat)
            .err()
            .unwrap()
            .code,
        "card_public_state_missing"
    );
    raw["hand"][0]["observer_enchantment_status"] = json!("Normal");
    assert!(observation::sanitize_live(&raw, Context::Combat).is_err());
    raw["capabilities"]["spiremind_card_status"] = json!(1);
    let active = observation::sanitize_live(&raw, Context::Combat).unwrap();
    raw["hand"][0]["observer_enchantment_status"] = json!("Disabled");
    let spent = observation::sanitize_live(&raw, Context::Combat).unwrap();
    assert_ne!(active.board, spent.board);
    let active_fight = engine::reconstruct(&active).unwrap().combat.unwrap();
    let spent_fight = engine::reconstruct(&spent).unwrap().combat.unwrap();
    assert_eq!(
        emit::legal_actions(&active_fight),
        emit::legal_actions(&spent_fight)
    );
    assert_ne!(
        emit::card_damage_live(&active_fight),
        emit::card_damage_live(&spent_fight)
    );
    assert!(sanitize(&spent.board, Context::Combat).is_ok());
}

#[test]
fn live_and_replay_do_not_use_private_relic_latches() {
    let mut raw = base(Context::Combat);
    for (id, counter) in [
        ("PERMAFROST", json!(0)),
        ("Permafrost", json!(1)),
        ("RELIC.PERMAFROST", Value::Null),
    ] {
        raw["run_state"]["relics"] = json!([{"id":id,"counter":counter}]);
        assert_eq!(
            observation::sanitize_live(&raw, Context::Combat)
                .err()
                .unwrap()
                .code,
            "relic_public_state_missing"
        );
        assert_eq!(
            sanitize(&raw, Context::Combat).err().unwrap().code,
            "relic_public_state_missing"
        );
    }

    let mut reward = base(Context::CardReward);
    reward["run_state"]["relics"] = json!([
        {"id":"PERMAFROST","counter":0},
        {"id":"PEN_NIB","counter":4}
    ]);
    let before = observation::sanitize_live(&reward, Context::CardReward).unwrap();
    reward["run_state"]["relics"][0]["counter"] = json!(1);
    let after = observation::sanitize_live(&reward, Context::CardReward).unwrap();
    assert_eq!(before.board, after.board);
    assert_eq!(after.board["run_state"]["relics"][0]["id"], "PERMAFROST");
    assert!(
        after.board["run_state"]["relics"][0]
            .get("counter")
            .is_none()
    );
    assert_eq!(after.board["run_state"]["relics"][1]["counter"], 4);
}

fn relic_witness_cases() -> [(&'static str, &'static str, Value, Value, i32); 6] {
    [
        (
            "VAMBRACE",
            "observer_relic_status",
            json!("active"),
            json!("normal"),
            1,
        ),
        (
            "UNSETTLING_LAMP",
            "observer_relic_status",
            json!("active"),
            json!("normal"),
            2,
        ),
        (
            "THROWING_AXE",
            "observer_relic_status",
            json!("active"),
            json!("normal"),
            1,
        ),
        (
            "LIZARD_TAIL",
            "observer_relic_status",
            json!("normal"),
            json!("disabled"),
            1,
        ),
        (
            "PERMAFROST",
            "observer_trigger_seen",
            json!(false),
            json!(true),
            1,
        ),
        (
            "CENTENNIAL_PUZZLE",
            "observer_trigger_seen",
            json!(false),
            json!(true),
            1,
        ),
    ]
}

#[test]
fn live_relic_transitions_depend_only_on_current_public_witness() {
    let _scope = sts2_e2e::build::Scope::enter(model::BUILD);
    for (id, field, ready, spent, spent_counter) in relic_witness_cases() {
        let mut raw = base(Context::Combat);
        let mut relic = json!({"id":id, "counter":999, "status":"normal"});
        relic[field] = ready.clone();
        raw["run_state"]["relics"] = json!([relic]);
        assert!(
            observation::sanitize_live(&raw, Context::Combat).is_err(),
            "{id}"
        );
        raw["capabilities"]["spiremind_relic_witness"] = json!(1);
        for (index, (witness, expected)) in [(ready.clone(), 0), (spent, spent_counter), (ready, 0)]
            .into_iter()
            .enumerate()
        {
            if id == "LIZARD_TAIL" && index == 2 {
                break;
            }
            raw["state_id"] = json!(index + 1);
            raw["run_state"]["relics"][0][field] = witness;
            let mut reconnected = Session::new(index as u64 + 10);
            let position = pos(reconnected.accept(&raw).unwrap());
            let obs = reconnected.observe(&raw, position).unwrap();
            assert!(obs.board["run_state"]["relics"][0].get("counter").is_none());
            assert_eq!(
                obs.board,
                sanitize(&obs.board, Context::Combat).unwrap().board
            );
            let rebuilt = engine::reconstruct(&obs).unwrap().combat.unwrap();
            assert_eq!(
                rebuilt.relics.iter().next().unwrap().counter,
                expected,
                "{id}"
            );
        }
        raw["run_state"]["relics"][0][field] = Value::Null;
        assert!(
            observation::sanitize_live(&raw, Context::Combat).is_err(),
            "{id}"
        );
    }
}

#[test]
#[ignore = "requires the pinned Instinct weights"]
fn model_smoke_public_relic_witness_parity() {
    use sts2_e2e::build::Scope;
    use sts2_wire::emit;
    let _scope = Scope::enter(model::BUILD);
    let net = model::load(&model::default_weights()).unwrap();
    for (id, field, ready, spent, used) in relic_witness_cases() {
        for (public, counter) in [(ready, 0), (spent, used)] {
            let mut raw = base(Context::Combat);
            let mut relic = json!({"id":id, "counter":999});
            relic[field] = public;
            raw["capabilities"]["spiremind_relic_witness"] = json!(1);
            raw["run_state"]["relics"] = json!([relic]);
            let obs = observation::sanitize_live(&raw, Context::Combat).unwrap();
            let mut state = engine::reconstruct(&obs).unwrap();
            let actions = emit::legal_actions(state.combat.as_ref().unwrap());
            let mut oracle = state.clone();
            oracle
                .combat
                .as_mut()
                .unwrap()
                .relics
                .iter_mut()
                .next()
                .unwrap()
                .counter = counter;
            assert_eq!(
                actions,
                emit::legal_actions(oracle.combat.as_ref().unwrap())
            );
            let actual = engine::checked_decision(&obs, &mut state, &actions).unwrap();
            let expected = engine::checked_decision(&obs, &mut oracle, &actions).unwrap();
            assert_eq!(actual.classes, expected.classes, "{id}");
            assert_eq!(actual.members, expected.members, "{id}");
            let a = model::observed_combat_row(&net, &actual, &state).unwrap();
            let b = model::observed_combat_row(&net, &expected, &oracle).unwrap();
            assert!(a == b, "{id}");
            assert_eq!(
                net.try_logits(&a).unwrap(),
                net.try_logits(&b).unwrap(),
                "{id}"
            );
        }
    }
}

#[test]
fn sword_of_stone_counter_has_same_polarity_in_run_and_combat() {
    let _scope = sts2_e2e::build::Scope::enter(model::BUILD);
    let mut raw = base(Context::Combat);
    raw["run_state"]["relics"] = json!([{"id":"SWORD_OF_STONE","counter":2}]);
    let obs = observation::sanitize_live(&raw, Context::Combat).unwrap();
    let state = engine::reconstruct(&obs).unwrap();
    assert_eq!(
        state.run_state.relic_counters[&sts2_core::RelicId::SwordOfStone],
        3
    );
    assert_eq!(
        state.combat.unwrap().relics.iter().next().unwrap().counter,
        3
    );
    for context in [Context::CardReward, Context::Map] {
        let mut raw = base(context);
        raw["run_state"]["relics"] = json!([{"id":"SWORD_OF_STONE","counter":2}]);
        let obs = observation::sanitize_live(&raw, context).unwrap();
        let state = engine::reconstruct(&obs).unwrap();
        assert_eq!(
            state.run_state.relic_counters[&sts2_core::RelicId::SwordOfStone],
            3
        );
    }
}

#[test]
fn imbalanced_accepts_only_rock_and_derives_dizzy_from_current_stun() {
    let _scope = sts2_e2e::build::Scope::enter(model::BUILD);
    let mut raw = base(Context::Combat);
    raw["enemies"][0]["powers"] = json!([{"id":"IMBALANCED","name":"Imbalanced","amount":1,
        "display_amount":1,"observer_visible":true}]);
    assert!(observation::sanitize_live(&raw, Context::Combat).is_err());
    for id in ["BOWLBUG_ROCK", "BowlbugRock"] {
        raw["enemies"][0]["id"] = json!(id);
        raw["enemies"][0]["intents"] = json!([{"kind":"StunIntent","intent_type":"Stun"}]);
        let obs = observation::sanitize_live(&raw, Context::Combat).unwrap();
        let (wire, warnings) = engine::combat_wire(&obs.board, 0).unwrap();
        assert!(warnings.is_empty());
        assert_eq!(wire["encounter"][0]["move_state"], "DIZZY");
    }
}

#[test]
fn live_power_tooltip_requires_explicit_public_provenance() {
    let mut raw = base(Context::Combat);
    raw["player"]["powers"] = json!([
        {"id":"CRIMSON_MANTLE","name":"Crimson Mantle","amount":17,"display_amount":17,
         "observer_visible":true,"vars":{"SelfDamage":99}},
        {"id":"SELF_FORMING_CLAY","name":"Self-Forming Clay","amount":6,"display_amount":6,"observer_visible":true},
        {"id":"PYRE","name":"Pyre","amount":2,"display_amount":2,"observer_visible":true},
        {"id":"FREE_ATTACK","name":"Free Attack","amount":2,"display_amount":2,"observer_visible":true}
    ]);
    assert!(observation::sanitize_live(&raw, Context::Combat).is_err());
    raw["player"]["powers"][0]["observer_self_damage"] = json!(2);
    assert!(observation::sanitize_live(&raw, Context::Combat).is_err());
    raw["capabilities"]["spiremind_power_tooltip"] = json!(1);
    let obs = observation::sanitize_live(&raw, Context::Combat).unwrap();
    assert!(obs.board["player"]["powers"][0].get("vars").is_none());
    let _scope = sts2_e2e::build::Scope::enter(model::BUILD);
    let (wire, warnings) = engine::combat_wire(&obs.board, 0).unwrap();
    assert!(warnings.is_empty());
    assert_eq!(wire["player"]["powers"][0]["counter"], 2);
}

#[test]
fn targeted_potion_requires_current_observed_legal_enemy_indices() {
    let mut raw = base(Context::Combat);
    raw["capabilities"]["spiremind_potion_targets"] = json!(1);
    raw["potions"][0] = json!({"slot":0,"empty":false,"id":"FIRE_POTION","name":"Fire Potion",
        "usage":"CombatOnly","can_use":true,"target_type":"AnyEnemy","requires_target":true,
        "valid_target_indices":[0,2]});
    raw["run_state"]["potions"] = raw["potions"].clone();
    raw["available_actions"] = json!(["PLAY", "END_TURN", "POTION"]);
    let _scope = sts2_e2e::build::Scope::enter(model::BUILD);
    let obs = observation::sanitize_live(&raw, Context::Combat).unwrap();
    let rm = engine::reconstruct(&obs).unwrap();
    let (_, actions) = engine::checked_actions(&obs, &rm).unwrap();
    let targets = actions
        .iter()
        .filter(|a| a["action"]["kind"] == "potion")
        .map(|a| a["action"]["target"]["combat_id"].clone())
        .collect::<Vec<_>>();
    assert_eq!(targets, vec![json!(91), json!(93)]);
    for bad in [json!([0]), json!([0, 1, 2]), json!([0, 0, 2]), Value::Null] {
        raw["potions"][0]["valid_target_indices"] = bad;
        raw["run_state"]["potions"] = raw["potions"].clone();
        let obs = observation::sanitize_live(&raw, Context::Combat).unwrap();
        assert!(engine::checked_actions(&obs, &rm).is_err());
    }
}

#[test]
fn simulator_compatibility_structural_suite() {
    let result = super::compatibility::run(None, 1, 95).unwrap();
    assert_eq!(result["campaign"]["source"], "simulator_derived");
    assert_eq!(result["campaign"]["live_validate_count"], 0);
    assert_eq!(
        result["campaign"]["parity_checks"]["same_public_projection_after_redeal"],
        8
    );
    assert!(result["campaign"]["unique"].as_u64().unwrap() >= 20);
    assert!(
        result["campaign"]["outcomes"]["structural_ok"]
            .as_u64()
            .unwrap()
            > 0
    );
    let targeted = &result["configured_fights"]["CORPSE_SLUGS_NORMAL"];
    assert!(targeted["multiple_enemies"].as_u64().unwrap() > 0);
    assert!(targeted["modified_cards"].as_u64().unwrap() > 0);
    assert_eq!(
        targeted["parity_checks"]["same_public_projection_after_redeal"],
        8
    );
}

#[test]
fn matrix_uses_the_model_build_registry() {
    let matrix = super::compatibility::power_matrix();
    let ids: std::collections::BTreeSet<_> = matrix["powers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["id"].as_str().unwrap())
        .collect();
    let upstream: std::collections::BTreeSet<_> = sts2_core::PowerKind::all_for(model::BUILD)
        .iter()
        .map(|kind| kind.python_name())
        .collect();
    assert_eq!(ids, upstream);
    assert_eq!(matrix["powers"].as_array().unwrap().len(), upstream.len());
    assert!(ids.contains("AMBERGRIS"));
    assert!(!ids.contains("AIM") && !ids.contains("OUTBREAK"));
}

#[test]
#[ignore = "requires the pinned Instinct weights"]
fn compatibility_model_smoke_simulator_policy_parity() {
    let net = model::load(&model::default_weights()).unwrap();
    let result = super::compatibility::run(Some(&net), 1, 95).unwrap();
    let campaign = &result["campaign"];
    assert!(
        campaign["parity_checks"]["exact_masked_tokens"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert!(
        campaign["parity_checks"]["logits_within_1e_5"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert!(campaign["outcomes"]["ok"].as_u64().unwrap() > 0);
    assert_eq!(campaign["live_validate_count"], 0);
}

#[test]
fn synthetic_schema_and_game_gates() {
    for (path, value, code) in [
        (
            "/game_version",
            json!("v0.111.2"),
            "incompatible_game_version",
        ),
        (
            "/game_version",
            json!("v0.113.0"),
            "incompatible_game_version",
        ),
        ("/mod_version", json!("0.11.9"), "schema_mismatch"),
        ("/player_count", json!(2), "unsupported_run"),
        ("/run_state/game_mode", json!("blitz"), "unsupported_run"),
        ("/run_state/character", json!("SILENT"), "unsupported_run"),
        ("/run_state/ascension", json!(3), "unsupported_ascension"),
        ("/can_skip", Value::Null, "schema_mismatch"),
        ("/cards/0/upgrade_level", json!(7), "schema_mismatch"),
    ] {
        let mut b = base(Context::CardReward);
        *b.pointer_mut(path).unwrap() = value;
        assert_eq!(
            sanitize(&b, Context::CardReward).err().unwrap().code,
            code,
            "{path}"
        );
    }
    let mut b = base(Context::CardReward);
    b["run_state"]["surprise"] = json!(1);
    assert_eq!(
        sanitize(&b, Context::CardReward).err().unwrap().code,
        "unknown_field"
    );
    b["advice_for"] = json!("event");
    assert_eq!(
        observation::context(&b).unwrap_err().code,
        "unsupported_state"
    );
}

fn pos(event: Event) -> Position {
    if let Event::Observation(p) = event {
        p
    } else {
        panic!("expected advisor observation")
    }
}

#[test]
fn synthetic_map_correlation_and_reconnect_are_fail_closed() {
    let mut s = Session::new(10);
    let map = base(Context::Map);
    let p = pos(s.accept(&map).unwrap());
    assert!(s.observe(&map, p).is_ok());
    assert!(matches!(s.accept(&map).unwrap(), Event::Ignore));
    let mut old = map.clone();
    old["state_id"] = json!(10);
    assert_eq!(s.accept(&old).err().unwrap().code, "schema_mismatch");
    assert!(matches!(
        s.accept(&json!({"type":"snapshot_saved","snapshot_id":"discarded"}))
            .unwrap(),
        Event::Invalidate("snapshot_saved")
    ));
    let acted = json!({"type":"human_action","decision_type":"map_select","advice_request_id":"a4","position_id":11,"choice":{"index":1}});
    assert!(matches!(
        s.accept(&acted).unwrap(),
        Event::Invalidate("player_action")
    ));
    let mut fight = base(Context::Combat);
    fight["request_id"] = json!("a5");
    fight["state_id"] = json!(13);
    fight.as_object_mut().unwrap().remove("map");
    let p = pos(s.accept(&fight).unwrap());
    assert_eq!(
        s.observe(&fight, p).unwrap().board["map"]["current"],
        json!({"col":4,"row":1})
    );
    let mut new = Session::new(11);
    let p = pos(new.accept(&fight).unwrap());
    assert_eq!(
        new.observe(&fight, p).err().unwrap().code,
        "missing_visible_map"
    );
    assert!(matches!(
        s.accept(&json!({"type":"game_over"})).unwrap(),
        Event::Invalidate("run_boundary")
    ));
    assert_eq!(
        s.observe(&fight, p).err().unwrap().code,
        "missing_visible_map"
    );
}

#[test]
fn synthetic_latest_state_gate_handles_races_and_ttl() {
    let p = |session, state_id| Position {
        session,
        state_id,
        request_id: state_id,
    };
    let mut latest = Latest::default();
    let received = Instant::now();
    let ttl = Duration::from_millis(500);
    latest.new_position(p(1, 1), received);
    assert!(latest.valid(p(1, 1), received, ttl));
    latest.new_position(p(1, 2), received);
    assert!(!latest.valid(p(1, 1), received, ttl));
    assert!(!latest.valid(p(1, 2), received + ttl, ttl));
    latest.new_position(p(2, 2), received);
    assert!(!latest.valid(p(1, 2), received, ttl));
    assert!(latest.valid(p(2, 2), received, ttl));
}

#[test]
fn synthetic_position_lease_lasts_ten_seconds_and_cannot_revive() {
    let mut session = Session::new(7);
    let mut latest = Latest::default();
    let observation = base(Context::Map);
    let position = pos(session.accept(&observation).unwrap());
    let at = Instant::now();
    let ttl = Duration::from_millis(1500);
    latest.new_position(position, at);
    assert!(!latest.confirmed);
    let confirm = |seq, state_id| json!({"type":"spiremind_confirm","request_id":"a4","state_id":state_id,"lease_seq":seq});
    for seq in 1..=22 {
        let Event::Confirm(p, n) = session.accept(&confirm(seq, 11)).unwrap() else {
            panic!("fresh confirmation expected");
        };
        assert_eq!(n, seq);
        assert!(latest.confirm(p, at + Duration::from_millis(500 * n), ttl));
        assert!(latest.pending.is_none());
    }
    assert!(latest.confirmed);
    assert!(latest.valid(position, at + Duration::from_secs(11), ttl));
    assert!(matches!(
        session.accept(&confirm(22, 11)).unwrap(),
        Event::Ignore
    ));
    assert!(matches!(
        session.accept(&confirm(23, 10)).unwrap(),
        Event::Ignore
    ));
    assert!(matches!(
        session.accept(&json!({"type":"pong"})).unwrap(),
        Event::Ignore
    ));
    assert!(!latest.confirm(position, at + Duration::from_millis(12500), ttl));
    let close = json!({"type":"spiremind_withdraw","request_id":"a4","state_id":11});
    assert!(matches!(session.accept(&close).unwrap(), Event::Withdraw(p) if p == position));
    assert!(matches!(
        Session::new(8).accept(&confirm(24, 11)).unwrap(),
        Event::Ignore
    ));
    assert!(!Latest::default().confirm(position, at, ttl));
}

#[test]
fn synthetic_live_requires_local_build_manifest() {
    assert!(crate::Cli::try_parse_from(["spire", "live"]).is_err());
    assert!(crate::Cli::try_parse_from(["spire", "live", "--game-version", "v0.111.1"]).is_err());
    assert!(
        crate::Cli::try_parse_from(["spire", "live", "--release-info", "release_info.json"])
            .is_ok()
    );
}

#[test]
fn synthetic_combat_recovers_only_from_a_confirmed_visible_map() {
    let mut fight = base(Context::Combat);
    let mut session = Session::new(12);
    let p = pos(session.accept(&fight).unwrap());
    assert!(session.observe(&fight, p).is_ok());
    fight["map"]["current"] = json!({"col":4,"row":1});
    assert_eq!(
        session.observe(&fight, p).err().unwrap().code,
        "missing_visible_map"
    );
}

#[test]
fn synthetic_bounded_frame_reader_handles_fragments_and_eof() {
    let mut input = Cursor::new(b"abc\r\ndef\nrest");
    let mut reader = Frames::default();
    assert!(matches!(reader.read(&mut input).unwrap(),Receive::Frame(v,_) if v==b"abc"));
    assert!(matches!(reader.read(&mut input).unwrap(),Receive::Frame(v,_) if v==b"def"));
    assert_eq!(
        reader.read(&mut input).err().unwrap().kind(),
        std::io::ErrorKind::UnexpectedEof
    );
    let mut huge = Cursor::new(vec![b'x'; MAX_FRAME + 1]);
    assert_eq!(
        Frames::default().read(&mut huge).err().unwrap().kind(),
        std::io::ErrorKind::InvalidData
    );
}

#[test]
fn synthetic_transport_writes_only_advice_ack_and_refuses_autopilot() {
    struct Duplex {
        input: Cursor<Vec<u8>>,
        output: Vec<u8>,
    }
    impl Read for Duplex {
        fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
            self.input.read(out)
        }
    }
    impl Write for Duplex {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.output.write(bytes)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut live = Live {
        native_current: false,
        latest: Mutex::new(Latest::default()),
        cache: Mutex::new(None),
        blocked_corpus: Mutex::new(None),
        wake: Condvar::new(),
        terminal: Terminal { json: true },
        ttl: Duration::from_secs(1),
    };
    let mut duplex = Duplex {
        input: Cursor::new(b"{\"type\":\"combat_action\",\"request_id\":\"3\"}\n".to_vec()),
        output: Vec::new(),
    };
    assert_eq!(
        live.serve(&mut duplex, 1, "v0.111.1").unwrap_err().code,
        "non_advisor_frame"
    );
    assert!(duplex.output.is_empty());
    let mut advisor = base(Context::Map);
    advisor.as_object_mut().unwrap().remove("capabilities");
    duplex.input = Cursor::new(format!("{advisor}\n").into_bytes());
    assert_eq!(
        live.serve(&mut duplex, 1, "v0.111.1").unwrap_err().code,
        "lease_not_supported"
    );
    assert!(duplex.output.is_empty());
    advisor["capabilities"] = json!({"spiremind_lease":1});
    duplex.input = Cursor::new(format!("{advisor}\n").into_bytes());
    assert_eq!(
        live.serve(&mut duplex, 1, "v0.111.1").unwrap_err().code,
        "power_visibility_not_supported"
    );
    assert!(duplex.output.is_empty());
    advisor["capabilities"] = json!({"spiremind_lease":1,"spiremind_power_visibility":1});
    advisor["advice_for"] = json!("shop");
    duplex.input = Cursor::new(format!("{advisor}\n").into_bytes());
    live.serve(&mut duplex, 1, "v0.111.1").unwrap();
    let reply: Value = serde_json::from_slice(&duplex.output).unwrap();
    let supported = json!({"coach":2,"instinct":[{"character":"IRONCLAD","game_version":"v0.111.0"},{"character":"IRONCLAD","game_version":"v0.111.1"}],"spiremind_lease":1});
    assert_eq!(
        reply,
        json!({"action":"advice","request_id":"a4","state_id":11,"advice":{"hand_back":"SpireMind terminal only"},"capabilities":supported})
    );
    let ack = serde_json::to_value(Ack::for_position(Position {
        session: 1,
        state_id: 42,
        request_id: 3,
    }))
    .unwrap();
    assert_eq!(
        ack,
        json!({"action":"advice","request_id":"a3","state_id":42,"advice":{"hand_back":"SpireMind terminal only"},"capabilities":supported})
    );
    advisor["advice_for"] = json!("map_select");
    duplex.output.clear();
    duplex.input = Cursor::new(format!("{advisor}\n{{\"type\":\"spiremind_confirm\",\"request_id\":\"a4\",\"state_id\":11,\"lease_seq\":1}}\n{{\"type\":\"spiremind_withdraw\",\"request_id\":\"a4\",\"state_id\":11}}\n").into_bytes());
    live.serve(&mut duplex, 2, "v0.111.1").unwrap();
    assert_eq!(
        duplex.output.iter().filter(|&&byte| byte == b'\n').count(),
        1
    );
    let latest = live.latest.lock().unwrap();
    assert!(latest.position.is_none() && latest.pending.is_none() && !latest.confirmed);
    drop(latest);
    let native = native_board();
    duplex.output.clear();
    duplex.input = Cursor::new(format!("{native}\n").into_bytes());
    assert_eq!(
        live.serve(&mut duplex, 3, "v0.111.1").unwrap_err().code,
        "producer_profile"
    );
    assert!(duplex.output.is_empty());
    live.native_current = true;
    duplex.input = Cursor::new(format!("{advisor}\n").into_bytes());
    assert_eq!(
        live.serve(&mut duplex, 3, "v0.111.1").unwrap_err().code,
        "producer_profile"
    );
    assert!(duplex.output.is_empty());
    duplex.input = Cursor::new(format!("{native}\n").into_bytes());
    live.serve(&mut duplex, 3, "v0.111.1").unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&duplex.output).unwrap(),
        reply
    );
    let latest = live.latest.lock().unwrap();
    let admitted = &latest.pending.as_ref().unwrap().observation.board;
    assert_eq!(admitted["native_current"], 1);
    assert!(admitted.get("rng").is_none());
}

#[test]
fn synthetic_cancelled_result_never_enters_reuse_cache() {
    let live = Live {
        native_current: false,
        latest: Mutex::new(Latest::default()),
        cache: Mutex::new(None),
        blocked_corpus: Mutex::new(None),
        wake: Condvar::new(),
        terminal: Terminal { json: true },
        ttl: Duration::from_secs(1),
    };
    let now = Instant::now();
    let mut latest = Latest::default();
    let p = Position {
        session: 1,
        state_id: 1,
        request_id: 1,
    };
    latest.new_position(p, now);
    latest.completed = Some((
        now,
        json!({"status":"ok","observation_id":"test","reused":false,"forward_ms":1}),
    ));
    live.publish(&mut latest, now);
    assert!(live.cache.lock().unwrap().is_none());
    live.clear(&mut latest, "producer_withdraw");
    assert!(!latest.confirm(p, now, live.ttl));
    live.publish(&mut latest, now);
    assert!(live.cache.lock().unwrap().is_none());
    let p = Position {
        state_id: 2,
        request_id: 2,
        ..p
    };
    latest.new_position(p, now);
    latest.completed = Some((
        now,
        json!({"status":"ok","observation_id":"test","reused":false,"forward_ms":1}),
    ));
    assert!(latest.confirm(p, now, live.ttl));
    live.publish(&mut latest, now);
    let cache = live.cache.lock().unwrap();
    let saved = cache.as_ref().unwrap();
    assert_eq!(saved.observation_id, "test");
    assert!(saved.result.get("forward_ms").is_none());
}

#[test]
#[ignore = "requires official Instinct weights; the Advisor-shaped observations are synthetic"]
fn live_model_smoke_synthetic_wire_real_instinct() {
    let net = model::load(&model::default_weights()).unwrap();
    let mut compound = base(Context::Combat);
    compound["enemies"][0]["intents"] = json!([
        {"kind":"SingleAttackIntent", "intent_type":"Attack", "damage":6, "hits":1},
        {"kind":"DefendIntent", "intent_type":"Defend"}
    ]);
    let report = engine::advise(&net, &sanitize(&compound, Context::Combat).unwrap()).unwrap();
    assert_eq!(report["status"], "ok");
    assert_eq!(report["legal_actions"].as_array().unwrap().len(), 5);
    compound["enemies"][0]["intents"][1]["intent_type"] = json!("Heal");
    assert_eq!(
        engine::advise(&net, &sanitize(&compound, Context::Combat).unwrap())
            .unwrap_err()
            .code,
        "reconstruction"
    );

    let mut entrance = base(Context::Map);
    entrance["act"] = json!(2);
    entrance["floor"] = json!(17);
    entrance["current_room"] = json!("Map");
    entrance["map"]["current"] = Value::Null;
    entrance["map"]["nodes"].as_array_mut().unwrap().remove(0);
    entrance["nodes"] = json!([{"col":3,"row":0,"type":"Ancient","index":0}]);
    let forced = engine::advise(&net, &sanitize(&entrance, Context::Map).unwrap()).unwrap();
    assert_eq!(forced["status"], "forced");
    assert_eq!(forced["legal_actions"][0]["action"]["room_type"], "Ancient");
    for ascension in [0, 10] {
        for c in [Context::Combat, Context::CardReward, Context::Map] {
            let mut b = base(c);
            b["run_state"]["ascension"] = json!(ascension);
            let o = sanitize(&b, c).unwrap();
            let result =
                engine::advise(&net, &o).unwrap_or_else(|e| panic!("{c:?} A{ascension}: {e:?}"));
            assert_eq!(result["status"], "ok");
            let legal = result["legal_actions"].as_array().unwrap();
            match c {
                Context::Combat => {
                    assert_eq!(legal.len(), 5);
                    assert!(
                        legal
                            .iter()
                            .any(|a| a["action"]["target"]["combat_id"] == 93)
                    );
                    assert!(
                        !legal
                            .iter()
                            .any(|a| a["action"]["target"]["combat_id"] == 92)
                    );
                    assert!(
                        legal
                            .iter()
                            .any(|a| a["action"]["card"]["effective_cost"] == 0)
                    );
                }
                Context::CardReward => assert!(legal.iter().any(|a| a["action"]["kind"] == "skip")),
                Context::Map => assert_eq!(legal.len(), 2),
            }
            assert!(result["forward_ms"].as_f64().unwrap() > 0.0);
            println!(
                "Synthetic {c:?} A{ascension}, real forward {:.3} ms",
                result["forward_ms"].as_f64().unwrap()
            );
        }
    }
    let mut forced = base(Context::CardReward);
    forced["can_skip"] = json!(false);
    forced["cards"].as_array_mut().unwrap().truncate(1);
    let result = engine::advise(&net, &sanitize(&forced, Context::CardReward).unwrap()).unwrap();
    assert_eq!(result["status"], "forced");
    assert!(result.get("forward_ms").is_none() && result.get("recommendations").is_none());
    let mut mismatch = base(Context::Combat);
    mismatch["hand"][0]["valid_target_indices"] = json!([0]);
    assert_eq!(
        engine::advise(&net, &sanitize(&mismatch, Context::Combat).unwrap())
            .err()
            .unwrap()
            .code,
        "action_coverage"
    );
    let mut target = base(Context::Combat);
    target["hand"][0]["valid_target_indices"] = json!([0, 1]);
    assert_eq!(
        engine::advise(&net, &sanitize(&target, Context::Combat).unwrap())
            .err()
            .unwrap()
            .code,
        "action_coverage"
    );
    let correct = base(Context::Combat);
    let mut hidden = correct.clone();
    hidden["seed"] = json!("changed");
    hidden["draw_pile"]["cards"]
        .as_array_mut()
        .unwrap()
        .reverse();
    hidden["enemies"][0]["forecast"] = json!("changed");
    let infer = |board: &Value| {
        let mut result = engine::advise(&net, &sanitize(board, Context::Combat).unwrap()).unwrap();
        result.as_object_mut().unwrap().remove("forward_ms");
        result
    };
    assert_eq!(infer(&correct), infer(&hidden));
    let mut potion = base(Context::Combat);
    potion["potions"][0] = json!({"slot":0,"empty":false,"id":"SKILL_POTION","name":"Skill Potion","usage":"CombatOnly","target_type":"AnyPlayer","requires_target":false,"can_use":true});
    potion["run_state"]["potions"] = potion["potions"].clone();
    potion["available_actions"] = json!(["PLAY", "END_TURN", "POTION"]);
    let result = engine::advise(&net, &sanitize(&potion, Context::Combat).unwrap()).unwrap();
    assert!(
        result["legal_actions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| {
                row["action"]["kind"] == "potion" && row["action"]["id"] == "SKILL_POTION"
            })
    );
    potion["potions"][0]["requires_target"] = json!(true);
    potion["run_state"]["potions"] = potion["potions"].clone();
    assert_eq!(
        engine::advise(&net, &sanitize(&potion, Context::Combat).unwrap())
            .unwrap_err()
            .code,
        "action_coverage"
    );
    let mut entrance = base(Context::Map);
    entrance["map"]["current"] = Value::Null;
    assert_eq!(
        engine::advise(&net, &sanitize(&entrance, Context::Map).unwrap()).unwrap()["status"],
        "ok"
    );
    entrance["map"]["nodes"].as_array_mut().unwrap().remove(0);
    entrance["nodes"] = json!([{"index":0,"col":3,"row":0,"type":"Monster"}]);
    let result = engine::advise(&net, &sanitize(&entrance, Context::Map).unwrap()).unwrap();
    assert_eq!(result["status"], "forced");
    assert_eq!(result["legal_actions"][0]["action"]["row"], 0);
    assert!(result.get("forward_ms").is_none());
    entrance["nodes"][0]["col"] = json!(2);
    assert!(engine::advise(&net, &sanitize(&entrance, Context::Map).unwrap()).is_err());
}
