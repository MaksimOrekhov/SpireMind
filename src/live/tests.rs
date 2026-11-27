use serde_json::{Value, json};
use std::{
    io::{Cursor, Read, Write},
    sync::{Condvar, Mutex},
    time::{Duration, Instant},
};

use super::{
    Latest, Live, Terminal, engine,
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
    let live = Live {
        latest: Mutex::new(Latest::default()),
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
    duplex.input = Cursor::new(format!("{advisor}\n").into_bytes());
    assert_eq!(
        live.serve(&mut duplex, 1, "v0.111.1").unwrap_err().code,
        "lease_not_supported"
    );
    assert!(duplex.output.is_empty());
    advisor["capabilities"] = json!({"spiremind_lease":1});
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
}

#[test]
#[ignore = "requires official Instinct weights; the Advisor-shaped observations are synthetic"]
fn live_model_smoke_synthetic_wire_real_instinct() {
    let net = model::load(&model::default_weights()).unwrap();
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
