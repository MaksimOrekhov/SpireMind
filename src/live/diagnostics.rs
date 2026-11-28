use std::{fs, io, path::PathBuf};

use super::{
    observation,
    powers::{self, Owner},
    session::Position,
};
use crate::{contract::Context, model};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const MAX_RECORDS: usize = 256;
const MAX_BYTES: usize = 2 * 1024 * 1024;
const MAX_BLOCKERS: usize = 256;

pub struct Corpus {
    dir: PathBuf,
    files: usize,
    bytes: usize,
}

impl Corpus {
    pub fn new(dir: PathBuf) -> io::Result<Self> {
        fs::create_dir(&dir)?;
        Ok(Self {
            dir,
            files: 0,
            bytes: 0,
        })
    }

    pub fn write(&mut self, record: impl FnOnce() -> Value) -> io::Result<()> {
        if self.files >= MAX_RECORDS || self.bytes >= MAX_BYTES {
            return Ok(());
        }
        let data = serde_json::to_vec(&record())?;
        if data.len() > MAX_BYTES - self.bytes {
            self.bytes = MAX_BYTES;
            return Ok(());
        }
        let path = self.dir.join(format!("blocked-{:04}.json", self.files));
        use std::io::Write as _;
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)?
            .write_all(&data)?;
        self.files += 1;
        self.bytes += data.len();
        Ok(())
    }
}

fn rows(v: &Value, max: usize) -> &[Value] {
    v.as_array()
        .filter(|xs| xs.len() <= max)
        .map_or(&[], Vec::as_slice)
}

fn integer(v: &Value) -> Option<i64> {
    v.as_i64().filter(|n| (-1000..=100_000).contains(n))
}

fn id(v: &Value) -> Option<&str> {
    v.as_str().filter(|name| {
        !name.is_empty()
            && name.len() <= 64
            && name
                .bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
    })
}

fn public_power_id(v: &Value) -> Option<&'static str> {
    if v["observer_visible"] != true {
        return None;
    }
    Some(powers::registry_id(id(&v["id"])?)?.python_name())
}

fn kind(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn field_types(v: &Value, fields: &[&str]) -> Value {
    Value::Object(
        fields
            .iter()
            .filter_map(|&k| v.get(k).map(|v| (k.into(), json!(kind(v)))))
            .collect(),
    )
}

struct Projection {
    blockers: Vec<Value>,
    visibility_marker: bool,
    truncated: bool,
}

impl Projection {
    fn check(
        &mut self,
        row: &Value,
        path: String,
        item_id: Option<&str>,
        keys: &[&str],
        why: &'static str,
        validate: impl FnOnce(&Value) -> crate::Result<Value>,
    ) {
        if let Err(error) = validate(row) {
            if self.blockers.len() == MAX_BLOCKERS {
                self.truncated = true;
                return;
            }
            self.blockers.push(json!({
                "path": path, "id": item_id, "code": error.code,
                "reason": if matches!(error.code, "unsupported_mechanic" | "power_visibility_unverified" | "power_public_parameter") {
                    why
                } else { "invalid_shape" },
                "field_types": field_types(row, keys)
            }));
        }
    }

    fn cards(&mut self, value: &Value, path: &str, max: usize) -> Vec<Value> {
        rows(value, max)
            .iter()
            .enumerate()
            .map(|(i, card)| {
                let card_id = id(&card["id"]);
                self.check(
                    card,
                    format!("{path}[{i}]"),
                    card_id,
                    &["id", "upgrade_level", "effective_cost", "combat_vars"],
                    "card_instance_state",
                    observation::card,
                );
                let targets = rows(&card["valid_target_indices"], 32)
                    .iter()
                    .filter_map(integer)
                    .collect::<Vec<_>>();
                json!([
                    card_id,
                    integer(&card["upgrade_level"]),
                    integer(&card["effective_cost"]),
                    card["playable"].as_bool(),
                    targets
                ])
            })
            .collect()
    }

    fn powers(&mut self, value: &Value, path: &str, owner: Owner) -> Vec<Value> {
        rows(value, 64)
            .iter()
            .enumerate()
            .filter_map(|(i, power)| {
                let public_id = self
                    .visibility_marker
                    .then(|| public_power_id(power))
                    .flatten();
                let check = if self.visibility_marker {
                    observation::live_power
                } else {
                    observation::power
                };
                self.check(
                    power,
                    format!("{path}[{i}]"),
                    public_id,
                    &["id", "amount", "display_amount", "vars", "observer_visible"],
                    match (
                        self.visibility_marker,
                        &power["observer_visible"],
                        public_id,
                    ) {
                        (true, Value::Bool(true), Some(_)) => "visible_power_needs_audit",
                        (true, Value::Bool(true), None) => "unmapped_visible_power_id",
                        (true, Value::Bool(false), _) => "hidden_power",
                        _ => "unverified_power_visibility",
                    },
                    |row| check(row, owner),
                );
                // An unverified power contributes no value or identifier to the public fingerprint.
                public_id.map(|id| json!([id, integer(&power["display_amount"])]))
            })
            .collect()
    }

    fn combat(&mut self, raw: &Value) -> Value {
        let hand = self.cards(&raw["hand"], "$.hand", 32);
        for pile in ["discard_pile", "exhaust_pile", "play_pile"] {
            self.cards(&raw[pile]["cards"], &format!("$.{pile}.cards"), 128);
        }
        // The draw pile contributes its public size only. Its contents and
        // array indices are deliberately neither traversed nor logged here.
        let player = &raw["player"];
        let player_powers = self.powers(&player["powers"], "$.player.powers", Owner::Player);
        let enemies: Vec<_> = rows(&raw["enemies"], 32)
            .iter()
            .enumerate()
            .map(|(i, enemy)| {
                let path = format!("$.enemies[{i}]");
                let enemy_id = id(&enemy["id"]);
                self.check(
                    enemy,
                    path.clone(),
                    enemy_id,
                    &["id", "hp", "is_alive", "is_hittable", "powers", "intents"],
                    "enemy_needs_audit",
                    observation::enemy_envelope,
                );
                let powers = self.powers(&enemy["powers"], &format!("{path}.powers"), Owner::Enemy);
                let intents: Vec<_> = rows(&enemy["intents"], 8)
                    .iter()
                    .enumerate()
                    .map(|(j, intent)| {
                        self.check(
                            intent,
                            format!("{path}.intents[{j}]"),
                            None,
                            &["intent_type", "damage", "hits"],
                            "intent_needs_audit",
                            observation::intent,
                        );
                        let known_kind = intent["intent_type"].as_str().filter(|s| {
                            matches!(
                                *s,
                                "Attack"
                                    | "Defend"
                                    | "Buff"
                                    | "Debuff"
                                    | "Heal"
                                    | "Summon"
                                    | "Sleep"
                                    | "Stun"
                                    | "Escape"
                                    | "DeathBlow"
                                    | "Unknown"
                            )
                        });
                        json!([
                            known_kind,
                            integer(&intent["damage"]),
                            integer(&intent["hits"])
                        ])
                    })
                    .collect();
                json!([
                    enemy_id,
                    integer(&enemy["hp"]),
                    integer(&enemy["block"]),
                    enemy["is_alive"].as_bool(),
                    enemy["is_hittable"].as_bool(),
                    powers,
                    intents
                ])
            })
            .collect();
        json!({
            "round": integer(&raw["round"]), "hand": hand, "enemies": enemies,
            "player": [integer(&player["hp"]), integer(&player["block"]), integer(&player["energy"]), player_powers],
            "draw_count": integer(&raw["draw_pile_count"]), "discard_count": integer(&raw["discard_pile_count"])
        })
    }
}

pub fn record(
    raw: &Value,
    position: Position,
    phase: &'static str,
    code: &str,
    admitted_id: Option<&str>,
) -> Value {
    let context = observation::context(raw).ok();
    let surface_in_scope = context.is_some() && code != "unsupported_state";
    let mut projection = Projection {
        blockers: Vec::new(),
        truncated: false,
        visibility_marker: raw["capabilities"]["spiremind_power_visibility"] == 1,
    };
    let mut public = Value::Null;
    if surface_in_scope {
        let run = &raw["run_state"];
        let relics: Vec<_> = rows(&run["relics"], 128)
            .iter()
            .enumerate()
            .map(|(i, relic)| {
                let relic_id = id(&relic["id"]);
                projection.check(
                    relic,
                    format!("$.run_state.relics[{i}]"),
                    relic_id,
                    &["id", "counter", "stack_count", "is_wax", "is_melted"],
                    "relic_needs_audit",
                    observation::relic,
                );
                relic_id
            })
            .collect();
        let potions: Vec<_> = rows(&run["potions"], 16)
            .iter()
            .enumerate()
            .map(|(i, potion)| {
                let potion_id = id(&potion["id"]);
                projection.check(
                    potion,
                    format!("$.run_state.potions[{i}]"),
                    potion_id,
                    &["id", "slot", "target_type", "requires_target", "can_use"],
                    "potion_needs_audit",
                    observation::potion,
                );
                json!([
                    potion_id,
                    potion["can_use"].as_bool(),
                    potion["requires_target"].as_bool()
                ])
            })
            .collect();
        let deck = projection.cards(&run["master_deck"], "$.run_state.master_deck", 128);
        let board = match context {
            Some(Context::Combat) => projection.combat(raw),
            Some(Context::CardReward) => json!({
                "offers":projection.cards(&raw["cards"], "$.cards", 16),
                "can_skip":raw["can_skip"].as_bool()
            }),
            Some(Context::Map) => json!(
                rows(&raw["nodes"], 16)
                    .iter()
                    .map(|n| [integer(&n["col"]), integer(&n["row"]), integer(&n["index"])])
                    .collect::<Vec<_>>()
            ),
            None => Value::Null,
        };
        public = json!({"act":integer(&raw["act"]), "floor":integer(&raw["floor"]), "board":board, "deck":deck, "relics":relics, "potions":potions});
    }

    if projection.blockers.is_empty() {
        projection.blockers.push(json!({"path":"$", "code":code,
            "reason": if surface_in_scope { "position_rejected" } else { "outside_supported_context" }
        }));
    }
    let fingerprint =
        surface_in_scope.then(|| format!("{:x}", Sha256::digest(public.to_string().as_bytes())));
    json!({
        "schema_version":1, "kind":"blocked_diagnostic", "context":context, "in_scope":surface_in_scope, "position":position,
        "phase":phase, "failure_code":code, "public_fingerprint":fingerprint, "admitted_observation_id":admitted_id,
        "producer_revision":if projection.visibility_marker {"observer-visibility-1"} else {"observer-lease-1-or-redacted"},
        "upstream_revision":model::REVISION, "engine_build":model::BUILD.as_str(), "blockers":projection.blockers,
        "truncated":projection.truncated
    })
}
