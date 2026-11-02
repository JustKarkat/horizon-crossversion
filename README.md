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
itself covers servers from 1.0 to 26.3. An **online-mode** 1.20.1 server
accepts the player's own account through the proxy.

One caveat, and it is Horizon's rather than the proxy's: on servers with
`enforce-secure-profile=true` (the default for online-mode servers since
1.19.3) Horizon's chat is disabled, with or without this mod, because the
client does not hold a player certificate. Commands still work.

## What it does, precisely

| Step | Where |
|---|---|
| Reads the server's protocol from its status ping | `route()` in `engine.rs` |
| Reuses a running proxy or starts one (finds a free port, waits for it to listen) | `ensure_proxy()` |
| Rewrites the join's address to `127.0.0.1:<port>` | `route.rs`, a `MODIFY_ARG` hook on `Session::connect` / `Session::connect_transferred` |
| Answers ViaProxy's `oam:join` login query with the player's own account | `proto.rs` + a `RETURN` hook on `Connection::recv` and a `MODIFY_ARG` hook on `Connection::send` |
| Stops the proxy when the player leaves | `session_end.rs`, a `HEAD` hook on `Session::disconnect` |
| Kills proxies a crash left behind | `reap_once()`, a pid file in the mod's data folder |

### Downloads, and why this costs little

On first use the mod downloads two GPL-3.0 jars it does not bundle — ViaProxy
and the `ViaProxyOpenAuthMod` plugin — into
`<game folder>/horizon-mods/.horizon-mod-data/crossversion/viaproxy/`, checking
each against a SHA-256 pinned in the source. If the machine has no Java 17 or
newer, it installs Mojang's own Java 21 runtime (the same files the official
launcher uses, sha1-checked) into `<game home>/runtime/`, once per machine.

**Most Minecraft servers are Java programs already.** The cost here is one JVM
on the player's machine, started only for a join that needs translation, on a
loopback socket — and it is a separate process, so it takes no frame time from
the game. The game thread blocks once, while the proxy opens its port (about a
second with the jar cached), not per packet. If you were going to run a
Minecraft server, you were going to run a JVM anyway.

## Requirements

- Horizon 26.3 (Nightly 2026-10-30 or anything whose source contains the pinned
  commit).
- Java 17+ — or nothing, in which case Mojang's runtime is downloaded on demand
  (about 180 MB, once).
- Internet the first time: ViaProxy is about 49 MB.

## Installing

The mod is native code compiled into Horizon, so the launcher builds it:

```sh
horizon-launcher native-build path/to/crossversion-0.1.0.hnmod --trust-native-code
```

Settings live in `horizon-mods/.horizon-mod-data/crossversion/config.json`:

```json
{
  "enabled": true,
  "always": false,
  "java": null
}
```

`always` routes every join through the proxy, including 26.3 servers (useful
when comparing behaviour). `java` names a `java` binary to use instead of the
one the mod finds or installs. Environment overrides for quick experiments:
`HORIZON_CROSSVERSION=0` disables the mod, `HORIZON_CROSSVERSION_JAVA=...` picks
a Java, `HORIZON_CROSSVERSION_DEBUG=1` copies the proxy's log into the game's
log, `HORIZON_CROSSVERSION_INSTALL_JAVA=1` ignores every installed Java and
installs Mojang's.

## What it does not do

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
python3 tools/package.py                         # target/crossversion-<version>.hnmod
```

The two modules are injected into the game: `proto.rs` into
`crates/horizon-protocol/src/lib.rs` and `engine.rs` into
`crates/horizon-client/src/net.rs`. The hook adapters are `route.rs`,
`oam_recv.rs`, `oam_send.rs` and `session_end.rs`.

## Licence

The mod is MIT (see [LICENSE](LICENSE)). ViaProxy and ViaProxyOpenAuthMod are
downloaded at run time under GPL-3.0, unmodified, and are not part of this
repository or of the `.hnmod`.
