//! Recovery-file based project encryption, interoperable with Web Crypto AES-GCM.
use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as B64};
use ring::aead::{AES_256_GCM, Aad, LessSafeKey, Nonce, UnboundKey};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Recovery {
    pub v: u8,
    pub server_id: String,
    pub project_id: String,
    pub key_id: String,
    key: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    history: Vec<Recovery>,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn web_crypto_interop_and_tampering() -> Result<()> {
        let fixture: Value = serde_json::from_str(include_str!("../tests/fixtures/e2ee-v1.json"))?;
        let r = &fixture["recovery"];
        let recovery = Recovery::parse(
            &r.to_string(),
            r["server_id"].as_str().unwrap(),
            r["project_id"].as_str().unwrap(),
        )?;
        let member = fixture["member"].as_str().unwrap();
        assert_eq!(
            recovery.decrypt(member, &fixture["payload"])?,
            fixture["clear"]
        );
        assert!(
            recovery
                .decrypt("another member", &fixture["payload"])
                .is_err()
        );
        let mut tampered = fixture["payload"].clone();
        tampered["operation_id"] = json!("different operation");
        assert!(recovery.decrypt(member, &tampered).is_err());
        let encrypted = recovery.encrypt(member, "operation", &fixture["clear"])?;
        assert_eq!(recovery.decrypt(member, &encrypted)?, fixture["clear"]);
        // The same nonce and AAD must produce precisely Web Crypto's output.
        let nonce: [u8; 12] = B64
            .decode(fixture["payload"]["nonce"].as_str().unwrap())?
            .try_into()
            .unwrap();
        let mut bytes = serde_json::to_vec(&fixture["clear"])?;
        recovery
            .cipher()?
            .seal_in_place_append_tag(
                Nonce::assume_unique_for_key(nonce),
                Aad::from(
                    recovery.aad(member, fixture["payload"]["operation_id"].as_str().unwrap())?,
                ),
                &mut bytes,
            )
            .unwrap();
        assert_eq!(
            B64.encode(bytes),
            fixture["payload"]["ciphertext"].as_str().unwrap()
        );
        Ok(())
    }
}
impl Recovery {
    pub fn parse(text: &str, server: &str, project: &str) -> Result<Self> {
        crate::signed::strict_json(text.as_bytes())?;
        let value: Self = serde_json::from_str(text)?;
        ensure!(
            value.v == 1 && value.server_id == server && value.project_id == project,
            "recovery key targets another project"
        );
        let raw = B64.decode(&value.key)?;
        ensure!(
            raw.len() == 32
                && B64.encode(&raw) == value.key
                && B64.encode(Sha256::digest(&raw)) == value.key_id,
            "invalid recovery key"
        );
        ensure!(value.history.len() <= 10000, "too many historical keys");
        let mut seen = std::collections::HashSet::from([value.key_id.as_str()]);
        for key in &value.history {
            ensure!(
                key.history.is_empty() && seen.insert(key.key_id.as_str()),
                "invalid historical keys"
            );
            Self::parse(&serde_json::to_string(key)?, server, project)?;
        }
        Ok(value)
    }
    pub fn restore_history(&mut self, records: &[Value]) -> Result<()> {
        ensure!(records.len() <= 10000, "key history limit");
        let mut links = std::collections::HashMap::new();
        for row in records {
            ensure!(
                links
                    .insert(
                        row["key_id"].as_str().context("missing history key ID")?,
                        row
                    )
                    .is_none(),
                "duplicate history link"
            );
        }
        let mut current = self.clone();
        current.history.clear();
        let mut history = Vec::new();
        let mut seen = std::collections::HashSet::from([self.key_id.clone()]);
        while let Some(row) = links.get(current.key_id.as_str()) {
            let previous = row["previous_key_id"]
                .as_str()
                .context("missing previous key ID")?;
            ensure!(seen.insert(previous.to_owned()), "cyclic key history");
            let context = serde_json::to_vec(&json!([
                "cc-sakura-key-history-v1",
                self.server_id,
                self.project_id,
                current.key_id,
                previous
            ]))?;
            let nonce: [u8; 12] = B64
                .decode(row["nonce"].as_str().context("missing history nonce")?)?
                .try_into()
                .map_err(|_| anyhow::anyhow!("invalid history nonce"))?;
            let text = row["ciphertext"].as_str().context("missing archived key")?;
            ensure!(text.len() <= 2048, "archived key too large");
            let mut wrapped = B64.decode(text)?;
            let clear = current
                .cipher()?
                .open_in_place(
                    Nonce::assume_unique_for_key(nonce),
                    Aad::from(context),
                    &mut wrapped,
                )
                .map_err(|_| anyhow::anyhow!("key history authentication failed"))?;
            let key = Self::parse(
                std::str::from_utf8(clear)?,
                &self.server_id,
                &self.project_id,
            )?;
            ensure!(
                key.key_id == previous && key.history.is_empty(),
                "history key mismatch"
            );
            wrapped.fill(0);
            current = key.clone();
            history.push(key);
        }
        self.history = history;
        Ok(())
    }
    fn cipher(&self) -> Result<LessSafeKey> {
        let raw = B64.decode(&self.key)?;
        Ok(LessSafeKey::new(
            UnboundKey::new(&AES_256_GCM, &raw)
                .map_err(|_| anyhow::anyhow!("invalid encryption key"))?,
        ))
    }
    fn aad(&self, member: &str, operation: &str) -> Result<Vec<u8>> {
        Ok(serde_json::to_vec(&json!([
            "cc-sakura-e2ee-v1",
            self.server_id,
            self.project_id,
            member,
            operation,
            self.key_id
        ]))?)
    }
    pub fn encrypt(&self, member: &str, operation: &str, payload: &Value) -> Result<Value> {
        ensure!(
            !matches!(
                payload["type"].as_str(),
                Some("approval_request" | "approval_decision")
            ),
            "encrypted approvals are not supported yet"
        );
        let mut nonce = [0u8; 12];
        getrandom::getrandom(&mut nonce)
            .map_err(|_| anyhow::anyhow!("OS randomness unavailable"))?;
        let mut bytes = serde_json::to_vec(payload)?;
        self.cipher()?
            .seal_in_place_append_tag(
                Nonce::assume_unique_for_key(nonce),
                Aad::from(self.aad(member, operation)?),
                &mut bytes,
            )
            .map_err(|_| anyhow::anyhow!("encryption failed"))?;
        Ok(
            json!({"type":"e2ee-v1","key_id":self.key_id,"operation_id":operation,"nonce":B64.encode(nonce),"ciphertext":B64.encode(bytes)}),
        )
    }
    pub fn decrypt(&self, member: &str, payload: &Value) -> Result<Value> {
        if payload["key_id"] != self.key_id {
            let old = self
                .history
                .iter()
                .find(|key| payload["key_id"] == key.key_id)
                .context("historical key is unavailable")?;
            return old.decrypt(member, payload);
        }
        ensure!(
            payload["type"] == "e2ee-v1" && payload["key_id"] == self.key_id,
            "recovery key does not match message"
        );
        let nonce = B64.decode(payload["nonce"].as_str().context("missing nonce")?)?;
        let nonce: [u8; 12] = nonce
            .try_into()
            .map_err(|_| anyhow::anyhow!("invalid nonce"))?;
        let operation = payload["operation_id"]
            .as_str()
            .context("missing operation ID")?;
        let mut bytes = B64.decode(
            payload["ciphertext"]
                .as_str()
                .context("missing ciphertext")?,
        )?;
        let clear = self
            .cipher()?
            .open_in_place(
                Nonce::assume_unique_for_key(nonce),
                Aad::from(self.aad(member, operation)?),
                &mut bytes,
            )
            .map_err(|_| anyhow::anyhow!("message authentication failed"))?;
        crate::signed::strict_json(clear)?;
        Ok(serde_json::from_slice(clear)?)
    }
}
