# horizon-crossversion

A native Horizon mod that lets the 26.3 client join servers that speak an older
Minecraft protocol.

Horizon talks exactly one protocol (777, Minecraft 26.3). This mod watches a
join, asks the server what protocol it speaks in its status reply, and — if it
is not 26.3 — starts a [ViaProxy](https://github.com/ViaVersion/ViaProxy)
instance on a loopback port and points the join at it. The proxy speaks 26.3 to
Horizon and the server's own version to the server. Nothing else in the client
changes, and a join to a 26.3 server never starts anything.

```
Horizon client ──26.3──▶ 127.0.0.1:<port>  ViaProxy  ──server's version──▶ the server
```

Tested against a vanilla 1.20.1 server (protocol 763) and a 1.21.1 server
(767): world load, chunks, entities, chat and the tab list all work. ViaProxy
itself covers servers from 1.0 to 26.3.

**Offline-mode servers only.** Horizon never signs a session hash: answering
ViaProxy's OpenAuthMod query would mean signing whatever hash arrives in a
login query, and a server a player joins can send one too, so it could have the
client sign a hash for a server it runs and log in as the player there —
Horizon's registry review found exactly that in 0.4.0.

0.6.0–0.7.1 instead let ViaProxy hold a Microsoft account of its own, but that
sign-in uses the official Minecraft launcher's client id (`00000000402b5328`),
and Horizon's registry may only sign in through its own Mojang-approved app. So
the sign-in is gone: online-mode servers cannot be joined. When an updated
install first uses the mod, the accounts a previous version stored in
ViaProxy's `saves.json` are deleted (`accountsV4` is emptied), together with
the old `viaproxy/plugins/` folder; the client never had them.
Online mode can return with a design that uses Horizon's own session.

Signed chat still works on offline-mode servers: from Horizon's Nightly
2026-11-01 on, the client holds a chat key pair and signs its messages
(`<JustKarkat> …`, not `[Not Secure]`). On Horizon before that nightly the
mod's 0.1.x pinned an earlier commit.

## What it does, precisely

| Step | Where |
|---|---|
| Reads the server's protocol from its status ping | `route()` in `engine.rs` |
| Reuses a running proxy or starts one (finds a free port, waits for it to listen) | `ensure_proxy()` |
| Rewrites the join's address to `127.0.0.1:<port>` | `route.rs`, a `MODIFY_ARG` hook on `Session::connect` / `Session::connect_transferred` |
| Shows a joinable server as compatible in the server list | `ping.rs`, a cancellable `HEAD` on `net::ping`, with `decisions.rs`
| Stops the proxy when the player leaves | `session_end.rs`, a `HEAD` hook on `Session::disconnect` |
| Kills proxies a crash left behind | `reap_once()`, a pid file in the mod's data folder |

### Choosing the version

**Edit Server** and **Direct Connect** draw one extra line under their buttons:
`ViaProxy asks the server its version (click to choose)`. Clicking it steps
through `auto`, `1.8`, `1.9`, `1.12.2`, `1.16.5`, `1.20.1`, `1.21.1`, `1.21.4`
and `26.3` for that address, and keeps the choice in the config's `servers`
map. It is the version ViaProxy tells the server it is, which is what a server
whose status reports the wrong version needs — that is the address in front of
the colon in the status, not the client: Horizon always speaks 26.3 to the
proxy, and pretending otherwise would only make the translation wrong. The line
is green when a version is chosen and grey when it is `auto`.

### The server list

A server whose status says it speaks another version is listed as joinable
rather than with the red "Incompatible version!" cross: its own version appears
in green above the MOTD (`PaperSpigot 1.8.8 · via ViaProxy`) and its ping bars
stay green. Pinging is done by the mod for that judgement, with the real status
request, so the version shown is the server's.

One case the status cannot describe: a network whose front (BungeeCord,
Velocity) echoes whatever protocol the client speaks while its lobbies are
older. It looks like 26.3 from the outside, so nothing is routed and the lobby
rejects the join with "Outdated server! I'm still on 1.8.8". Name it in the
config with the version its lobbies speak:

```json
{
  "servers": { "minevia.club": "1.8" }
}
```

The join then goes through the proxy without asking the server first.

### Downloads, and why this costs little

On first use the mod downloads one GPL-3.0 jar it does not bundle — ViaProxy —
into
`<game folder>/horizon-mods/.horizon-mod-data/crossversion/viaproxy/`, checking
it against a SHA-256 pinned in the source. Nothing else is downloaded: Java 17+
must already be on the machine (see Requirements).

**Most Minecraft servers are Java programs already.** The cost here is one JVM
on the player's machine, started only for a join that needs translation, on a
loopback socket — and it is a separate process, so it takes no frame time from
the game. The game thread blocks once, while the proxy opens its port (about a
second with the jar cached), not per packet. If you were going to run a
Minecraft server, you were going to run a JVM anyway.

The proxy runs in an empty directory of its own for each join (no `plugins/`
folder, a `viaproxy.yml` written fresh), because the mod's data folder is one a
modpack can write: nothing planted there is loaded. Java is taken only from
`HORIZON_CROSSVERSION_JAVA`, `JAVA_HOME`, `PATH` or the system's own install
folders, and never from a path inside the game folder (a `java` in
`config.json` is ignored).

## Requirements

- Horizon 26.3 (the pinned commit, `07b7ea3`).
- Java 17+, installed outside the game folder. Set `HORIZON_CROSSVERSION_JAVA`
  to an absolute `java` path if the mod cannot find one.
- Internet the first time: ViaProxy is about 49 MB.
- Offline-mode servers. Online mode is not supported.

## Installing

The mod is native code compiled into Horizon, so the launcher builds it:

```sh
horizon-launcher native-build path/to/crossversion-0.8.0.hnmod --trust-native-code
```

The settings page's "Delete the downloaded ViaProxy" removes the jar so the
next join fetches it again. An install updated from 0.6.0–0.7.1 also has the
Microsoft account those versions stored for ViaProxy deleted the next time
the mod is used.

Settings live in `horizon-mods/.horizon-mod-data/crossversion/config.json`:

```json
{
  "enabled": true,
  "always": false
}
```

`always` routes every join through the proxy, including 26.3 servers (useful
when comparing behaviour). A `java` key is ignored: Java comes from
`HORIZON_CROSSVERSION_JAVA` (an absolute path outside the game folder),
`JAVA_HOME`, `PATH` or the system's install folders, never from this
modpack-writable file. Environment overrides for quick experiments:
`HORIZON_CROSSVERSION=0` disables the mod, `HORIZON_CROSSVERSION_DEBUG=1`
copies the proxy's log into the game's log.

## What it does not do

- **Online-mode servers.** No account is used or stored; the proxy logs in
  offline.
- **Bedrock.** ViaProxy's Bedrock support is incomplete and needs Geyser; not
  wired up here.
- **Old clients joining a Horizon world.** That is the other direction
  (ViaBackwards), and this mod does not touch the integrated server.
- **Server-side anti-cheat.** Horizon sends `minecraft:brand = horizon`;
  translation does not make it look like a vanilla client.
- **Fix Horizon's own protocol bugs.** If Horizon is not protocol-exact for
  26.3, a join through the proxy will hit it. That is a bug in the client, not
  in the proxy — and this mod is a good way to find it.

## Building and testing

Requirements: Linux, Rust 1.95.0, Python 3.12+, Git, Cargo, and checkouts of
Horizon and horizon-mixins matching `compatibility.json` (Horizon
`ba6344a5…`, horizon-mixins `d9c83d7…`).

```sh
cargo test --workspace --locked --offline        # the protocol logic (5 tests)
python3 tools/horizon.py check                   # the bundle compiles into Horizon
python3 tools/horizon.py build                   # a game binary to run
python3 tools/horizon.py test                    # the mod's tests inside the game
# the registry artifact comes from the registry repo's own tool:
#   python3 native/tools/native.py package crossversion   (in a horizon-mods clone)
```

The modules are injected into the game: `decisions.rs` and `engine.rs` into
`crates/horizon-client/src/net.rs`, and `ui.rs` into
`crates/horizon-client/src/game/mods.rs`. The hook adapters are `route.rs`,
`ping.rs`, `ui_mods.rs`, `ui_settings.rs`, `ui_click.rs` and `session_end.rs`.

![Cross-version](icon.png)

## Signing

Releases are signed with [minisign](https://jedisct1.github.io/minisign/). The
registry entry (`registry/crossversion.toml`) pins this public key:

```
RWTcPJHJYjPW0pV3lPwr45crCu0wJxW5HbDLc3fr3Eom1TzxO++kHOo4
```

Sign a new version before submitting it:

```sh
minisign -S -s secret-key -m target/crossversion-<version>.hnmod
```

The secret key is never in this repository. It can be given a password without
changing the key pair (`minisign -C -s secret-key`), so the public key above
stays valid.

## Licence

The mod is MIT (see [LICENSE](LICENSE)). ViaProxy is downloaded at run time
under GPL-3.0, unmodified, and is not part of this repository or of the
`.hnmod`.
