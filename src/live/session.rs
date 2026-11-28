use serde::Serialize;
use serde_json::{Value, json};

use crate::{Failure, Result, contract::Context};

use super::observation::{self, Observation, list, number, point, schema, string};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Position {
    pub session: u64,
    pub state_id: u64,
    pub request_id: u64,
}

#[derive(Serialize)]
pub struct Ack {
    action: &'static str,
    request_id: String,
    state_id: u64,
    advice: HandBack,
    capabilities: Capabilities,
}

#[derive(Serialize)]
struct HandBack {
    hand_back: &'static str,
}

#[derive(Serialize)]
struct Capabilities {
    coach: u8,
    instinct: [InstinctTarget; 2],
    spiremind_lease: u8,
}

#[derive(Serialize)]
struct InstinctTarget {
    character: &'static str,
    game_version: &'static str,
}

impl Ack {
    pub fn for_position(p: Position) -> Self {
        Self {
            action: "advice",
            request_id: format!("a{}", p.request_id),
            state_id: p.state_id,
            advice: HandBack {
                hand_back: "SpireMind terminal only",
            },
            capabilities: Capabilities {
                coach: 2,
                instinct: ["v0.111.0", "v0.111.1"].map(|game_version| InstinctTarget {
                    character: "IRONCLAD",
                    game_version,
                }),
                spiremind_lease: 1,
            },
        }
    }
}

fn request_id(raw: &Value, key: &str) -> Result<u64> {
    let value = string(raw, key)?;
    let number = value
        .strip_prefix('a')
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|n| *n > 0)
        .ok_or_else(|| schema("Advisor request ID"))?;
    if value != format!("a{number}") {
        return Err(schema("Advisor request ID"));
    }
    Ok(number)
}

pub enum Event {
    Observation(Position),
    Confirm(Position, u64),
    Withdraw(Position),
    Invalidate(&'static str),
    Ignore,
}

struct MapPosition {
    version: String,
    act: u64,
    floor: u64,
    map: Value,
}

struct MapChoice {
    position: Position,
    nodes: Value,
}

pub struct Session {
    id: u64,
    last: Option<Position>,
    map: Option<MapPosition>,
    choice: Option<MapChoice>,
    lease_seq: u64,
}

impl Session {
    pub fn new(id: u64) -> Self {
        Self {
            id,
            last: None,
            map: None,
            choice: None,
            lease_seq: 0,
        }
    }

    pub fn accept(&mut self, raw: &Value) -> Result<Event> {
        match string(raw, "type")? {
            "human_action" => {
                if raw["decision_type"] == "map_select" {
                    self.map_pick(raw);
                }
                Ok(Event::Invalidate("player_action"))
            }
            "combat_ended" => Ok(Event::Invalidate("combat_ended")),
            "game_over" | "run_complete" | "rewind" | "snapshot_restored" | "combat_undone" => {
                self.map = None;
                self.choice = None;
                Ok(Event::Invalidate("run_boundary"))
            }
            // The producer's heartbeat carries no new position.
            "pong" => Ok(Event::Ignore),
            "spiremind_confirm" | "spiremind_withdraw" => {
                let position = self.wire_position(raw)?;
                if self.last != Some(position) {
                    return Ok(Event::Ignore);
                }
                if raw["type"] == "spiremind_withdraw" {
                    return Ok(Event::Withdraw(position));
                }
                let seq = number(raw, "lease_seq", i64::MAX as u64)?;
                if seq <= self.lease_seq {
                    return Ok(Event::Ignore);
                }
                self.lease_seq = seq;
                Ok(Event::Confirm(position, seq))
            }
            // Vanilla room entry saves before the Advisor's correlated map
            // pick arrives. Drop the display, keeping that pending pick.
            "snapshot_saved" if raw.get("request_id").is_none() => {
                Ok(Event::Invalidate("snapshot_saved"))
            }
            "advice_request" => {
                let current = self.wire_position(raw)?;
                if let Some(last) = self.last {
                    if last == current {
                        return Ok(Event::Ignore);
                    }
                    if last.state_id >= current.state_id || last.request_id >= current.request_id {
                        return Err(schema("out-of-order Advisor frame"));
                    }
                }
                self.last = Some(current);
                self.lease_seq = 0;
                Ok(Event::Observation(current))
            }
            // Reject the autopilot lane (and everything unrecognized) before
            // a write. A disconnected client cannot answer the mod's commands.
            _ => Err(Failure::new(
                "non_advisor_frame",
                "This connection is not on the read-only Advisor lane",
            )),
        }
    }

    fn wire_position(&self, raw: &Value) -> Result<Position> {
        let state_id = number(raw, "state_id", i64::MAX as u64)?;
        if state_id == 0 {
            return Err(schema("state_id"));
        }
        Ok(Position {
            session: self.id,
            state_id,
            request_id: request_id(raw, "request_id")?,
        })
    }

    pub fn observe(&mut self, raw: &Value, position: Position) -> Result<Observation> {
        let mut observation = observation::sanitize_live(raw, observation::context(raw)?)?;
        self.attach_map(&mut observation, position)?;
        Ok(observation)
    }

    pub(super) fn observe_native(
        &mut self,
        raw: &Value,
        position: Position,
    ) -> Result<Observation> {
        if observation::context(raw)? != Context::Combat {
            return self.observe(raw, position);
        }
        let mut observation = super::native_current::sanitize(raw)?;
        self.attach_map(&mut observation, position)?;
        Ok(observation)
    }

    fn attach_map(&mut self, o: &mut Observation, position: Position) -> Result<()> {
        let b = &mut o.board;
        let version = string(b, "game_version")?.to_owned();
        let act = number(b, "act", 3)?;
        let floor = number(b, "floor", 100)?;
        if self
            .map
            .as_ref()
            .is_some_and(|m| m.version != version || m.act != act || m.floor > floor)
        {
            self.map = None;
            self.choice = None;
        }
        if !b["map"].is_null() {
            if o.context == Context::Combat
                && (b["map"]["current"].is_null()
                    || list(&b["map"], "visited", 64)?.last() != Some(&b["map"]["current"]))
            {
                return Err(Failure::new(
                    "missing_visible_map",
                    "The combat observation does not identify the last visited map point",
                ));
            }
            self.map = Some(MapPosition {
                version,
                act,
                floor,
                map: b["map"].clone(),
            });
            self.choice = (o.context == Context::Map).then(|| MapChoice {
                position,
                nodes: b["nodes"].clone(),
            });
        } else if o.context == Context::Combat
            && let Some(map) = self
                .map
                .as_ref()
                .filter(|m| m.floor == floor && !m.map["current"].is_null())
        {
            b["map"] = map.map.clone();
        } else {
            return Err(Failure::new(
                "missing_visible_map",
                "Enter from a map choice observed by this session or replay a complete capture",
            ));
        }
        Ok(())
    }

    fn map_pick(&mut self, raw: &Value) {
        let (Some(choice), Some(map)) = (self.choice.take(), self.map.as_mut()) else {
            return;
        };
        let selected = (|| -> Result<Value> {
            if request_id(raw, "advice_request_id")? != choice.position.request_id
                || number(raw, "position_id", i64::MAX as u64)? != choice.position.state_id
            {
                return Err(schema("map choice correlation"));
            }
            let index = number(&raw["choice"], "index", 255)?;
            let node = choice
                .nodes
                .as_array()
                .and_then(|ns| ns.iter().find(|n| n["index"] == index))
                .ok_or_else(|| schema("reported map choice"))?;
            let (col, row) = point(node)?;
            Ok(json!({"col":col,"row":row}))
        })();
        if let Ok(coord) = selected {
            if let Ok(visited) = list(&map.map, "visited", 63) {
                let mut updated = visited.to_vec();
                if !updated.contains(&coord) {
                    updated.push(coord.clone());
                }
                map.map["visited"] = json!(updated);
                map.map["current"] = coord;
                map.floor += 1;
            } else {
                self.map = None;
            }
        } else {
            self.map = None;
        }
    }
}
