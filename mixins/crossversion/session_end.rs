// A session ended: the proxy that served it has nothing left to do.
fn session_end(session: &crate::net::Session) {
    crate::net::crossversion::session_ended(&session.address);
}
