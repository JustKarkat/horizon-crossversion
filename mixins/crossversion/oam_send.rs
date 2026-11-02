// Rewrites the client's empty answer to ViaProxy's OpenAuthMod query.
fn answer(body: &[u8]) -> &[u8] {
    crate::crossversion::answer(body)
}
