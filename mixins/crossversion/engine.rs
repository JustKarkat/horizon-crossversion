// MIT License
//
// Copyright (c) 2026 Horizon Veinminer contributors
// Copyright (c) 2026 Horizon native mod contributors
//
// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files (the "Software"), to deal
// in the Software without restriction, including without limitation the rights
// to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
// copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in all
// copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
// SOFTWARE.
//
// This file is part of the horizon-crossversion bundle: the MIT notice above
// ships with it, so every copy of the .hnmod carries the licence.

// The proxy manager.
//
// Injected into `crates/horizon-client/src/net.rs` by horizon-mixins and
// compiled into the client library, so it can reach the game's own ping and
// logging code.
//
// A join to a server that is not 26.3 is pointed at a ViaProxy instance this
// module starts on a loopback port. The proxy speaks 26.3 to the client and the
// server's version to the server; nothing else about the client changes.

use std::{
    fs,
    io::Read,
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use sha2::{Digest, Sha256};

/// ViaProxy, GPL-3.0: downloaded, never embedded
/// (https://github.com/ViaVersion/ViaProxy).
const VIAPROXY_VERSION: &str = "3.4.14";
const VIAPROXY_SHA256: &str = "2894bbfb2f4342f8fde887af6c689313efd5b0e46a8006775eea0e21e2948ab5";
/// ViaProxy needs Java 17 or newer.
const JAVA_MINIMUM: u32 = 17;

/// How long a proxy gets to open its port.
const READY_TIMEOUT: Duration = Duration::from_secs(45);
/// How long the server-list ping may take when deciding a version.
const PING_TIMEOUT: Duration = Duration::from_secs(4);
/// A download larger than this is not a jar.
const DOWNLOAD_LIMIT: u64 = 128 * 1024 * 1024;

/// The mod's data folder, the same place native mods keep their settings:
/// `<game folder>/horizon-mods/.horizon-mod-data/<id>/`. Horizon's own game
/// folder (`logging::game_home`: `HORIZON_HOME` when the launcher set it,
/// else the platform default) rather than the process's current directory,
/// so the path is the game's wherever the process was started from.
fn data_dir() -> PathBuf {
    crate::logging::game_home()
        .join("horizon-mods")
        .join(".horizon-mod-data")
        .join("crossversion")
}

#[derive(Clone, Debug)]
struct Config {
    /// Turn the whole mod off.
    enabled: bool,
    /// Use the proxy even when the server is 26.3 (diagnostics).
    always: bool,
    /// Servers the status cannot describe: address -> the target version to
    /// translate to (for a network whose front reports the client's own
    /// version while its lobbies are older).
    servers: Vec<(String, String)>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            enabled: !matches!(
                std::env::var("HORIZON_CROSSVERSION").as_deref(),
                Ok("0") | Ok("off")
            ),
            always: std::env::var_os("HORIZON_CROSSVERSION_ALWAYS").is_some(),
            servers: Vec::new(),
        }
    }
}

static CONFIG: Mutex<Option<Config>> = Mutex::new(None);
static CLEANED: Mutex<bool> = Mutex::new(false);

/// Whatever an older version left for ViaProxy in the data folder is removed
/// the first time this version uses the mod at all (a join, or the mods list
/// asking for its summary), not only when a proxy is started: the accounts
/// must be gone before anyone can rely on them, and a `plugins/` folder must
/// never be loaded.
fn clean_legacy_state_once() {
    let mut done = CLEANED.lock().unwrap_or_else(|e| e.into_inner());
    if *done {
        return;
    }
    *done = true;
    let folder = data_dir().join("viaproxy");
    if fs::remove_dir_all(folder.join("plugins")).is_ok() {
        eprintln!("[crossversion] removed the old viaproxy/plugins folder");
    }
    strip_stored_accounts(&folder.join("saves.json"));
}

fn config() -> Config {
    clean_legacy_state_once();
    let mut slot = CONFIG.lock().unwrap_or_else(|e| e.into_inner());
    slot.get_or_insert_with(|| {
        let defaults = Config::default();
        let path = data_dir().join("config.json");
        let Ok(text) = fs::read_to_string(&path) else {
            let _ = fs::create_dir_all(data_dir());
            let _ = fs::write(&path, "{\n  \"enabled\": true,\n  \"always\": false\n}\n");
            return defaults;
        };
        let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) else {
            eprintln!(
                "[crossversion] {} is not JSON; using defaults",
                path.display()
            );
            return defaults;
        };
        // An environment override is for experiments and wins over the file.
        let env_on = std::env::var_os("HORIZON_CROSSVERSION").is_some();
        Config {
            enabled: if env_on {
                defaults.enabled
            } else {
                json["enabled"].as_bool().unwrap_or(defaults.enabled)
            },
            always: json["always"].as_bool().unwrap_or(defaults.always) || defaults.always,
            // A `java` in the file is ignored on purpose: the file sits in a
            // folder modpacks can write, and Java is only taken from the
            // environment or a verified path outside the game folder.
            servers: json["servers"]
                .as_object()
                .map(|servers| {
                    servers
                        .iter()
                        .filter_map(|(address, version)| {
                            Some((address.clone(), version.as_str()?.to_owned()))
                        })
                        .collect()
                })
                .unwrap_or_default(),
        }
    })
    .clone()
}

struct Proxy {
    port: u16,
    target: String,
    child: Child,
}

static PROXIES: Mutex<Vec<Proxy>> = Mutex::new(Vec::new());
static REAPED: Mutex<bool> = Mutex::new(false);

thread_local! {
    /// Set while `route` pings: that ping must keep the real protocol.
    static IN_ROUTE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Where a join should go: `Some("127.0.0.1:<port>")` when the server needs the
/// proxy, `None` to leave the client alone.
pub fn route(address: &str) -> Option<String> {
    if address.starts_with(crate::net::FRIEND) || address == "local" {
        return None;
    }
    // A server transfer comes back to this address on purpose (ViaProxy
    // rewrites the transfer to itself): the connection is already translated,
    // so it must not be routed again.
    if let Some(port) = our_port(address) {
        hold(port);
        return None;
    }
    let settings = config();
    if !settings.enabled {
        return None;
    }
    reap_once();

    // A network whose front reports whatever the client speaks, while its
    // lobbies are older, is named in the config instead.
    let forced = settings
        .servers
        .iter()
        .find(|(known, _)| known == address)
        .map(|(_, version)| version.clone())
        .filter(|version| version != "auto");
    let protocol = if forced.is_some() {
        None
    } else {
        IN_ROUTE.with(|flag| flag.set(true));
        let protocol = ping_protocol(address);
        IN_ROUTE.with(|flag| flag.set(false));
        protocol
    };
    let translated = forced.is_some()
        || protocol.is_some_and(|protocol| protocol != horizon_protocol::PROTOCOL_VERSION);
    if !translated && !settings.always {
        return None;
    }

    match &forced {
        Some(version) => eprintln!(
            "[crossversion] {address} is configured for target version {version}; starting the proxy"
        ),
        None => eprintln!(
            "[crossversion] {address} speaks protocol {}; starting the proxy",
            protocol.map_or_else(|| "?".to_owned(), |protocol| protocol.to_string())
        ),
    }
    match ensure_proxy(address, forced.as_deref()) {
        Ok(port) => {
            hold(port);
            Some(format!("127.0.0.1:{port}"))
        }
        Err(error) => {
            eprintln!("[crossversion] {address}: {error}");
            eprintln!("[crossversion] joining without translation; a 26.3 client will be refused");
            None
        }
    }
}

/// The server's protocol version from its status, if it answered.
fn ping_protocol(address: &str) -> Option<i32> {
    let (json, _) = crate::net::ping(address, PING_TIMEOUT).ok()?;
    i32::try_from(json["version"]["protocol"].as_i64()?).ok()
}

/// A session ended. A transfer ends one session and starts the next on the
/// same proxy within a second, so the proxy is stopped only once nothing has
/// used it for a few seconds.
pub fn session_ended(address: &str) {
    if let Some(port) = our_port(address) {
        release(port);
    }
}

/// Sessions using a proxy port (count) and when it was last handed to one, so
/// a proxy outlives a transfer whose sessions overlap or arrive out of order.
static ACTIVE: Mutex<Vec<(u16, usize, Instant)>> = Mutex::new(Vec::new());

/// The port, if `address` is a proxy this mod started.
fn our_port(address: &str) -> Option<u16> {
    let port = address
        .strip_prefix("127.0.0.1:")
        .and_then(|port| port.parse::<u16>().ok())?;
    let proxies = PROXIES.lock().unwrap_or_else(|e| e.into_inner());
    proxies
        .iter()
        .any(|proxy| proxy.port == port)
        .then_some(port)
}

fn hold(port: u16) {
    let mut active = ACTIVE.lock().unwrap_or_else(|e| e.into_inner());
    let now = Instant::now();
    match active.iter_mut().find(|(known, _, _)| *known == port) {
        Some((_, count, used)) => {
            *count += 1;
            *used = now;
        }
        None => active.push((port, 1, now)),
    }
}

fn release(port: u16) {
    {
        let mut active = ACTIVE.lock().unwrap_or_else(|e| e.into_inner());
        let Some(index) = active.iter().position(|(known, _, _)| *known == port) else {
            return;
        };
        active[index].1 = active[index].1.saturating_sub(1);
    }
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(5));
        stop_if_idle(port, 3);
    });
}

fn stop_if_idle(port: u16, attempts: u8) {
    {
        let active = ACTIVE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((_, count, used)) = active.iter().find(|(known, _, _)| *known == port) {
            // Someone is on it, or a transfer just picked it up: look again.
            if *count > 0 || used.elapsed() < Duration::from_secs(5) {
                if attempts > 0 {
                    std::thread::spawn(move || {
                        std::thread::sleep(Duration::from_secs(5));
                        stop_if_idle(port, attempts - 1);
                    });
                }
                return;
            }
        }
    }
    let mut proxies = PROXIES.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(index) = proxies.iter().position(|proxy| proxy.port == port) {
        stop(proxies.remove(index));
        write_pid_file(&proxies);
    }
}

fn stop(mut proxy: Proxy) {
    let _ = proxy.child.kill();
    let _ = proxy.child.wait();
    // Its working directory was made for this proxy alone; take it away.
    let _ = fs::remove_dir_all(run_dir(proxy.port));
}

/// A proxy's own working directory: empty but for the settings file written
/// for this start, so nothing a modpack put in the data folder is loaded.
fn run_dir(port: u16) -> PathBuf {
    data_dir()
        .join("viaproxy")
        .join("run")
        .join(port.to_string())
}

/// The proxy's settings, written fresh: the same values passed on the
/// command line, plus the offline default. A stale or planted file is
/// overwritten before the proxy starts.
fn write_viaproxy_yml(
    run: &Path,
    target: &str,
    port: u16,
    version: Option<&str>,
) -> Result<(), String> {
    let body = format!(
        "bind-address: \"127.0.0.1:{port}\"\ntarget-address: \"{target}\"\n\
         target-version: \"{}\"\nauth-method: NONE\nproxy-online-mode: false\n",
        version.unwrap_or("auto")
    );
    let path = run.join("viaproxy.yml");
    fs::write(&path, body).map_err(|error| format!("{}: {error}", path.display()))
}

/// Working directories a crashed run left behind: no proxy is listening on
/// those ports now, so they hold nothing worth keeping.
fn sweep_run_dirs(keep: &[u16]) {
    let folder = data_dir().join("viaproxy").join("run");
    let Ok(entries) = fs::read_dir(&folder) else {
        return;
    };
    for entry in entries.flatten() {
        let port = entry.file_name().to_string_lossy().parse::<u16>().ok();
        if port.is_some_and(|port| keep.contains(&port)) {
            continue;
        }
        let keep_running = port.is_some_and(listening);
        if !keep_running {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

/// A port this instance of ViaProxy is listening on, or a fresh one.
fn ensure_proxy(target: &str, version: Option<&str>) -> Result<u16, String> {
    let mut proxies = PROXIES.lock().unwrap_or_else(|e| e.into_inner());
    // Reuse a live one; drop a dead one.
    if let Some(index) = proxies.iter().position(|proxy| proxy.target == target) {
        if listening(proxies[index].port) {
            return Ok(proxies[index].port);
        }
        stop(proxies.remove(index));
    }
    let port = free_port()?;
    let java = find_java()?;
    let jar = ensure_assets()?;
    let child = spawn(&java, &jar, target, port, version)?;
    proxies.push(Proxy {
        port,
        target: target.to_owned(),
        child,
    });
    write_pid_file(&proxies);
    Ok(port)
}

fn listening(port: u16) -> bool {
    let address = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    TcpStream::connect_timeout(&address, Duration::from_millis(300)).is_ok()
}

fn free_port() -> Result<u16, String> {
    let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| format!("no free port: {e}"))?;
    listener
        .local_addr()
        .map(|address| address.port())
        .map_err(|e| format!("no free port: {e}"))
}

fn spawn(
    java: &Path,
    jar: &Path,
    target: &str,
    port: u16,
    version: Option<&str>,
) -> Result<Child, String> {
    // ViaProxy loads every jar in `plugins/` under its working directory and
    // reads its settings from `viaproxy.yml` there. The mod's data folder is
    // one a modpack can write, so the proxy runs in a directory this start
    // creates empty (no `plugins/`) with the settings file written fresh;
    // the options that matter are also passed on the command line, which
    // wins over the file.
    let run = run_dir(port);
    let _ = fs::remove_dir_all(&run);
    fs::create_dir_all(&run).map_err(|error| format!("{}: {error}", run.display()))?;
    write_viaproxy_yml(&run, target, port, version)?;
    let mut child = Command::new(java)
        .arg("-jar")
        .arg(jar)
        .arg("cli")
        .args(["--bind-address", &format!("127.0.0.1:{port}")])
        .args(["--target-address", target])
        .args(["--target-version", version.unwrap_or("auto")])
        // Offline mode only. The client never signs a session hash, and the
        // proxy is not given an account either: ViaProxy's Microsoft sign-in
        // uses the official launcher's client id, which Horizon's registry
        // may not ship under its own name.
        .args(["--auth-method", "NONE"])
        .current_dir(&run)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot start {}: {e}", java.display()))?;

    // Drain both pipes: a full pipe would stall the proxy, and the last lines
    // explain a startup failure.
    let errors = Arc::new(Mutex::new(Vec::<String>::new()));
    if let Some(stdout) = child.stdout.take() {
        drain(stdout, Some(errors.clone()));
    }
    if let Some(stderr) = child.stderr.take() {
        drain(stderr, Some(errors.clone()));
    }

    let started = Instant::now();
    while started.elapsed() < READY_TIMEOUT {
        if listening(port) {
            return Ok(child);
        }
        if let Ok(Some(status)) = child.try_wait() {
            let tail = errors.lock().unwrap_or_else(|e| e.into_inner()).join("\n");
            return Err(format!(
                "ViaProxy stopped during startup ({status})\n{tail}"
            ));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = child.kill();
    let _ = child.wait();
    let tail = errors.lock().unwrap_or_else(|e| e.into_inner()).join("\n");
    Err(format!(
        "ViaProxy did not open port {port} within {}s\n{tail}",
        READY_TIMEOUT.as_secs()
    ))
}

fn drain(mut pipe: impl Read + Send + 'static, keep: Option<Arc<Mutex<Vec<String>>>>) {
    let echo = std::env::var_os("HORIZON_CROSSVERSION_DEBUG").is_some();
    std::thread::spawn(move || {
        let mut buffer = [0u8; 4096];
        let mut line = Vec::new();
        loop {
            match pipe.read(&mut buffer) {
                Ok(0) | Err(_) => return,
                Ok(read) => {
                    for &byte in &buffer[..read] {
                        if byte == b'\n' {
                            if let Some(ref keep) = keep {
                                let text = String::from_utf8_lossy(&line).trim().to_owned();
                                if !text.is_empty() {
                                    if echo {
                                        eprintln!("[viaproxy] {text}");
                                    }
                                    let mut kept = keep.lock().unwrap_or_else(|e| e.into_inner());
                                    kept.push(text);
                                    if kept.len() > 40 {
                                        kept.remove(0);
                                    }
                                }
                            }
                            line.clear();
                        } else if line.len() < 4096 {
                            line.push(byte);
                        }
                    }
                }
            }
        }
    });
}

/// The ViaProxy jar. Anything an older version put beside it that the proxy
/// must not load is taken away first (see `clean_legacy_state_once`).
fn ensure_assets() -> Result<PathBuf, String> {
    let folder = data_dir().join("viaproxy");
    let jar = folder.join(format!("ViaProxy-{VIAPROXY_VERSION}.jar"));
    fs::create_dir_all(&folder).map_err(|e| format!("{}: {e}", folder.display()))?;
    clean_legacy_state_once();
    fetch(
        &format!(
            "https://github.com/ViaVersion/ViaProxy/releases/download/v{VIAPROXY_VERSION}/ViaProxy-{VIAPROXY_VERSION}.jar"
        ),
        &jar,
        VIAPROXY_SHA256,
    )?;
    Ok(jar)
}

/// 0.6.0–0.7.1 could have a Microsoft account (and its refresh token) in
/// ViaProxy's own save. Online mode is gone, so the accounts go too: the
/// file keeps its other keys and an empty `accountsV4`.
fn strip_stored_accounts(path: &Path) {
    let Ok(text) = fs::read_to_string(path) else {
        return;
    };
    let Ok(mut json) = serde_json::from_str::<serde_json::Value>(&text) else {
        let _ = fs::remove_file(path);
        return;
    };
    let Some(accounts) = json["accountsV4"].as_array() else {
        return;
    };
    if accounts.is_empty() {
        return;
    }
    let removed = accounts.len();
    json["accountsV4"] = serde_json::Value::Array(Vec::new());
    if let Ok(body) = serde_json::to_vec_pretty(&json) {
        let _ = fs::write(path, body);
        eprintln!(
            "[crossversion] removed {removed} stored account(s) from ViaProxy's saves.json; \
             online-mode servers are not supported"
        );
    }
}

fn fetch(url: &str, path: &Path, sha256: &str) -> Result<(), String> {
    if path.is_file() && digest(path)? == sha256 {
        return Ok(());
    }
    eprintln!(
        "[crossversion] downloading {}",
        path.file_name().unwrap_or_default().to_string_lossy()
    );
    let agent = agent();
    let mut bytes = Vec::new();
    agent
        .get(url)
        .call()
        .map_err(|e| format!("downloading {url}: {e}"))?
        .into_reader()
        .take(DOWNLOAD_LIMIT)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("downloading {url}: {e}"))?;
    let found = format!("{:x}", Sha256::digest(&bytes));
    if found != sha256 {
        return Err(format!(
            "{url} is not the pinned file (sha256 {found}, expected {sha256})"
        ));
    }
    let part = path.with_extension("part");
    fs::write(&part, &bytes).map_err(|e| format!("{}: {e}", part.display()))?;
    fs::rename(&part, path).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(())
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(20))
        .timeout_read(Duration::from_secs(120))
        .build()
}

fn digest(path: &Path) -> Result<String, String> {
    let mut file = fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

// ---- Java ----

/// A Java 17+ to run the proxy with. Only the environment (`HORIZON_CROSSVERSION_JAVA`,
/// `JAVA_HOME`, `PATH`) and the system's own install folders count: the game folder is
/// writable by modpacks, so a `runtime/` there, a `java` in the config, or any other
/// path inside it is never used. Nothing is downloaded.
fn find_java() -> Result<PathBuf, String> {
    if let Some(java) = installed_java() {
        return Ok(java);
    }
    Err(format!(
        "no Java {JAVA_MINIMUM}+ found. Install one, or set HORIZON_CROSSVERSION_JAVA \
         to an absolute java path outside the game folder"
    ))
}

fn java_program(home: &Path) -> PathBuf {
    home.join("bin")
        .join(if cfg!(windows) { "javaw.exe" } else { "java" })
}

/// Whether `java` may be run: an absolute path that is really a file and not
/// inside the game folder, which modpacks can write.
fn usable_java(java: &Path) -> bool {
    usable_java_at(&crate::logging::game_home(), java)
}

fn usable_java_at(home: &Path, java: &Path) -> bool {
    if !java.is_absolute() || !java.is_file() {
        return false;
    }
    match (java.canonicalize(), home.canonicalize()) {
        (Ok(java), Ok(home)) => !java.starts_with(&home),
        _ => false,
    }
}

/// The first Java home at or above `JAVA_MINIMUM` the environment names or the
/// system has: the explicit override, `JAVA_HOME`, `PATH`, then the platform's
/// install folders. Anything that resolves inside the game folder is skipped.
fn installed_java() -> Option<PathBuf> {
    let mut homes: Vec<PathBuf> = Vec::new();
    if let Some(java) = std::env::var_os("HORIZON_CROSSVERSION_JAVA") {
        homes.push(PathBuf::from(java));
    }
    if let Some(home) = std::env::var_os("JAVA_HOME") {
        homes.push(PathBuf::from(home));
    }
    if let Some(paths) = std::env::var_os("PATH") {
        for folder in std::env::split_paths(&paths) {
            if let Some(home) = folder.parent() {
                homes.push(home.to_owned());
            }
        }
    }
    for parent in java_parents() {
        if let Ok(entries) = std::fs::read_dir(&parent) {
            for entry in entries.flatten() {
                homes.push(entry.path().join("Contents").join("Home"));
                homes.push(entry.path());
            }
        }
    }
    let mut programs: Vec<PathBuf> = Vec::new();
    for home in homes {
        programs.push(java_program(&home));
        // The explicit override may name the binary itself.
        programs.push(home);
    }
    programs.into_iter().find(|program| {
        usable_java(program)
            && release_major(
                program
                    .parent()
                    .and_then(Path::parent)
                    .unwrap_or(Path::new("")),
            )
            .is_some_and(|major| major >= JAVA_MINIMUM)
    })
}

/// The folders installers put Java homes in, per platform.
fn java_parents() -> Vec<PathBuf> {
    let mut parents: Vec<PathBuf> = vec![
        PathBuf::from("/usr/lib/jvm"),
        PathBuf::from("/usr/java"),
        PathBuf::from("/usr/local/lib/jvm"),
        PathBuf::from("/Library/Java/JavaVirtualMachines"),
    ];
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        parents.push(home.join(".jdks"));
        parents.push(home.join(".sdkman").join("candidates").join("java"));
        parents.push(
            home.join("Library")
                .join("Java")
                .join("JavaVirtualMachines"),
        );
    }
    if let Some(program) = std::env::var_os("ProgramFiles") {
        let program = PathBuf::from(program);
        parents.push(program.join("Java"));
        parents.push(program.join("Eclipse Adoptium"));
    }
    parents
}

/// `JAVA_VERSION="21.0.5"` from a Java home's `release` file.
fn release_major(home: &Path) -> Option<u32> {
    let text = fs::read_to_string(home.join("release")).ok()?;
    let version = text
        .lines()
        .find_map(|line| line.strip_prefix("JAVA_VERSION="))?
        .trim()
        .trim_matches('"');
    let mut parts = version.split(['.', '_', '-', '+']);
    let first: u32 = parts.next()?.parse().ok()?;
    if first == 1 {
        parts.next()?.parse().ok()
    } else {
        Some(first)
    }
}

// ---- Settings ----

/// The mod's own version, shown in Options > Mods. Keep in step with
/// `horizon-mixin.toml`.
pub const VERSION: &str = "0.8.0";

/// What the settings page shows.
/// What the version switch needs to know: whether the mod is on at all.
pub fn settings_summary() -> (bool, String, String) {
    summary()
}

pub fn summary() -> (bool, String, String) {
    let settings = config();
    let enabled = settings.enabled;
    let java = match installed_java() {
        Some(path) => path.display().to_string(),
        None => "No Java 17+ found (install one or set HORIZON_CROSSVERSION_JAVA)".to_owned(),
    };
    (enabled, java, VIAPROXY_VERSION.to_owned())
}

/// Turn the mod on or off from its settings page, and keep it.
pub fn toggle_enabled() -> bool {
    let mut slot = CONFIG.lock().unwrap_or_else(|e| e.into_inner());
    let mut settings = slot.clone().unwrap_or_else(Config::default);
    settings.enabled = !settings.enabled;
    let enabled = settings.enabled;
    write_config(&settings);
    *slot = Some(settings);
    enabled
}

fn write_config(settings: &Config) {
    let path = data_dir().join("config.json");
    let _ = fs::create_dir_all(data_dir());
    let servers: serde_json::Map<String, serde_json::Value> = settings
        .servers
        .iter()
        .map(|(address, version)| (address.clone(), serde_json::json!(version)))
        .collect();
    let body = serde_json::json!({
        "enabled": settings.enabled,
        "always": settings.always,
        "servers": servers,
    });
    let _ = fs::write(&path, serde_json::to_vec_pretty(&body).unwrap_or_default());
}

/// The versions the server-facing switch offers: what ViaProxy tells the
/// server it speaks. "auto" asks the server (its status) and trusts the answer.
pub const VERSIONS: &[&str] = &[
    "auto", "1.8", "1.9", "1.12.2", "1.16.5", "1.20.1", "1.21.1", "1.21.4", "26.3",
];

/// The version configured for a server, "auto" when it is left to the status.
pub fn server_version(address: &str) -> String {
    let settings = config();
    settings
        .servers
        .iter()
        .find(|(known, _)| known == address)
        .map(|(_, version)| version.clone())
        .unwrap_or_else(|| "auto".to_owned())
}

/// Next version in the list after the one a server has, and keep it.
pub fn cycle_server_version(address: &str) -> String {
    let current = server_version(address);
    let index = VERSIONS.iter().position(|v| *v == current).unwrap_or(0);
    let next = VERSIONS[(index + 1) % VERSIONS.len()].to_owned();
    let mut slot = CONFIG.lock().unwrap_or_else(|e| e.into_inner());
    let mut settings = slot.clone().unwrap_or_else(Config::default);
    settings.servers.retain(|(known, _)| known != address);
    if next != "auto" {
        settings.servers.push((address.to_owned(), next.clone()));
    }
    write_config(&settings);
    *slot = Some(settings);
    next
}

/// Drop the downloaded ViaProxy so the next join fetches it again.
pub fn clear_downloads() -> Result<(), String> {
    let folder = data_dir().join("viaproxy");
    let mut removed = 0;
    for dir in [folder.clone(), folder.join("plugins")] {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.ends_with(".jar") {
                fs::remove_file(entry.path())
                    .map_err(|error| format!("{}: {error}", entry.path().display()))?;
                removed += 1;
            }
        }
    }
    if removed == 0 {
        return Err("nothing downloaded yet".to_owned());
    }
    Ok(())
}

// ---- Server list ----

/// The server list pings every server. A server this mod would route answers as
/// if it spoke this game's protocol, with its own version in green above the
/// MOTD, so the list shows it as joinable instead of "Incompatible version!".
/// Runs as a cancellable HEAD: when it answers, the client's own ping does not
/// run; when it cannot (no status, or this is our own routing ping) the real
/// `net::ping` does, unchanged.
pub fn ping_head(
    address: &str,
) -> std::ops::ControlFlow<Result<(serde_json::Value, u128), String>> {
    use std::ops::ControlFlow;
    if IN_ROUTE.with(|flag| flag.get()) || !config().enabled {
        return ControlFlow::Continue(());
    }
    match status_query(address) {
        Some((json, ms)) => ControlFlow::Break(Ok((
            crate::net::crossversion_decisions::badge(json, address),
            ms,
        ))),
        None => ControlFlow::Continue(()),
    }
}

/// The same status exchange `net::ping` makes, through the protocol crate so we
/// keep SRV resolution and the exact packet shapes.
fn status_query(address: &str) -> Option<(serde_json::Value, u128)> {
    use horizon_protocol::{
        connection::Connection,
        packets::{Empty, handshake, status},
    };
    let (host, port) = horizon_protocol::servers::parse_address(address)?;
    let mut connection = Connection::connect_server(&host, port, PING_TIMEOUT).ok()?;
    connection
        .reader
        .set_read_timeout(Some(PING_TIMEOUT))
        .ok()?;
    let intention = handshake::Serverbound::Intention(handshake::Intention {
        protocol: horizon_protocol::PROTOCOL_VERSION,
        host: host.clone(),
        port,
        intent: handshake::STATUS,
    });
    connection.send(&intention.encode()).ok()?;
    connection
        .send(&status::Serverbound::StatusRequest(Empty).encode())
        .ok()?;
    let (id, payload) = connection.recv().ok()?;
    let json = match status::Clientbound::decode(id, &payload).ok()? {
        status::Clientbound::StatusResponse(response) => {
            serde_json::from_str::<serde_json::Value>(&response.json).ok()?
        }
        _ => return None,
    };
    let started = Instant::now();
    connection
        .send(&status::Serverbound::PingRequest(status::Ping { time: 1 }).encode())
        .ok()?;
    let _ = connection.recv();
    Some((json, started.elapsed().as_millis()))
}

// ---- Leftover proxies ----

/// A proxy outlives a crash, so each start kills the ones a previous run left.
fn reap_once() {
    let mut done = REAPED.lock().unwrap_or_else(|e| e.into_inner());
    if *done {
        return;
    }
    *done = true;
    reap_leftovers();
    sweep_run_dirs(&[]);
}

/// Kill the proxies a crashed run left behind: the pid file lists them, and
/// only a process that still looks like one of ours is killed.
fn reap_leftovers() {
    let path = data_dir().join("proxies.json");
    let Ok(text) = fs::read_to_string(&path) else {
        return;
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) else {
        let _ = fs::remove_file(&path);
        return;
    };
    let mut killed = 0;
    for entry in json.as_array().into_iter().flatten() {
        let Some(pid) = entry["pid"]
            .as_u64()
            .and_then(|pid| u32::try_from(pid).ok())
        else {
            continue;
        };
        if is_our_java(pid) {
            kill(pid);
            killed += 1;
        }
    }
    let _ = fs::remove_file(&path);
    if killed > 0 {
        eprintln!("[crossversion] stopped {killed} leftover proxy process(es)");
    }
}

fn write_pid_file(proxies: &[Proxy]) {
    let entries: Vec<serde_json::Value> = proxies
        .iter()
        .map(|proxy| {
            serde_json::json!({
                "pid": proxy.child.id(),
                "port": proxy.port,
                "target": proxy.target,
            })
        })
        .collect();
    let path = data_dir().join("proxies.json");
    let _ = fs::create_dir_all(data_dir());
    let _ = fs::write(
        &path,
        serde_json::to_vec_pretty(&serde_json::Value::Array(entries)).unwrap_or_default(),
    );
}

/// Whether `pid` is a ViaProxy we started, so a reused pid is never killed.
fn is_our_java(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        fs::read(format!("/proc/{pid}/cmdline"))
            .map(|bytes| String::from_utf8_lossy(&bytes).contains("ViaProxy"))
            .unwrap_or(false)
    }
    #[cfg(target_os = "macos")]
    {
        Command::new("ps")
            .args(["-o", "command=", "-p", &pid.to_string()])
            .output()
            .map(|output| String::from_utf8_lossy(&output.stdout).contains("ViaProxy"))
            .unwrap_or(false)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = pid;
        false
    }
}

fn kill(pid: u32) {
    #[cfg(unix)]
    {
        let _ = Command::new("kill").arg(pid.to_string()).status();
    }
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/F"])
            .status();
    }
    #[cfg(not(any(unix, windows)))]
    let _ = pid;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_java_home_needs_a_release_file() {
        assert_eq!(release_major(Path::new("/nonexistent")), None);
    }

    #[test]
    fn stored_accounts_are_removed_from_a_previous_sign_in() {
        let folder =
            std::env::temp_dir().join(format!("crossversion-saves-{}", std::process::id()));
        let _ = fs::remove_dir_all(&folder);
        fs::create_dir_all(&folder).unwrap();
        let path = folder.join("saves.json");
        fs::write(
            &path,
            r#"{"accountsV4":[{"accountType":"MSA-MicrosoftAccount"},{"accountType":"DeviceCodeAccount"}],"ui":{"notice":true}}"#,
        )
        .unwrap();
        strip_stored_accounts(&path);
        let json: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(json["accountsV4"].as_array().unwrap().len(), 0);
        assert_eq!(json["ui"]["notice"], true);
        // Running again changes nothing and does not fail.
        strip_stored_accounts(&path);
        let again: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(again["ui"]["notice"], true);
        let _ = fs::remove_dir_all(&folder);
    }

    #[test]
    fn java_inside_the_game_folder_is_refused() {
        let folder = std::env::temp_dir().join(format!("crossversion-java-{}", std::process::id()));
        let _ = fs::remove_dir_all(&folder);
        let game = folder.join("game");
        let outside = folder.join("jdk");
        for dir in [&game, &outside] {
            fs::create_dir_all(dir).unwrap();
        }
        let inside_java = game.join("runtime").join("java");
        let outside_java = outside.join("java");
        fs::create_dir_all(inside_java.parent().unwrap()).unwrap();
        fs::write(&inside_java, b"#!/bin/sh\n").unwrap();
        fs::write(&outside_java, b"#!/bin/sh\n").unwrap();
        assert!(!usable_java_at(&game, &inside_java));
        assert!(usable_java_at(&game, &outside_java));
        assert!(!usable_java_at(&game, Path::new("java")));
        let _ = fs::remove_dir_all(&folder);
    }
}
