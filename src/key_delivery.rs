//! Browser-compatible ML-KEM-768 + P-256 room-key delivery for room agents.
use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as B64};
use ml_kem::{Decapsulate, DecapsulationKey, KeyExport, MlKem768};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub const SUITE: &str = "ml-kem-768+p256-hkdf-sha256-v2";
#[derive(Serialize, Deserialize, zeroize::Zeroize, zeroize::ZeroizeOnDrop)]
#[serde(deny_unknown_fields)]
pub struct Device {
    v: u8,
    server_id: String,
    project_id: String,
    member_id: String,
    p256_private: String,
    ml_kem_seed: String,
}
#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    pub(crate) fn browser(input: Value) -> Result<Value> {
        use std::io::Write;
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut process = std::process::Command::new(root.join("node_modules/.bin/node"))
            .arg(root.join("tests/key-delivery-interop.mjs"))
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .context("run npm ci before interop tests")?;
        process
            .stdin
            .take()
            .unwrap()
            .write_all(&serde_json::to_vec(&input)?)?;
        let output = process.wait_with_output()?;
        ensure!(
            output.status.success(),
            "browser interop failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(serde_json::from_slice(&output.stdout)?)
    }
    #[test]
    fn browser_rotation_history_is_readable_in_rust_but_new_posts_are_not_readable_with_old_keys()
    -> Result<()> {
        let fixture = browser(json!({"mode":"rotation"}))?;
        let mut latest =
            crate::e2ee::Recovery::parse(&fixture["bundle"].to_string(), "server", "project")?;
        let first =
            crate::e2ee::Recovery::parse(&fixture["first"].to_string(), "server", "project")?;
        assert!(first.decrypt("member", &fixture["payload"]).is_err());
        assert!(latest.decrypt("member", &fixture["old"]).is_err());
        latest.restore_history(fixture["history"].as_array().unwrap())?;
        assert_eq!(
            latest.decrypt("member", &fixture["old"])?["message"],
            "old history"
        );
        assert_eq!(
            latest.decrypt("member", &fixture["payload"])?["message"],
            "new history"
        );
        let mut tampered = fixture["history"].as_array().unwrap().clone();
        tampered[0]["previous_key_id"] = json!(first.key_id);
        let mut current =
            crate::e2ee::Recovery::parse(&fixture["bundle"].to_string(), "server", "project")?;
        assert!(current.restore_history(&tampered).is_err());
        Ok(())
    }
    #[test]
    fn noble_browser_delivery_decrypts_in_rust_and_rust_posts_decrypt_in_browser() -> Result<()> {
        let device = Device::generate("server", "project", "member")?;
        let serialized = serde_json::to_string(&device)?;
        let device = Device::parse(&serialized, "server", "project", "member")?;
        assert!(Device::parse(&serialized, "server", "other", "member").is_err());
        let result = browser(
            json!({"server":"server","project":"project","member":"member","device":{"id":device.id()?,"public_key":device.public_key()?}}),
        )?;
        let key_id = result["bundle"]["key_id"].as_str().unwrap();
        let recovery = device.unwrap(&result["envelope"], key_id)?;
        let payload = recovery.encrypt(
            "member",
            "operation",
            &json!({"message":"private Rust post"}),
        )?;
        assert_eq!(
            browser(
                json!({"mode":"decrypt","bundle":result["bundle"],"member":"member","payload":payload})
            )?,
            json!({"message":"private Rust post"})
        );
        for field in ["kem_ciphertext", "nonce", "ciphertext"] {
            let mut changed = result["envelope"].clone();
            let text = changed[field].as_str().unwrap();
            changed[field] = json!(format!(
                "{}{}",
                if text.starts_with('A') { 'B' } else { 'A' },
                &text[1..]
            ));
            assert!(device.unwrap(&changed, key_id).is_err());
        }
        let mut changed = result["envelope"].clone();
        changed["suite"] = json!("legacy");
        assert!(device.unwrap(&changed, key_id).is_err());
        assert!(device.unwrap(&result["envelope"], "wrong-key").is_err());
        assert!(
            Device::generate("server", "project", "other")?
                .unwrap(&result["envelope"], key_id)
                .is_err()
        );
        Ok(())
    }
}
fn bytes(text: &str, len: usize) -> Result<Vec<u8>> {
    let raw = B64.decode(text)?;
    ensure!(
        raw.len() == len && B64.encode(&raw) == text,
        "invalid delivery key encoding"
    );
    Ok(raw)
}
impl Device {
    pub fn generate(server: &str, project: &str, member: &str) -> Result<Self> {
        let mut scalar = [0u8; 32];
        loop {
            getrandom::getrandom(&mut scalar)
                .map_err(|_| anyhow::anyhow!("OS randomness unavailable"))?;
            if p256::SecretKey::from_slice(&scalar).is_ok() {
                break;
            }
        }
        let mut seed = [0u8; 64];
        getrandom::getrandom(&mut seed)
            .map_err(|_| anyhow::anyhow!("OS randomness unavailable"))?;
        let device = Self {
            v: 1,
            server_id: server.into(),
            project_id: project.into(),
            member_id: member.into(),
            p256_private: B64.encode(scalar),
            ml_kem_seed: B64.encode(seed),
        };
        scalar.fill(0);
        seed.fill(0);
        Ok(device)
    }
    pub fn parse(text: &str, server: &str, project: &str, member: &str) -> Result<Self> {
        crate::signed::strict_json(text.as_bytes())?;
        let device: Self = serde_json::from_str(text)?;
        ensure!(
            device.v == 1
                && device.server_id == server
                && device.project_id == project
                && device.member_id == member,
            "delivery key targets another room or member"
        );
        device.public_key()?;
        Ok(device)
    }
    fn p256(&self) -> Result<p256::SecretKey> {
        Ok(p256::SecretKey::from_slice(&bytes(
            &self.p256_private,
            32,
        )?)?)
    }
    fn kem(&self) -> Result<DecapsulationKey<MlKem768>> {
        let mut seed = bytes(&self.ml_kem_seed, 64)?;
        let key = DecapsulationKey::from_seed(seed.as_slice().try_into()?);
        seed.fill(0);
        Ok(key)
    }
    pub fn public_key(&self) -> Result<Value> {
        use p256::elliptic_curve::sec1::ToSec1Point;
        let point = self.p256()?.public_key().to_sec1_point(false);
        let raw = point.as_bytes();
        Ok(
            json!({"suite":SUITE,"p256":{"kty":"EC","crv":"P-256","x":B64.encode(&raw[1..33]),"y":B64.encode(&raw[33..65])},"ml_kem":B64.encode(self.kem()?.encapsulation_key().to_bytes())}),
        )
    }
    pub fn id(&self) -> Result<String> {
        let p = self.public_key()?;
        Ok(B64.encode(Sha256::digest(serde_json::to_vec(&json!([
            SUITE,
            p["p256"]["x"],
            p["p256"]["y"],
            p["ml_kem"]
        ]))?)))
    }
    pub fn unwrap(&self, envelope: &Value, key_id: &str) -> Result<crate::e2ee::Recovery> {
        ensure!(
            envelope["suite"] == SUITE && envelope["device_id"].as_str() == Some(&self.id()?),
            "delivery suite or device mismatch"
        );
        let point = &envelope["ephemeral"];
        ensure!(
            point["kty"] == "EC" && point["crv"] == "P-256",
            "invalid ephemeral key"
        );
        let mut encoded = vec![4u8];
        encoded.extend(bytes(point["x"].as_str().context("missing x")?, 32)?);
        encoded.extend(bytes(point["y"].as_str().context("missing y")?, 32)?);
        let public = p256::PublicKey::from_sec1_bytes(&encoded)?;
        let shared =
            p256::ecdh::diffie_hellman(self.p256()?.to_nonzero_scalar(), public.as_affine());
        let ciphertext = bytes(
            envelope["kem_ciphertext"]
                .as_str()
                .context("missing KEM ciphertext")?,
            1088,
        )?;
        let kem = self.kem()?.decapsulate(ciphertext.as_slice().try_into()?);
        let mut combined = Vec::from(shared.raw_secret_bytes().as_slice());
        combined.extend_from_slice(&kem);
        let nonce = bytes(envelope["nonce"].as_str().context("missing nonce")?, 12)?;
        let context = serde_json::to_vec(&json!([
            "cc-sakura-room-key-v2",
            SUITE,
            self.server_id,
            self.project_id,
            format!("agent:{}", self.member_id),
            self.id()?,
            key_id
        ]))?;
        let mut aes = [0u8; 32];
        hkdf::Hkdf::<Sha256>::new(Some(&nonce), &combined)
            .expand(&context, &mut aes)
            .map_err(|_| anyhow::anyhow!("key derivation failed"))?;
        combined.fill(0);
        let cipher = ring::aead::LessSafeKey::new(
            ring::aead::UnboundKey::new(&ring::aead::AES_256_GCM, &aes)
                .map_err(|_| anyhow::anyhow!("invalid encryption key"))?,
        );
        aes.fill(0);
        let text = envelope["ciphertext"]
            .as_str()
            .context("missing wrapped key")?;
        ensure!(text.len() <= 8192, "wrapped key too large");
        let mut wrapped = B64.decode(text)?;
        let clear = cipher
            .open_in_place(
                ring::aead::Nonce::try_assume_unique_for_key(&nonce)
                    .map_err(|_| anyhow::anyhow!("invalid nonce"))?,
                ring::aead::Aad::from(context),
                &mut wrapped,
            )
            .map_err(|_| anyhow::anyhow!("room key delivery authentication failed"))?;
        let recovery = crate::e2ee::Recovery::parse(
            std::str::from_utf8(clear)?,
            &self.server_id,
            &self.project_id,
        )?;
        ensure!(recovery.key_id == key_id, "room key mismatch");
        wrapped.fill(0);
        Ok(recovery)
    }
}
