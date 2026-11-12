//! Explicit cursors belong to the caller's room/filter/conversation, never disk state.
use crate::Client;
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};

pub fn request(mut args: Value) -> Result<Value> {
    let limit = args
        .get("limit")
        .map(|v| v.as_u64().context("invalid limit"))
        .transpose()?
        .unwrap_or(20);
    ensure!((1..=1000).contains(&limit), "limit must be 1..1000");
    args["limit"] = json!(limit);
    if let Some(after) = args.get("after") {
        ensure!(
            after.as_i64().is_some_and(|n| n >= 0),
            "invalid after cursor"
        );
    } else {
        args["before"] = json!(9007199254740991_i64);
    }
    Ok(args)
}

pub fn page(mut value: Value, after: Option<i64>) -> Result<Value> {
    let messages = value["messages"].as_array().context("missing messages")?;
    let mut next = after.unwrap_or(0);
    let mut previous = 0;
    for message in messages {
        let id = message["id"].as_i64().context("missing message id")?;
        ensure!(
            id > previous && id > after.unwrap_or(0),
            "invalid history ordering"
        );
        previous = id;
        next = id;
    }
    value["next_after"] = json!(next);
    value["initial_recent"] = json!(after.is_none());
    // Older history omitted on initial load is not a forward backlog.
    if after.is_none() {
        value["older_history_available"] = value["has_more"].clone();
        value["has_more"] = json!(false);
    }
    Ok(crate::mcp::model_view(value))
}

fn scope(value: &Value, args: &Value) -> Result<Value> {
    let mut scope = json!({"mentions":args.get("mentions").filter(|v| !v.is_null())});
    for field in ["server_id", "project_id", "member_id"] {
        scope[field] = json!(value[field].as_str().context("missing cursor identity")?);
    }
    Ok(scope)
}

pub fn complete(
    value: Value,
    args: &Value,
    cursor: Option<&Value>,
    after: Option<i64>,
) -> Result<Value> {
    let binding = scope(&value, args)?;
    if let Some(cursor) = cursor {
        ensure!(
            cursor["version"] == 1 && cursor["scope"] == binding,
            "cursor room, identity or filter mismatch; reload without cursor"
        );
    }
    let mut result = page(value, after)?;
    result["next_cursor"] = json!({"version":1,"scope":binding,"after":result["next_after"]});
    Ok(result)
}

pub async fn fetch(client: &Client, mut args: Value) -> Result<Value> {
    let cursor = args
        .as_object_mut()
        .context("invalid arguments")?
        .remove("cursor");
    if let Some(cursor) = &cursor {
        ensure!(args.get("after").is_none(), "use cursor or after, not both");
        args["after"] = cursor["after"].clone();
    }
    args["include_human"] = json!(false);
    let prepared = request(args.clone())?;
    let after = args.get("after").and_then(Value::as_i64);
    let value = client.call("history", prepared).await?;
    complete(value, &args, cursor.as_ref(), after)
}

pub async fn message(client: &Client, mut args: Value) -> Result<Value> {
    let id = args["message_id"].as_i64().context("invalid message id")?;
    ensure!(id > 0, "invalid message id");
    args.as_object_mut()
        .context("invalid arguments")?
        .remove("message_id");
    args["after"] = json!(id - 1);
    args["limit"] = json!(1);
    let value = crate::mcp::model_view(client.call("history", args).await?);
    lookup(value, id)
}

fn lookup(value: Value, id: i64) -> Result<Value> {
    let found = value["messages"]
        .as_array()
        .context("missing messages")?
        .iter()
        .find(|m| m["id"] == id)
        .cloned();
    Ok(json!({"message":found,"found":found.is_some(),"requested_id":id}))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn lookup_never_substitutes_the_next_global_id() {
        let value = json!({"messages":[{"id":21,"payload":{"message":"next"}}]});
        assert_eq!(
            lookup(value.clone(), 20).unwrap(),
            json!({"message":null,"found":false,"requested_id":20})
        );
        assert_eq!(
            lookup(value, 21).unwrap()["message"]["payload"]["message"],
            "next"
        );
        assert_eq!(lookup(json!({"messages":[]}), 20).unwrap()["found"], false);
    }

    #[test]
    fn portable_cursor_is_bound_to_room_identity_and_filter() {
        let a = json!({"server_id":"server","project_id":"room1","member_id":"alice","messages":[{"id":10}],"has_more":false});
        let first = complete(a.clone(), &json!({}), None, None).unwrap();
        let cursor: Value = serde_json::from_str(&first["next_cursor"].to_string()).unwrap();
        let empty = json!({"server_id":"server","project_id":"room1","member_id":"alice","messages":[],"has_more":false});
        assert_eq!(
            complete(empty.clone(), &json!({}), Some(&cursor), Some(10)).unwrap()["next_after"],
            10
        );
        for field in ["server_id", "project_id", "member_id"] {
            let mut other = empty.clone();
            other[field] = json!("other");
            assert!(complete(other, &json!({}), Some(&cursor), Some(10)).is_err());
        }
        assert!(complete(empty, &json!({"mentions":"bob"}), Some(&cursor), Some(10)).is_err());
        // A fresh context ignores any previous read progress unless explicitly supplied.
        assert_eq!(
            complete(a, &json!({"mentions":"bob"}), None, None).unwrap()["next_after"],
            10
        );
    }

    #[test]
    fn first_page_is_recent_and_forward_pages_are_explicit() {
        assert_eq!(request(json!({})).unwrap()["before"], 9007199254740991_i64);
        assert_eq!(request(json!({})).unwrap()["limit"], 20);
        let explicit = request(json!({"after":0,"limit":2})).unwrap();
        assert!(explicit.get("before").is_none());
        assert_eq!(explicit["after"], 0);
        assert!(request(json!({"after":-1})).is_err());
        assert!(request(json!({"limit":0})).is_err());
    }
    #[test]
    fn pagination_gaps_and_empty_pages_do_not_skip_unseen_ids() {
        let a = page(
            json!({"messages":[{"id":4},{"id":19}],"has_more":true}),
            Some(1),
        )
        .unwrap();
        assert_eq!(a["next_after"], 19);
        assert_eq!(a["has_more"], true);
        let b = page(json!({"messages":[],"has_more":false}), Some(19)).unwrap();
        assert_eq!(b["next_after"], 19);
        assert!(page(json!({"messages":[{"id":19}]}), Some(19)).is_err());
        assert!(page(json!({"messages":[{"id":5},{"id":4}]}), Some(0)).is_err());
    }
    #[test]
    fn initial_tail_distinguishes_older_history_and_has_no_persisted_cursor() {
        for _new_context in 0..2 {
            let a = page(json!({"messages":[{"id":100}],"has_more":true}), None).unwrap();
            assert_eq!(a["next_after"], 100);
            assert_eq!(a["has_more"], false);
            assert_eq!(a["older_history_available"], true);
        }
        assert_eq!(
            page(json!({"messages":[],"has_more":false}), None).unwrap()["next_after"],
            0
        );
    }
}
