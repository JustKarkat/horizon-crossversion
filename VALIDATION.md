# Validation

Validated on Linux (CachyOS, x86-64) on 2026-11-01 with Rust 1.95.0, using the
exact revisions in `compatibility.json` and the bundle in this repository.

| Check | Result |
|---|---|
| `cargo fmt --all --check` | Pass |
| `cargo clippy --workspace --all-targets --locked --offline -- -D warnings` | Pass |
| `cargo test --workspace --locked --offline` | 5 protocol tests pass |
| `python3 tools/test_stage.py` | The pinned export stages deterministically; the Horizon checkout is unchanged |
| `python3 tools/horizon.py check` | The patched `horizon-client` compiles (`--no-default-features`) |
| `cargo test -p horizon-protocol --lib crossversion` (staged tree) | 5 pass, inside Horizon |
| `cargo test -p horizon-client --lib crossversion` (staged tree) | 2 pass, inside Horizon |
| `python3 tools/horizon.py build` | A playable `horizon-client` binary |
| `python3 tools/package.py` | `target/crossversion-0.1.0.hnmod`, 13,721 bytes (source only) |

## Live servers

A build from `tools/horizon.py build` was run offscreen and joined through
`horizon-ctl`, with the client's normal resource folder.

| Server | Result |
|---|---|
| Vanilla 1.20.1, `online-mode=false`, port 25581 (protocol 763) | Joined; 201 chunks, 83 entities, health 20; chat arrived on the server as `<Tester> hello from 1.20.1 via viaproxy`; the server logged the login |
| Vanilla 1.21.1, `online-mode=false`, port 25582 (protocol 767) | Joined; 201 chunks, 14 entity types |
| Vanilla 26.3 (the client's own version) | No routing at all: no `[crossversion]` line, no JVM started |
| Vanilla 1.20.1, `online-mode=true`, port 25584, fixed answer (`HORIZON_CROSSVERSION_AUTH=ok`) | ViaProxy's `oam:join` query was answered and the proxy went on to log in; the server refused it with "invalid session" because the answer was a fixed `true`, not a Mojang join. Shows the answer reaches the proxy intact |
| Vanilla 1.20.1, `online-mode=true`, port 25584, **real account** | **Joined, authenticated as the signed-in account**: the server logged `UUID of player JustKarkat is 5166e6e4-…`, `JustKarkat[…] logged in`, `JustKarkat joined the game`; the client had 201 chunks. The session came from `horizon-launcher play --arg --help`, which writes the session file and exits before the game reads it |
| Vanilla 26.3, `online-mode=true`, port 25585, direct (no proxy) | Joined as the account; chat is disabled by the client itself: "Chat disabled due to missing profile public key" |
| First run with no cached jar | ViaProxy 3.4.14 downloaded (49 MB), SHA-256 verified, joined |
| No Java on the machine (`HORIZON_CROSSVERSION_INSTALL_JAVA=1`) | Mojang's `java-runtime-delta` (Java 21, 423 files) installed into `<game home>/runtime/`, then joined through it |
| Leaving the server | The proxy process is gone and `proxies.json` is empty |

## Not covered

- **Secure-profile chat.** With `enforce-secure-profile=true` (the default on
  online-mode servers since 1.19.3) Horizon's chat is disabled whether or not
  the proxy is used: the client sends unsigned chat (`signature: None`,
  `send_chat` in `game/remote.rs`) and never sends a `ChatSessionUpdate`, so
  the server reports no profile key and the client shows "Chat disabled due to
  missing profile public key". This is a Horizon gap, not a proxy one —
  commands still work. Fixing it needs player certificates from
  `api.minecraftservices.com/player/certificates` and signing in the client.
- **Bedrock** targets.
- Windows and macOS: the process cleanup and Java discovery have platform
  branches that compiled but were not run. The pid reaping verifies the command
  line on Linux (`/proc`) and macOS (`ps`); on Windows it is disabled.
- A machine where the Mojang Java download fails (no network, blocked host).

The staging tool plans every injection against pristine ASTs and refuses
anything outside its proven subset; the compiled and tested patched tree is the
authority that the transform did what the plan said.
