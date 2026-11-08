//! Verification of archived WebAuthn proofs, with no server or session store.
use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::time::Duration;
use uuid::Uuid;
use webauthn_rs::prelude::*;
const CEREMONY_TTL: i64 = 120;
#[derive(Clone, Serialize, Deserialize)]
struct StoredPasskey {
    origin: String,
    passkey: Passkey,
}
fn hash(bytes: impl AsRef<[u8]>) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(bytes.as_ref()))
}

fn key_id(passkey: &Passkey) -> Result<String> {
    if passkey.cred_algorithm() != &COSEAlgorithm::ES256 {
        bail!("Only ES256 passkeys are supported");
    }
    let COSEKeyType::EC_EC2(key) = &passkey.get_public_key().key else {
        bail!("Expected P-256 public key");
    };
    if key.curve != ECDSACurve::SECP256R1 || key.x.len() != 32 || key.y.len() != 32 {
        bail!("Expected P-256 public key");
    }
    let mut point = vec![4];
    point.extend_from_slice(&key.x);
    point.extend_from_slice(&key.y);
    Ok(format!("webauthn-p256:{}", hash(point)))
}
fn build_webauthn(origin: &str) -> Result<(Webauthn, String, bool)> {
    let url = Url::parse(origin)?;
    if url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        bail!("WebAuthn origin must be an origin without a path or credentials");
    }
    let domain = url
        .domain()
        .context("WebAuthn requires a domain name (localhost is allowed for local testing)")?;
    let secure = url.scheme() == "https";
    if !secure && !(url.scheme() == "http" && domain == "localhost") {
        bail!("WebAuthn requires HTTPS except localhost");
    }
    let webauthn = WebauthnBuilder::new(domain, &url)?
        .rp_name("cc-sakura")
        .timeout(Duration::from_secs(CEREMONY_TTL as u64))
        .build()?;
    Ok((webauthn, url.origin().ascii_serialization(), secure))
}

fn check_user_handle(assertion: &PublicKeyCredential, user: &str) -> Result<()> {
    let expected = Uuid::parse_str(user)?;
    ensure!(
        assertion.type_ == "public-key"
            && assertion.id == URL_SAFE_NO_PAD.encode(&assertion.raw_id)
            && assertion
                .get_user_unique_id()
                .is_none_or(|actual| actual == expected.as_bytes()),
        "invalid user handle"
    );
    Ok(())
}

fn bind_challenge(state: PasskeyAuthentication, challenge: &str) -> Result<PasskeyAuthentication> {
    let mut encoded = serde_json::to_value(&state)?;
    let slot = encoded
        .pointer_mut("/ast/challenge")
        .context("unsupported webauthn-rs authentication state format")?;
    if !slot.is_string() {
        bail!("unsupported challenge representation");
    }
    *slot = json!(challenge);
    let bound: PasskeyAuthentication = serde_json::from_value(encoded.clone())?;
    if serde_json::to_value(&bound)? != encoded {
        bail!("WebAuthn state did not round-trip");
    }
    Ok(bound)
}

/// Verify an archived browser post without consulting mutable login/session state.
/// The caller must obtain public_key from a trusted key registry and separately
/// check the returned context's project/member against its expected destination.
pub fn verify_proof(proof: &Value, public_key: &str) -> Result<Value> {
    if proof["scheme"] != "webauthn-v1" {
        bail!("unsupported browser proof");
    }
    let stored: StoredPasskey = serde_json::from_str(public_key)?;
    let (webauthn, _, _) = build_webauthn(&stored.origin)?;
    let statement = proof["statement"]
        .as_str()
        .context("missing signed statement")?;
    let claims: Value = serde_json::from_str(statement)?;
    let context = &proof["context"];
    let action_json = context["action_json"]
        .as_str()
        .context("missing action original")?;
    if claims["v"] != 1
        || claims["purpose"] != "cc-sakura-web-action-v1"
        || claims["key_id"] != key_id(&stored.passkey)?
        || claims["action_sha256"] != hash(action_json)
    {
        bail!("invalid action binding");
    }
    for field in ["server_id", "project_id", "member_id", "endpoint"] {
        if claims[field].as_str().is_none() || claims[field] != context[field] {
            bail!("invalid statement context");
        }
    }
    let action: Value = serde_json::from_str(action_json)?;
    if action["project_id"] != claims["project_id"] {
        bail!("project mismatch");
    }
    let assertion: PublicKeyCredential = serde_json::from_value(proof["credential"].clone())?;
    check_user_handle(
        &assertion,
        claims["user_id"].as_str().context("missing user")?,
    )?;
    // Counters and backup state are acceptance-time checks, not expiry rules for
    // archived signatures. Keep the trusted public key and required UV policy,
    // but verify the proof independently of its current mutable counter.
    let mut credential: Credential = stored.passkey.into();
    credential.counter = 0;
    credential.backup_eligible = assertion
        .response
        .authenticator_data
        .get(32)
        .is_some_and(|flags| flags & 8 != 0);
    credential.backup_state = false;
    let (_, state) = webauthn.start_passkey_authentication(&[Passkey::from(credential)])?;
    let bound = bind_challenge(state, &hash(statement))?;
    let result = webauthn.finish_passkey_authentication(&assertion, &bound)?;
    if !result.user_verified() {
        bail!("user verification required");
    }
    Ok(context.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn archived_browser_proof_and_tampering() -> Result<()> {
        let fixture: Value =
            serde_json::from_str(include_str!("../tests/fixtures/cloud-webauthn.json"))?;
        let public = fixture["public_key"].as_str().unwrap();
        assert_eq!(
            verify_proof(&fixture["proof"], public)?,
            fixture["proof"]["context"]
        );
        let mut changed = fixture["proof"].clone();
        changed["context"]["action_json"] = json!("{}");
        assert!(verify_proof(&changed, public).is_err());
        Ok(())
    }
}
