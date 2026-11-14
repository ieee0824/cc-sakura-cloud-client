//! Encrypted approval metadata. Labels and comments stay inside the ciphertext.

const OTHER_CHOICE: &str = "00000000-0000-4000-8000-000000000000";
const REJECT_CHOICE: &str = "00000000-0000-4000-8000-000000000001";
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use uuid::Uuid;

fn fields(value: &Value, required: &[&str], optional: &[&str]) -> Result<()> {
    let map = value.as_object().context("approval must be an object")?;
    ensure!(
        required.iter().all(|key| map.contains_key(*key))
            && map
                .keys()
                .all(|key| required.contains(&key.as_str()) || optional.contains(&key.as_str())),
        "invalid approval fields"
    );
    Ok(())
}
fn option_id(value: &Value) -> Result<&str> {
    let text = value.as_str().context("choice ID must be a UUIDv4")?;
    let id = Uuid::parse_str(text)?;
    ensure!(
        id.get_version_num() == 4
            && id.get_variant() == uuid::Variant::RFC4122
            && id.to_string() == text,
        "choice ID must be canonical UUIDv4"
    );
    Ok(text)
}
fn text(value: &Value, max: usize, nonempty: bool) -> Result<()> {
    let text = value.as_str().context("approval content must be text")?;
    ensure!(
        text.len() <= max && (!nonempty || !text.trim().is_empty()),
        "invalid approval text length"
    );
    Ok(())
}
pub fn metadata(value: &Value) -> Result<()> {
    ensure!(value["version"] == 1, "unsupported approval version");
    match value["kind"].as_str() {
        Some("request") => {
            fields(value, &["version", "kind", "option_ids"], &[])?;
            let ids = value["option_ids"]
                .as_array()
                .context("missing option IDs")?;
            ensure!(
                (1..=8).contains(&ids.len()),
                "approval needs 1 to 8 choices"
            );
            let ids: Vec<_> = ids.iter().map(option_id).collect::<Result<_>>()?;
            ensure!(
                ids.windows(2).all(|pair| pair[0] < pair[1]),
                "option IDs must be unique and sorted"
            );
        }
        Some("decision") => {
            fields(
                value,
                &[
                    "version",
                    "kind",
                    "request_id",
                    "request_digest",
                    "choice_id",
                ],
                &[],
            )?;
            ensure!(
                value["request_id"]
                    .as_u64()
                    .is_some_and(|id| id > 0 && id <= 9_007_199_254_740_991),
                "invalid approval target"
            );
            let digest = value["request_digest"]
                .as_str()
                .and_then(|v| v.strip_prefix("sha256:"))
                .context("invalid request digest")?;
            use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
            ensure!(
                URL_SAFE_NO_PAD.decode(digest)?.len() == 32,
                "invalid request digest"
            );
            option_id(&value["choice_id"])?;
        }
        _ => anyhow::bail!("invalid approval kind"),
    }
    Ok(())
}
pub fn content(outer: &Value, body: &Value) -> Result<()> {
    metadata(outer)?;
    metadata(&body["approval"])?;
    ensure!(outer == &body["approval"], "approval metadata mismatch");
    if outer["kind"] == "request" {
        fields(
            body,
            &["type", "title", "options", "approval"],
            &["description"],
        )?;
        ensure!(body["type"] == "approval_request", "approval type mismatch");
        text(&body["title"], 256, true)?;
        if let Some(description) = body.get("description") {
            text(description, 65536, false)?;
        }
        let options = body["options"]
            .as_array()
            .context("missing approval options")?;
        let mut ids = BTreeSet::new();
        let mut labels = BTreeSet::new();
        for option in options {
            fields(option, &["id", "label"], &[])?;
            ensure!(ids.insert(option_id(&option["id"])?), "duplicate choice ID");
            text(&option["label"], 2000, true)?;
            ensure!(
                labels.insert(option["label"].as_str().unwrap()),
                "duplicate choice label"
            );
        }
        ensure!(
            json!(ids) == outer["option_ids"],
            "approval option set mismatch"
        );
    } else {
        fields(
            body,
            &[
                "type",
                "reply_to",
                "request_digest",
                "choice_id",
                "approval",
            ],
            &["comment"],
        )?;
        ensure!(
            body["type"] == "approval_decision"
                && body["reply_to"] == outer["request_id"]
                && body["request_digest"] == outer["request_digest"]
                && body["choice_id"] == outer["choice_id"],
            "approval target or choice mismatch"
        );
        if outer["choice_id"] == OTHER_CHOICE {
            text(&body["comment"], 65536, true)?;
        }
        if let Some(comment) = body.get("comment") {
            text(comment, 65536, false)?;
        }
    }
    Ok(())
}
pub fn request(title: Value, description: Option<Value>, labels: Value) -> Result<Value> {
    let labels = labels
        .as_array()
        .context("options must be an array of labels")?;
    let options: Vec<_> = labels
        .iter()
        .map(|label| json!({"id":Uuid::new_v4().to_string(),"label":label}))
        .collect();
    let mut ids: Vec<_> = options.iter().map(|option| option["id"].clone()).collect();
    ids.sort_by(|a, b| a.as_str().cmp(&b.as_str()));
    let mut body = json!({"type":"approval_request","title":title,"options":options,"approval":{"version":1,"kind":"request","option_ids":ids}});
    if let Some(description) = description {
        body["description"] = description;
    }
    content(&body["approval"], &body)?;
    Ok(body)
}
pub fn authenticated(message: &Value) -> bool {
    message["auth"]["verified"] == true || message["auth"]["server_authenticated"] == true
}
pub fn decision(
    request: &Value,
    choice: Value,
    comment: Option<Value>,
    supplied_digest: Option<&Value>,
) -> Result<Value> {
    ensure!(
        authenticated(request) && request["auth"]["approval_verified"] == true,
        "approval requires a verified request"
    );
    ensure!(
        request["approval"]["kind"] == "request",
        "target is not an approval request"
    );
    content(&request["approval"], &request["payload"])?;
    let digest = &request["auth"]["action_digest"];
    ensure!(
        supplied_digest.is_none_or(|supplied| supplied == digest),
        "approval request digest mismatch"
    );
    ensure!(
        (choice == OTHER_CHOICE
            || choice == REJECT_CHOICE
            || request["approval"]["option_ids"]
                .as_array()
                .unwrap()
                .contains(&choice)),
        "choice is not in this request"
    );
    let mut body = json!({"type":"approval_decision","reply_to":request["id"],"request_digest":digest,"choice_id":choice,"approval":{"version":1,"kind":"decision","request_id":request["id"],"request_digest":digest,"choice_id":choice}});
    if let Some(comment) = comment {
        body["comment"] = comment;
    }
    content(&body["approval"], &body)?;
    Ok(body)
}
/// A standalone decision needs its referenced request before it grants authority.
pub fn bind_decision(request: &Value, decision: &Value) -> Result<()> {
    ensure!(
        authenticated(request)
            && authenticated(decision)
            && request["auth"]["approval_verified"] == true
            && decision["auth"]["approval_verified"] == true
            && decision["auth"]["approval_authorized"] == true,
        "unverified approval pair"
    );
    content(&request["approval"], &request["payload"])?;
    content(&decision["approval"], &decision["payload"])?;
    ensure!(
        request["approval"]["kind"] == "request"
            && decision["approval"]["kind"] == "decision"
            && decision["approval"]["request_id"] == request["id"]
            && decision["approval"]["request_digest"] == request["auth"]["action_digest"]
            && (decision["approval"]["choice_id"] == OTHER_CHOICE
                || decision["approval"]["choice_id"] == REJECT_CHOICE
                || request["approval"]["option_ids"]
                    .as_array()
                    .unwrap()
                    .contains(&decision["approval"]["choice_id"])),
        "decision does not match request"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shared_encrypted_vectors_and_substitution_fail_closed() -> Result<()> {
        let fixture: Value =
            serde_json::from_str(include_str!("../tests/fixtures/e2ee-approvals-v1.json"))?;
        let recovery = &fixture["recovery"];
        let key = crate::e2ee::Recovery::parse(
            &recovery.to_string(),
            recovery["server_id"].as_str().unwrap(),
            recovery["project_id"].as_str().unwrap(),
        )?;
        for name in ["request", "decision"] {
            let row = &fixture[name];
            let clear = key.decrypt(fixture["member"].as_str().unwrap(), &row["payload"])?;
            assert_eq!(clear, row["clear"]);
            content(&row["approval"], &clear)?;
            assert_eq!(
                crate::signed::digest(row["action_json"].as_str().unwrap().as_bytes()),
                row["action_digest"]
            );
            let mut wrong = clear.clone();
            wrong["approval"]["version"] = json!(2);
            assert!(content(&row["approval"], &wrong).is_err());
            wrong = clear.clone();
            wrong["extra"] = json!(true);
            assert!(content(&row["approval"], &wrong).is_err());
        }
        let mut request = json!({"id":42,"approval":fixture["request"]["approval"],"payload":fixture["request"]["clear"],"auth":{"server_authenticated":true,"approval_verified":true,"action_digest":fixture["request"]["action_digest"]}});
        let choice = fixture["decision"]["clear"]["choice_id"].clone();
        let rejected = decision(&request, json!(REJECT_CHOICE), None, None)?;
        assert!(rejected.get("comment").is_none());
        let rejected_answer = json!({"approval":rejected["approval"],"payload":rejected,"auth":{"verified":true,"approval_verified":true,"approval_authorized":true}});
        bind_decision(&request, &rejected_answer)?;
        for field in ["request_id", "request_digest", "choice_id"] {
            let mut bad = rejected_answer.clone();
            bad["approval"][field] = json!("tampered");
            assert!(bind_decision(&request, &bad).is_err());
        }
        let mut unauthorized = rejected_answer.clone();
        unauthorized["auth"]["approval_authorized"] = json!(false);
        assert!(bind_decision(&request, &unauthorized).is_err());
        assert!(decision(&request, json!(OTHER_CHOICE), None, None).is_err());
        assert!(decision(&request, json!(OTHER_CHOICE), Some(json!(" ")), None).is_err());
        let other = decision(
            &request,
            json!(OTHER_CHOICE),
            Some(json!("Check staging first")),
            None,
        )?;
        let other_answer = json!({"approval":other["approval"],"payload":other,"auth":{"verified":true,"approval_verified":true,"approval_authorized":true}});
        bind_decision(&request, &other_answer)?;
        let mut wrong = other_answer.clone();
        wrong["approval"]["request_digest"] = json!("wrong");
        assert!(bind_decision(&request, &wrong).is_err());
        let body = decision(&request, choice.clone(), None, None)?;
        let mut answer = json!({"approval":body["approval"],"payload":body,"auth":{"verified":true,"approval_verified":true,"approval_authorized":true}});
        bind_decision(&request, &answer)?;
        answer["auth"]["approval_authorized"] = json!(false);
        assert!(bind_decision(&request, &answer).is_err());
        request["auth"]["approval_verified"] = json!(false);
        assert!(decision(&request, choice, None, None).is_err());
        Ok(())
    }
    #[test]
    fn builders_validate_labels_and_keep_ids_opaque() -> Result<()> {
        let one = request(json!("Deploy?"), None, json!(["yes", "no"]))?;
        let two = request(json!("Deploy?"), None, json!(["yes", "no"]))?;
        assert_ne!(one["approval"], two["approval"]);
        assert!(request(json!("あ".repeat(86)), None, json!(["yes"])).is_err());
        assert!(request(json!("Deploy?"), None, json!(["yes", "yes"])).is_err());
        assert!(request(json!("Deploy?"), None, json!([])).is_err());
        Ok(())
    }
}
