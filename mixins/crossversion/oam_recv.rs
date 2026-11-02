// Runs on every received packet; records ViaProxy's OpenAuthMod login query.
fn observe(value: Result<(i32, Vec<u8>)>) -> Result<(i32, Vec<u8>)> {
    if let Ok((id, payload)) = &value {
        crate::crossversion::note_packet(*id, payload);
    }
    value
}
