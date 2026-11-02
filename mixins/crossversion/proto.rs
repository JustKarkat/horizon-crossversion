// Cross-version support (horizon-crossversion): the OpenAuthMod half.
//
// Injected into `crates/horizon-protocol/src/lib.rs` by horizon-mixins and
// compiled into the game. This file is also the mod's standalone library, so
// it uses `std` only and no Horizon types.

/// Whether a translated connection is in play; set by the client when it hands
/// a join to a local ViaProxy.
static ARMED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The account's `joinServer` call, registered by the client. ViaProxy asks the
/// client to authenticate the target server's session hash; only the client
/// holds the player's token.
static AUTH: std::sync::Mutex<Option<AuthFn>> = std::sync::Mutex::new(None);

/// A login transaction with an OpenAuthMod query that still needs its answer.
static PENDING: std::sync::Mutex<Option<Pending>> = std::sync::Mutex::new(None);

type AuthFn = Box<dyn Fn(&str) -> bool + Send + Sync>;

/// A panic inside an authenticator must not wedge every later connection.
fn lock<T>(mutex: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Pending {
    transaction: i32,
    success: bool,
}

/// The login-state clientbound id of `ClientboundCustomQueryPacket`.
const LOGIN_CUSTOM_QUERY: i32 = 4;
/// The login-state serverbound id of `ServerboundCustomQueryAnswerPacket`.
const LOGIN_CUSTOM_QUERY_ANSWER: i32 = 2;
/// ViaProxy's OpenAuthMod channel.
const OAM_CHANNEL: &str = "oam:join";

/// Arm the hooks for a connection that goes through a translating proxy, with
/// the authenticator that joins the target server's hash.
pub fn arm(authenticator: AuthFn) {
    *lock(&AUTH) = Some(authenticator);
    *lock(&PENDING) = None;
    ARMED.store(true, std::sync::atomic::Ordering::SeqCst);
}

/// Disarm: the connection ended, or it never went through a proxy.
pub fn disarm() {
    ARMED.store(false, std::sync::atomic::Ordering::SeqCst);
    *lock(&AUTH) = None;
    *lock(&PENDING) = None;
}

pub fn armed() -> bool {
    ARMED.load(std::sync::atomic::Ordering::SeqCst)
}

/// Called for every received packet. Cheap when disarmed.
pub fn note_packet(id: i32, payload: &[u8]) {
    // Login packets are small; chunks and other play packets are not.
    if !armed() || id != LOGIN_CUSTOM_QUERY || payload.len() > 1024 {
        return;
    }
    let Some((transaction, channel, data)) = parse_custom_query(payload) else {
        return;
    };
    if channel != OAM_CHANNEL {
        return;
    }
    let Some(hash) = read_varint_str(data).filter(|hash| !hash.is_empty()) else {
        return;
    };
    let success = match lock(&AUTH).as_ref() {
        Some(auth) => auth(hash),
        None => false,
    };
    *lock(&PENDING) = Some(Pending {
        transaction,
        success,
    });
}

/// Called for every sent packet body (VarInt id then payload). Replaces the
/// client's `payload: None` answer with the OpenAuthMod reply.
pub fn answer(body: &[u8]) -> &[u8] {
    if !armed() {
        return body;
    }
    let Some((id, rest)) = read_varint(body) else {
        return body;
    };
    if id != i64::from(LOGIN_CUSTOM_QUERY_ANSWER) {
        return body;
    }
    let Some((transaction, payload)) = read_varint(rest) else {
        return body;
    };
    let Ok(transaction) = i32::try_from(transaction) else {
        return body;
    };
    // Only an empty answer for the transaction we authenticated.
    if payload != [0] {
        return body;
    }
    let pending = match *lock(&PENDING) {
        Some(pending) if pending.transaction == transaction => pending,
        _ => return body,
    };
    *lock(&PENDING) = None;
    let mut rewritten = Vec::with_capacity(body.len() + 2);
    write_varint(i64::from(LOGIN_CUSTOM_QUERY_ANSWER), &mut rewritten);
    write_varint(i64::from(transaction), &mut rewritten);
    rewritten.push(1); // Option::Some
    rewritten.push(u8::from(pending.success)); // the OpenAuthMod result byte
    // The connection outlives this packet, so the slice has to as well. One
    // small buffer per authenticated connection.
    Box::leak(rewritten.into_boxed_slice())
}

/// `ClientboundCustomQueryPacket`: VarInt transaction, Ident channel, rest.
fn parse_custom_query(payload: &[u8]) -> Option<(i32, String, &[u8])> {
    let (transaction, rest) = read_varint(payload)?;
    let (channel, data) = read_ident(rest)?;
    Some((i32::try_from(transaction).ok()?, channel, data))
}

fn read_ident(bytes: &[u8]) -> Option<(String, &[u8])> {
    let (length, rest) = read_varint(bytes)?;
    let length = usize::try_from(length).ok()?;
    if length > 32767 || rest.len() < length {
        return None;
    }
    let (name, rest) = rest.split_at(length);
    Some((std::str::from_utf8(name).ok()?.to_owned(), rest))
}

fn read_varint_str(bytes: &[u8]) -> Option<&str> {
    let (length, rest) = read_varint(bytes)?;
    let length = usize::try_from(length).ok()?;
    if rest.len() < length {
        return None;
    }
    std::str::from_utf8(&rest[..length]).ok()
}

/// A Minecraft VarInt: up to five bytes, seven bits each, little endian.
fn read_varint(bytes: &[u8]) -> Option<(i64, &[u8])> {
    let mut value: i64 = 0;
    for (i, &byte) in bytes.iter().take(5).enumerate() {
        value |= i64::from(byte & 0x7F) << (7 * i);
        if byte & 0x80 == 0 {
            return Some((value, &bytes[i + 1..]));
        }
    }
    None
}

fn write_varint(value: i64, out: &mut Vec<u8>) {
    let mut value = value as u32;
    loop {
        if value & !0x7F == 0 {
            out.push(value as u8);
            return;
        }
        out.push((value & 0x7F) as u8 | 0x80);
        value >>= 7;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The statics are shared by the whole process; run the tests one at a time.
    static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn exclusive() -> std::sync::MutexGuard<'static, ()> {
        lock(&TEST_LOCK)
    }

    #[test]
    fn sees_the_query_and_answers_it() {
        let _guard = exclusive();
        arm(Box::new(|hash| {
            assert_eq!(hash, "abc123");
            true
        }));
        // CustomQuery: id 4, transaction 7, channel "oam:join", hash "abc123".
        let mut payload = vec![7, 8];
        payload.extend_from_slice(b"oam:join");
        payload.push(6);
        payload.extend_from_slice(b"abc123");
        note_packet(LOGIN_CUSTOM_QUERY, &payload);
        // The client answers with no payload; we fill it in.
        assert_eq!(answer(&[2, 7, 0]), &[2, 7, 1, 1]);
        // Once answered, the next one passes through.
        assert_eq!(answer(&[2, 7, 0]), &[2, 7, 0]);
        disarm();
    }

    #[test]
    fn other_packets_are_untouched() {
        let _guard = exclusive();
        arm(Box::new(|_| panic!("must not authenticate")));
        // A different channel on the same packet id.
        note_packet(4, b"\x01\x0bserver:test\x00");
        // The OAM channel with no hash at all.
        note_packet(4, b"\x01\x08oam:join\x00");
        assert_eq!(answer(&[2, 9, 0]), &[2, 9, 0]); // a different transaction
        assert_eq!(answer(&[3, 1, 0]), &[3, 1, 0]); // a different packet
        assert_eq!(answer(&[2, 1, 1, 0]), &[2, 1, 1, 0]); // already has a payload
        disarm();
    }

    #[test]
    fn a_failed_join_answers_false() {
        let _guard = exclusive();
        arm(Box::new(|_| false));
        let mut payload = vec![3, 8];
        payload.extend_from_slice(b"oam:join");
        payload.push(1);
        payload.extend_from_slice(b"h");
        note_packet(LOGIN_CUSTOM_QUERY, &payload);
        assert_eq!(answer(&[2, 3, 0]), &[2, 3, 1, 0]);
        disarm();
    }

    #[test]
    fn varints_round_trip() {
        for value in [
            0i64,
            1,
            127,
            128,
            255,
            300,
            16_383,
            2_097_151,
            i64::from(i32::MAX),
        ] {
            let mut bytes = Vec::new();
            write_varint(value, &mut bytes);
            assert_eq!(read_varint(&bytes), Some((value, &[][..])));
        }
    }

    #[test]
    fn malformed_packets_are_ignored() {
        let _guard = exclusive();
        arm(Box::new(|_| panic!("must not authenticate")));
        note_packet(LOGIN_CUSTOM_QUERY, &[]);
        note_packet(LOGIN_CUSTOM_QUERY, &[0, 200]); // channel longer than the packet
        note_packet(LOGIN_CUSTOM_QUERY, &[0, 4, b'x']); // truncated channel
        assert_eq!(answer(&[2]), &[2]); // truncated body
        disarm();
    }
}
