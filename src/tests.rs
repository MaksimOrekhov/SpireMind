use std::path::PathBuf;

use serde_json::Value;
use sts2_e2e::{
    build::Scope,
    env::{Driver, E2eEnv, Next, Step},
    net::{PolicyRow, argmax},
    schema::{N_CAT, N_CONT, xc, xf},
    tokens::{TokCache, encode_for},
};

use crate::{
    contract::{self, Context},
    model::{self, BUILD},
    simulation::{self, FIRST_SEED},
};

#[test]
fn synthetic_classes_keep_each_action_and_break_ties_by_class_id() {
    let ranked = contract::rank(
        &[0, 0, 1, 2],
        &[vec![0, 1], vec![2], vec![3]],
        &[1., 1., 2.],
    )
    .unwrap();
    assert_eq!(
        ranked.iter().map(|r| r.class_id).collect::<Vec<_>>(),
        [2, 0, 1]
    );
    assert_eq!(ranked[1].action_ids, [0, 1]);
    assert_eq!(ranked[1].representative_action_id, 0);
    assert!((ranked.iter().map(|r| r.policy_probability).sum::<f64>() - 1.).abs() < 1e-14);
}

#[test]
fn synthetic_malformed_policy_responses_are_rejected() {
    for logits in [
        vec![],
        vec![1.],
        vec![1., f32::NAN],
        vec![f32::INFINITY, 0.],
    ] {
        assert_eq!(
            contract::rank(&[0, 1], &[vec![0], vec![1]], &logits)
                .unwrap_err()
                .code,
            "malformed_upstream_response"
        );
    }
    for members in [
        vec![vec![0, 0], vec![1]],
        vec![vec![0], vec![2]],
        vec![vec![0], vec![]],
    ] {
        assert_eq!(
            contract::rank(&[0, 1], &members, &[0., 1.])
                .unwrap_err()
                .code,
            "malformed_upstream_response"
        );
    }
    assert!(contract::rank(&[0, 1, 1], &[vec![0], vec![1]], &[0., 1.]).is_err());
}

#[test]
fn synthetic_unsupported_state_and_build_are_rejected() {
    assert_eq!(
        Context::from_dtype(sts2_e2e::contracts::dt::COMBAT_CHOICE)
            .unwrap_err()
            .code,
        "unsupported_state"
    );
    assert_eq!(
        model::check_build("v0.111.1").unwrap_err().code,
        "incompatible_game_version"
    );
    assert!(model::check_build("v0.111.0").is_ok());
}

#[test]
fn synthetic_missing_weights_are_reported() {
    assert_eq!(
        model::load(&model::default_weights().with_file_name("missing-test-file.e2ew"))
            .err()
            .unwrap()
            .code,
        "missing_weights"
    );
}

#[test]
fn synthetic_wrong_size_weights_are_rejected() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    assert_eq!(model::load(&path).err().unwrap().code, "weights_integrity");
}

#[test]
fn simulator_legal_map_actions_match_connected_start_nodes() {
    let _scope = Scope::enter(BUILD);
    let mut env = E2eEnv::new(FIRST_SEED);
    let mut step = env.reset();
    let mut found = false;
    for _ in 0..8 {
        let Step::Decision(d) = step else { break };
        if Context::from_dtype(d.dtype).ok() == Some(Context::Map) {
            let map = env.rm().run_state.current_act_map.as_ref().unwrap();
            let start = map.point(map.start.unwrap());
            let mut reachable: Vec<_> = map
                .children_of(start.coord)
                .into_iter()
                .filter(|node| map.grid.contains_key(&node.coord))
                .map(|node| (node.coord.row, node.col()))
                .collect();
            reachable.sort_unstable();
            let mapped: Vec<_> = contract::legal(&d, env.rm(), Context::Map)
                .unwrap()
                .iter()
                .map(|action| {
                    let node = &action["detail"]["node"];
                    (
                        node["row"].as_i64().unwrap() as i32,
                        node["col"].as_i64().unwrap() as i32,
                    )
                })
                .collect();
            assert_eq!(mapped, reachable);
            assert!(mapped.len() > 1);
            let mut broken = *d;
            broken.actions.pop();
            assert_eq!(
                contract::legal(&broken, env.rm(), Context::Map)
                    .unwrap_err()
                    .code,
                "malformed_upstream_response"
            );
            found = true;
            break;
        }
        step = env.step(0);
    }
    assert!(found, "the real seeded simulator must reach a map decision");
}

#[test]
#[ignore = "requires the official Instinct weights downloaded by scripts/setup.py"]
fn model_smoke_real_inference_and_observation_boundary() {
    let net = model::load(&model::default_weights()).unwrap();
    let _scope = Scope::enter(BUILD);
    let seed = FIRST_SEED + 8;
    let mut env = E2eEnv::new(seed);
    let mut cache = TokCache::new();
    let mut step = env.reset();
    let mut checked = Vec::new();
    for _ in 0..100 {
        let Step::Decision(d) = step else {
            panic!("simulation ended before checking all contexts")
        };
        let next_cache = cache.clone();
        let (logits, _) = model::infer(&net, &d, env.rm(), &mut cache).unwrap();
        if let Ok(context) = Context::from_dtype(d.dtype)
            && !checked.contains(&context)
        {
            let raw = encode_for(&net, &d, env.rm(), &mut next_cache.clone()).unwrap();
            let visible =
                model::policy_tokens(&net, &d, env.rm(), &mut next_cache.clone()).unwrap();
            let reference = net.act_greedy(&PolicyRow::from(visible.clone()));
            let original = net.act_greedy(&PolicyRow::from(raw.clone()));
            let response = simulation::recommend(&net, context, seed, 30).unwrap();
            assert_eq!(response["state"]["decision_index"], d.row);
            let best = &response["recommendations"][0];
            assert_eq!(best["class_id"], reference);
            assert_eq!(best["representative_action_id"], d.members[reference][0]);
            let legal = response["legal_actions"].as_array().unwrap();
            assert_eq!(
                legal.iter().map(|a| &a["upstream"]).collect::<Vec<_>>(),
                d.actions.iter().collect::<Vec<_>>()
            );

            match context {
                Context::Combat => {
                    assert!(legal.iter().any(|a| a["upstream"]["action"] == "end_turn"));
                    assert!(
                        legal
                            .iter()
                            .any(|a| a["detail"]["target"]["enemy_index"] == 2)
                    );
                    let selected = &legal[d.members[reference][0]];
                    assert_eq!(selected["detail"]["card"]["id"], "STRIKE_IRONCLAD");
                    assert_eq!(selected["detail"]["target"]["enemy_index"], 0);
                    let combat = env.rm().combat.as_ref().unwrap();
                    for action in legal.iter().filter(|a| a["upstream"]["action"] == "play") {
                        let hand = action["upstream"]["card_index"].as_u64().unwrap() as usize;
                        assert_eq!(
                            action["detail"]["card"]["instance_id"],
                            combat.player.hand[hand].get()
                        );
                        if let Some(target) = action["upstream"]["target_index"].as_u64() {
                            assert_eq!(
                                action["detail"]["target"]["instance_id"],
                                combat.enemy_uids[target as usize].get()
                            );
                        }
                    }
                    assert_ne!(raw, visible, "the fixture must exercise the forecast mask");
                    assert!(
                        visible
                            .cat
                            .as_chunks::<N_CAT>()
                            .0
                            .iter()
                            .all(|r| r[xc("X_FC1")..=xc("X_FC3")].iter().all(|&x| x == 0))
                    );
                    assert!(
                        visible
                            .cont
                            .as_chunks::<N_CONT>()
                            .0
                            .iter()
                            .all(|r| r[xf("E_F1_CERT")..=xf("E_F3_TOT")].iter().all(|&x| x == 0))
                    );

                    let mut redealt = env.rm().clone();
                    sts2_core::blindfold::blindfold_root(redealt.combat.as_mut().unwrap(), 2026);
                    let redealt_cs = redealt.combat.as_ref().unwrap();
                    assert_ne!(redealt_cs.run_rng, combat.run_rng);
                    assert_ne!(redealt_cs.player.draw, combat.player.draw);
                    let Next::Decision(alternative) = Driver::new(2026).peek(&mut redealt) else {
                        panic!("expected redealt combat decision")
                    };
                    assert_eq!(
                        visible,
                        model::policy_tokens(&net, &alternative, &redealt, &mut next_cache.clone())
                            .unwrap()
                    );
                }
                Context::CardReward | Context::Map => {
                    assert_eq!(raw, visible);
                    assert_eq!(original, reference);
                    let mut alternative = (*d).clone();
                    alternative.ooc.as_mut().unwrap().run_obs["seed"] = Value::from(2026);
                    let mut hidden_rooms = env.rm().clone();
                    for act in &mut hidden_rooms.run_state.generated_rooms {
                        act.normal_encounters.clear();
                        act.elite_encounters.clear();
                        act.events_shuffled.clear();
                    }
                    assert_eq!(
                        visible,
                        model::policy_tokens(
                            &net,
                            &alternative,
                            &hidden_rooms,
                            &mut next_cache.clone()
                        )
                        .unwrap()
                    );
                    if context == Context::CardReward {
                        assert!(legal.iter().any(|action| action["detail"]["skip"] == true));
                    }
                }
            }
            println!(
                "{context:?} row={} actions={} masked_class={reference} upstream_class={original} best={}",
                d.row,
                legal.len(),
                legal[d.members[reference][0]]
            );
            checked.push(context);
            if checked.len() == 3 {
                break;
            }
        }
        step = env.step(argmax(&logits));
    }
    assert_eq!(
        checked.len(),
        3,
        "all three contexts must run with real weights"
    );
}
