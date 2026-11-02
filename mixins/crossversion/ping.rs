// The server list's status ping: a server this mod would route answers as if it
// spoke this game's protocol, with its version in green above the MOTD.
fn note(address: &str) -> std::ops::ControlFlow<Result<(serde_json::Value, u128), String>> {
    crate::net::crossversion::ping_head(address)
}
