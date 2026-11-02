# Working on horizon-crossversion

Read README.md and compatibility.json before editing. Keep this repository
separate from Horizon and horizon-mixins. Only export committed pinned Horizon
sources into ignored target trees; never modify the original checkout or player
resources. Do not commit jars, downloaded runtimes, exported sources or
Minecraft assets.

The bundle injects two modules: `mixins/crossversion/proto.rs` into
`crates/horizon-protocol/src/lib.rs` (public, so the client can arm it) and
`mixins/crossversion/engine.rs` into `crates/horizon-client/src/net.rs`. The
engine is the mod's standalone Cargo library, so it may only use what
`horizon-client` already depends on (ureq, sha1, sha2, serde_json) — no new
dependencies: the workspace is built `--locked --offline`. `proto.rs` uses
`std` only.

Hooks are declarative in `horizon-mixin.toml`; never rewrite Rust source by
hand. Hook adapters stay tiny typed functions (`route.rs`, `oam_recv.rs`,
`oam_send.rs`, `session_end.rs`) and call into the injected module, because a
helper may not take explicit generic parameters and `RETURN` bindings must
match the target's declared type text exactly.

When a hook refuses to plan, the fallbacks are, in order: `MODIFY_ARGS` instead
of `MODIFY_ARG`; `RETURN` on an enclosing function; `ConnReader`/`ConnWriter`
instead of `Connection`. Keep the module name `crossversion` free of collisions
in both target files.

Validate with `cargo fmt --all --check`, workspace clippy with warnings denied,
`cargo test --workspace --locked --offline`, and
`python3 tools/horizon.py check`. For anything touching the hooks also build
(`horizon.py build`) and test against a real old server — the two backends that
matter are a vanilla 1.20.1 (763) and a 1.21.1 (767) server, plus a 26.3 server
to prove nothing is routed. Keep the README's claims honest: this mod downloads
GPL-3.0 jars, starts a JVM, and installs a Mojang runtime when there is no
Java.
