// Decides where a join goes. Returns the loopback proxy address when the server
// is not 26.3, or the original address untouched.
fn route(address: &str) -> &str {
    match crate::net::crossversion::route(address) {
        Some(rewritten) => Box::leak(rewritten.into_boxed_str()),
        None => address,
    }
}
