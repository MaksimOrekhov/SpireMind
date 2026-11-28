use serde_json::Value;
use sts2_core::PowerKind;
use sts2_wire::state_dict::power_kind_from_python_name;

use crate::{Failure, Result};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Owner {
    Player,
    Enemy,
}

pub(super) fn registry_id(id: &str) -> Option<PowerKind> {
    power_kind_from_python_name(id)
        .or_else(|| power_kind_from_python_name(id.strip_suffix("_POWER")?))
}

pub(super) fn legacy(kind: PowerKind) -> bool {
    use PowerKind::*;
    matches!(
        kind,
        Strength
            | Dexterity
            | Vulnerable
            | Weak
            | Frail
            | Artifact
            | Ritual
            | Thorns
            | Metallicize
            | CurlUp
            | Intangible
            | Buffer
            | Entangled
            | Vigor
    )
}

pub(super) fn requirement(kind: PowerKind, owner: Owner) -> (&'static str, &'static str) {
    use PowerKind::*;
    if legacy(kind) {
        return ("visible_amount", "Existing audited amount mapping");
    }
    match (kind, owner) {
        (
            Barricade | Clarity | Corruption | Cruelty | DarkEmbrace | DemonForm | FeelNoPain
            | NoDraw | Rage | RetainHand | Rupture | SetupStrike,
            Owner::Player,
        ) => ("visible_amount", "Pinned hooks read presence or amount"),
        (Shrink, _) | (Plating, Owner::Player) | (Ravenous | Shriek | Slippery, Owner::Enemy) => (
            "visible_amount",
            "Pinned damage and turn hooks read the shown amount and public board",
        ),
        (Minion, Owner::Enemy) => (
            "visible_amount",
            "Pinned engine checks presence for secondary enemies",
        ),
        (Slow, Owner::Enemy) => (
            "public_parameter",
            "Pinned state_dict derives the counter from DisplayAmount",
        ),
        (HardenedShell, Owner::Enemy) | (Tender, Owner::Player) => (
            "public_parameter_pending",
            "Pinned parser derives the counter, but the simulator emitter does not reproduce the public display",
        ),
        (Juggling, Owner::Player) => (
            "public_history",
            "Pinned state_dict derives the counter from this turn's Attack bucket",
        ),
        (Unmovable, Owner::Player) => (
            "public_history_pending",
            "Pinned mirror uses distinct card-sourced block gains; requires position-level parity",
        ),
        (Plating, Owner::Enemy) => (
            "public_history_pending",
            "Pinned hooks use a round-specific counter after an enemy gains Plating",
        ),
        (CrimsonMantle | Inferno | ToricToughness | PainfulStabs, _) => (
            "public_parameter_pending",
            "Upstream maps a DynamicVar; public provenance and semantics require verification",
        ),
        (Constrict | Flanking | Knockdown | TagTeam, _) => (
            "public_parameter_pending",
            "Upstream uses ownership or applier; solo identity mapping requires proof",
        ),
        (Panache | Surrounded | Skittish, _) => (
            "hidden_or_ambiguous",
            "Current wire cannot prove one exact state without a hidden or ambiguous counter",
        ),
        _ => (
            "unverified",
            "Registry presence alone does not prove reachable hooks or exact reconstruction",
        ),
    }
}

pub(super) fn validate(value: &Value, owner: Owner) -> Result<()> {
    let Some(kind) = value["id"].as_str().and_then(registry_id) else {
        return Err(Failure::new(
            "unsupported_mechanic",
            "Power ID is outside the pinned registry",
        ));
    };
    let amount = value["amount"].as_i64().expect("validated amount");
    let shown = value["display_amount"].as_i64();
    let (class, _) = requirement(kind, owner);
    if match (class, kind) {
        ("visible_amount" | "public_history", _) => legacy(kind) || shown == Some(amount),
        ("public_parameter", PowerKind::Slow) => {
            amount == 1 && shown.is_some_and(|n| n >= 0 && n % 10 == 0)
        }
        _ => false,
    } {
        return Ok(());
    }
    if class == "public_parameter" {
        Err(Failure::new(
            "power_public_parameter",
            "Power requires a valid visible DisplayAmount",
        ))
    } else {
        Err(Failure::new(
            "unsupported_mechanic",
            "Power is outside the audited observation mapping",
        ))
    }
}
