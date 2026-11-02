# Validation

Validated on Linux (CachyOS, x86-64) on 2026-11-01 with Rust 1.95.0, using the
exact revisions in `compatibility.json` (Horizon Nightly 2026-11-01,
`07b7ea3`) and the bundle in this repository.

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
| Vanilla 1.20.1, `online-mode=true`, port 25584 | **No longer supported (0.5.0).** The registry review found that answering ViaProxy's `oam:join` query means signing any hash a joined server can inject; the mod now starts ViaProxy with `--auth-method NONE` and never calls `join_server`, so online-mode servers refuse the join |
| Vanilla 1.21.1, port 25582, chat | `[Not Secure] <Tester7> hello 1.21.1` (offline server, no profile key possible — correct) |
| Vanilla 26.3, `online-mode=true`, port 25585, direct (no proxy) | No proxy started (0 routing lines); `<JustKarkat> 26.3 secure chat after the update` — signed chat works against a same-version server too |
| First run with no cached jar | ViaProxy 3.4.14 downloaded (49 MB), SHA-256 verified, joined |
| No Java on the machine | **No longer downloads (0.5.0):** it reports that Java 17+ is needed and does not start the proxy. An existing runtime under `<game home>/runtime/` (including `jre.bundle/Contents/Home`) is used |
| Leaving the server | The proxy process is gone and `proxies.json` is empty |

## Server list

A windowed build was opened on the title screen, Multiplayer clicked, and the
list screenshotted with the player's own `servers.dat`:

| Entry | Shown |
|---|---|
| Horizon Official (26.3) | unchanged: no badge, green bars |
| Community SMP (status: PaperSpigot 1.8.8) | `PaperSpigot 1.8.8 · via ViaProxy` in green, green bars, 1/25 |
| MC.Jungle (status: BungeeCord 1.8.x-1.21.x) | `BungeeCord 1.8.x-1.21.x · via ViaProxy`, green bars, 0/100 |

A network whose status echoes the client's protocol (Minevia, "Minevia Proxy")
cannot be told apart, so it gets no badge; naming it in `servers` with the
version its lobbies speak makes the join work (verified: `[26.3 <-> 1.8.x]`,
configuration finished, play state).

The tooltip on the badge was not done: the mixin tool refuses `RETURN` hooks in
functions containing `?` or closures, and both `ping_tooltip` and the list
drawing code contain them. The badge text carries the explanation instead.

## Not covered

- **Horizon older than Nightly 2026-11-01.** Those builds send unsigned chat
  (`signature: None`) and no `ChatSessionUpdate`, so a secure-profile server
  refuses chat with "Chat disabled due to missing profile public key". This mod
  now pins `07b7ea3`, which has the client-side chat session (Horizon commit
  `eaf1507`, "Secure chat on a server's client"). Horizon issue #5936 tracks the
  old behaviour.
- **Bedrock** targets.
- Windows and macOS: the process cleanup and Java discovery have platform
  branches that compiled but were not run. The pid reaping verifies the command
  line on Linux (`/proc`) and macOS (`ps`); on Windows it is disabled.
- A machine where the Mojang Java download fails (no network, blocked host).

The staging tool plans every injection against pristine ASTs and refuses
anything outside its proven subset; the compiled and tested patched tree is the
authority that the transform did what the plan said.
