use serde_json::Value;
use sts2_core::RelicId;

use crate::{Failure, Result};

pub(super) fn id(row: &Value) -> Option<RelicId> {
    let name = row["id"].as_str()?;
    sts2_content::names::relic_id_from_str_any_build(name.strip_prefix("RELIC.").unwrap_or(name))
}

pub(super) fn has_private_latch(relic: RelicId) -> bool {
    use RelicId::*;
    matches!(
        relic,
        Vambrace
            | RuinedHelmet
            | Permafrost
            | CentennialPuzzle
            | UnsettlingLamp
            | ThrowingAxe
            | LizardTail
    )
}

// Settled single-player combat. Active/normal is a per-combat icon on these
// three relics. Lizard Tail's disabled icon survives room transitions.
pub(super) fn status_values(relic: RelicId) -> Option<[(&'static str, i32); 2]> {
    use RelicId::*;
    match relic {
        Vambrace | ThrowingAxe => Some([("active", 0), ("normal", 1)]),
        UnsettlingLamp => Some([("active", 0), ("normal", 2)]),
        LizardTail => Some([("normal", 0), ("disabled", 1)]),
        _ => None,
    }
}

pub(super) fn combat_counter(row: &Value) -> Result<Option<i32>> {
    let Some(relic) = id(row).filter(|&relic| has_private_latch(relic)) else {
        return Ok(None);
    };
    let derived = if let Some(values) = status_values(relic) {
        values.into_iter().find_map(|(status, counter)| {
            (row["observer_relic_status"] == status).then_some(counter)
        })
    } else if matches!(relic, RelicId::Permafrost | RelicId::CentennialPuzzle) {
        row["observer_trigger_seen"].as_bool().map(i32::from)
    } else {
        None
    };
    derived.map(Some).ok_or_else(|| {
        Failure::new(
            "relic_public_state_missing",
            "A combat relic has no verified public spent-state carrier",
        )
    })
}
