use clap::ValueEnum;
use serde::Serialize;
use serde_json::{Value, json};
use sts2_core::{card_by_uid, creature_by_id, run_manager::RunManager};
use sts2_e2e::{contracts::dt, env::Decision, net::softmax};
use sts2_wire::emit::{legal_actions, run_manager as emit_rm};

use crate::{Failure, Result, model::validate_logits};

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Context {
    Combat,
    CardReward,
    Map,
}

impl Context {
    pub fn from_dtype(dtype: u8) -> Result<Self> {
        match dtype {
            dt::COMBAT_TURN => Ok(Self::Combat),
            dt::CARD_REWARD => Ok(Self::CardReward),
            dt::MAP => Ok(Self::Map),
            _ => Err(Failure::new(
                "unsupported_state",
                format!("Decision type {} is outside SM-01", dt_name(dtype)),
            )),
        }
    }
}

fn dt_name(dtype: u8) -> &'static str {
    sts2_e2e::contracts::dtype_name(dtype)
}

#[derive(Debug, Serialize)]
pub struct RankedClass {
    pub class_id: usize,
    pub representative_action_id: usize,
    pub action_ids: Vec<usize>,
    pub policy_logit: f32,
    pub policy_probability: f64,
}

fn malformed(message: &str) -> Failure {
    Failure::new("malformed_upstream_response", message)
}

pub fn rank(
    class_of_action: &[usize],
    members: &[Vec<usize>],
    logits: &[f32],
) -> Result<Vec<RankedClass>> {
    validate_logits(members.len(), logits)?;
    let mut seen = vec![false; class_of_action.len()];
    let probabilities = softmax(logits);
    let mut ranked = Vec::with_capacity(members.len());
    for (class_id, actions) in members.iter().enumerate() {
        let first = *actions
            .first()
            .ok_or_else(|| malformed("Empty policy class"))?;
        for &i in actions {
            if class_of_action.get(i) != Some(&class_id) || seen[i] {
                return Err(malformed("Policy classes do not partition the action IDs"));
            }
            seen[i] = true;
        }
        ranked.push(RankedClass {
            class_id,
            representative_action_id: first,
            action_ids: actions.clone(),
            policy_logit: logits[class_id],
            policy_probability: probabilities[class_id],
        });
    }
    if seen.contains(&false) {
        return Err(malformed("A legal action lacks a policy class"));
    }
    ranked.sort_by(|a, b| {
        b.policy_logit
            .partial_cmp(&a.policy_logit)
            .unwrap()
            .then(a.class_id.cmp(&b.class_id))
    });
    Ok(ranked)
}

fn index(action: &Value, field: &str) -> Result<usize> {
    action[field]
        .as_u64()
        .and_then(|v| usize::try_from(v).ok())
        .ok_or_else(|| malformed(&format!("Invalid {field} in upstream action")))
}

fn card_action(d: &Decision, rm: &RunManager, raw: &Value) -> Result<Value> {
    let combat = rm
        .combat
        .as_ref()
        .ok_or_else(|| malformed("Missing combat state"))?;
    let mut detail = json!({});
    match raw["action"].as_str() {
        Some("play") => {
            let hand_index = index(raw, "card_index")?;
            let card = combat
                .player
                .hand
                .get(hand_index)
                .and_then(|&uid| card_by_uid(&combat.cards, uid))
                .ok_or_else(|| {
                    malformed("Play action refers to a card outside the current hand")
                })?;
            detail["card"] = json!({"hand_index": hand_index, "instance_id": card.uid.get(),
                "id": card.card_id.to_python_name(), "upgraded": card.upgraded});
        }
        Some("end_turn" | "potion" | "drink_potion") => {}
        _ => return Err(malformed("Unexpected combat turn action")),
    }
    if raw.get("target_index").is_some_and(|v| !v.is_null()) {
        let enemy_index = index(raw, "target_index")?;
        let target = combat
            .enemy_uids
            .get(enemy_index)
            .and_then(|uid| creature_by_id(&combat.creatures, uid.0))
            .ok_or_else(|| malformed("Target index is outside the simulator enemy list"))?;
        detail["target"] = json!({"enemy_index": enemy_index, "instance_id": target.id.get(),
            "id": target.species.map(|kind| kind.python_name())});
    }
    if d.combat.is_none() || raw.get("ally_index").is_some() {
        return Err(malformed("Expected a single-player combat view"));
    }
    Ok(detail)
}

pub fn legal(d: &Decision, rm: &RunManager, context: Context) -> Result<Vec<Value>> {
    let live_actions = match context {
        Context::Combat => legal_actions(
            rm.combat
                .as_ref()
                .ok_or_else(|| malformed("Missing combat state"))?,
        ),
        Context::CardReward | Context::Map => emit_rm::available_actions(rm),
    };
    let live_actions = live_actions
        .as_array()
        .ok_or_else(|| malformed("Expected simulator action array"))?;
    if live_actions != &d.actions || d.actions.len() != d.classes.len() {
        return Err(malformed(
            "Decision actions differ from the current simulator action list",
        ));
    }
    d.actions.iter().enumerate().map(|(id, raw)| {
        let detail = match context {
            Context::Combat => card_action(d, rm, raw)?,
            Context::Map => {
                if raw["action"] != "move" {
                    return Err(malformed("Unexpected map navigation action"));
                }
                let col = index(raw, "col")?;
                let map = rm.run_state.current_act_map.as_ref().ok_or_else(|| malformed("Missing act map"))?;
                let row = emit_rm::map_coord(rm).map_or(1, |(_, row)| row + 1);
                let node = map.points_in_row(row).into_iter().find(|node| node.col() as usize == col)
                    .ok_or_else(|| malformed("Legal move has no matching node in the next map row"))?;
                json!({"node": {"row": node.coord.row, "col": col}})
            }
            Context::CardReward => match raw["action"].as_str() {
                Some("pick_card_reward") if raw.get("idx") == Some(&Value::Null) => json!({"skip": true}),
                Some("pick_card_reward") => {
                    let i = index(raw, "idx")?;
                    let card = rm.card_reward_options().get(i).ok_or_else(|| malformed("Reward index is outside the current offer"))?;
                    let upgraded = rm.card_reward_upgraded_flags().get(i).copied().unwrap_or(false);
                    json!({"card": {"offer_index": i, "id": card.to_python_name(), "upgraded": upgraded}})
                }
                Some("sacrifice_card_reward") => json!({}),
                _ => return Err(malformed("Unexpected card reward action")),
            },
        };
        Ok(json!({"action_id": id, "class_id": d.classes[id], "upstream": raw, "detail": detail}))
    }).collect()
}
