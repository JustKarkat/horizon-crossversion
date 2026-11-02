// The pure decisions the mod makes, kept free of the game so they can be unit
// tested on their own: how a status becomes a server-list entry.

/// A status that is not this game's version, dressed as one.
pub fn badge(mut json: serde_json::Value, address: &str) -> serde_json::Value {
    let protocol = json["version"]["protocol"].as_i64().unwrap_or(0);
    // A legacy answer with no version, or this game's own: leave it alone.
    if protocol == 0 || protocol == GAME_PROTOCOL {
        return json;
    }
    let name = json["version"]["name"].as_str().unwrap_or("?").to_owned();
    eprintln!("[crossversion] {address} speaks {name}; the list shows it as joinable");
    let badge = serde_json::json!({
        "text": format!("{name} · via ViaProxy\n"),
        "color": "green",
    });
    let description = json
        .get("description")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    json["description"] = serde_json::Value::Array(vec![badge, description]);
    json["version"]["protocol"] = serde_json::json!(GAME_PROTOCOL);
    json
}

/// The protocol this game speaks (26.3).
const GAME_PROTOCOL: i64 = 777;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_joinable_server_is_badged_green() {
        let json = serde_json::json!({
            "version": {"name": "1.20.1", "protocol": 763},
            "description": {"text": "A server"},
        });
        let out = badge(json, "example.test");
        assert_eq!(out["version"]["protocol"], 777);
        let lines = out["description"].as_array().unwrap();
        assert_eq!(lines[0]["text"], "1.20.1 · via ViaProxy\n");
        assert_eq!(lines[0]["color"], "green");
        assert_eq!(lines[1]["text"], "A server");
    }

    #[test]
    fn the_badge_leaves_this_version_alone() {
        let json = serde_json::json!({
            "version": {"name": "26.3", "protocol": 777},
            "description": "hi",
        });
        assert_eq!(badge(json.clone(), "example.test"), json);
    }
}
