use std::{
    fs,
    io::ErrorKind,
    path::{Path, PathBuf},
    time::Instant,
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sts2_core::{GameVersion, run_manager::RunManager};
use sts2_e2e::{
    env::Decision,
    net::{E2eNet, PolicyRow},
    schema::{N_CAT, N_CONT, xc, xf},
    tokens::{TokCache, TokRow, encode_for, without_row_dump},
};

use crate::{Failure, Result};

pub const BUILD: GameVersion = GameVersion::V0_111_0;
pub const ASCENSION: u8 = 10;
pub const REVISION: &str = env!("SPIRE_MENTOR_REV");
pub const OBSERVATION: &str = "upstream-policy-without-enemy-forecast-v1";

#[derive(Deserialize, Serialize)]
pub struct ModelPin {
    pub name: String,
    release: String,
    file: String,
    sha256: String,
    bytes: u64,
    format: String,
    game_version: String,
    license: String,
}

pub fn pin() -> ModelPin {
    serde_json::from_str(include_str!("../pins/instinct.json")).expect("checked-in model pin")
}

pub fn default_weights() -> PathBuf {
    Path::new(".local/weights").join(pin().file)
}

pub fn check_build(actual: &str) -> Result<()> {
    if actual == BUILD.as_str() {
        Ok(())
    } else {
        Err(Failure::new(
            "incompatible_game_version",
            format!(
                "Expected {} for this Instinct checkpoint, got {actual}",
                BUILD.as_str()
            ),
        ))
    }
}

pub fn load(path: &Path) -> Result<E2eNet> {
    let pin = pin();
    let metadata = fs::metadata(path).map_err(|error| {
        if error.kind() == ErrorKind::NotFound {
            Failure::new(
                "missing_weights",
                "Run python scripts/setup.py or supply --weights",
            )
        } else {
            Failure::new("weights_io", error.to_string())
        }
    })?;
    if !metadata.is_file() || metadata.len() != pin.bytes {
        return Err(Failure::new(
            "weights_integrity",
            "Unexpected weights file or size",
        ));
    }
    let bytes = fs::read(path).map_err(|e| Failure::new("weights_io", e.to_string()))?;
    if format!("{:x}", Sha256::digest(&bytes)) != pin.sha256 {
        return Err(Failure::new(
            "weights_integrity",
            "SHA-256 does not match the model pin",
        ));
    }
    let net = E2eNet::from_bytes(&bytes).map_err(|e| Failure::new("invalid_weights", e))?;
    let header = net.header();
    check_build(header.game_version.as_deref().unwrap_or("missing"))?;
    if header.format != pin.format || pin.game_version != BUILD.as_str() {
        return Err(Failure::new(
            "invalid_weights",
            "Model metadata differs from the pin",
        ));
    }
    Ok(net)
}

pub fn policy_tokens(
    net: &E2eNet,
    d: &Decision,
    rm: &RunManager,
    cache: &mut TokCache,
) -> Result<TokRow> {
    let mut tokens = without_row_dump(|| encode_for(net, d, rm, cache))
        .map_err(|e| Failure::new("encoder", e))?;
    if d.is_combat() {
        // The policy encoder also predicts later moves from internal AI state.
        // Use its zero/absent values for those fields, keeping the current telegraph.
        for row in tokens.cat.as_chunks_mut::<N_CAT>().0 {
            row[const { xc("X_FC1") }..=const { xc("X_FC3") }].fill(0);
        }
        for row in tokens.cont.as_chunks_mut::<N_CONT>().0 {
            row[const { xf("E_F1_CERT") }..=const { xf("E_F3_TOT") }].fill(0);
        }
    }
    Ok(tokens)
}

pub fn infer(
    net: &E2eNet,
    d: &Decision,
    rm: &RunManager,
    cache: &mut TokCache,
) -> Result<(Vec<f32>, f64)> {
    let row: PolicyRow = policy_tokens(net, d, rm, cache)?.into();
    let start = Instant::now();
    let logits = net
        .try_logits(&row)
        .map_err(|e| Failure::new("inference", e))?;
    let ms = start.elapsed().as_secs_f64() * 1000.0;
    validate_logits(d.members.len(), &logits)?;
    Ok((logits, ms))
}

pub fn validate_logits(classes: usize, logits: &[f32]) -> Result<()> {
    if classes < 2 || logits.len() != classes || logits.iter().any(|value| !value.is_finite()) {
        return Err(Failure::new(
            "malformed_upstream_response",
            "Expected one finite logit per policy class",
        ));
    }
    Ok(())
}
