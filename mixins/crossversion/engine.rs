// Cross-version support (horizon-crossversion): the proxy manager.
//
// Injected into `crates/horizon-client/src/net.rs` by horizon-mixins and
// compiled into the client library, so it can reach the game's own ping,
// account and logging code.
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
/// `<game folder>/horizon-mods/.horizon-mod-data/<id>/`.
fn data_dir() -> PathBuf {
    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
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
    /// A `java` to use instead of finding or installing one.
    java: Option<PathBuf>,
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
            java: std::env::var_os("HORIZON_CROSSVERSION_JAVA").map(PathBuf::from),
            servers: Vec::new(),
        }
    }
}

static CONFIG: Mutex<Option<Config>> = Mutex::new(None);

fn config() -> Config {
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
            java: defaults
                .java
                .or_else(|| json["java"].as_str().map(PathBuf::from)),
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
    // so it must not be routed again, and its login still needs the account.
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
        .map(|(_, version)| version.clone());
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
    match ensure_proxy(address, &settings, forced.as_deref()) {
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
}

/// A port this instance of ViaProxy is listening on, or a fresh one.
fn ensure_proxy(target: &str, settings: &Config, version: Option<&str>) -> Result<u16, String> {
    let mut proxies = PROXIES.lock().unwrap_or_else(|e| e.into_inner());
    // Reuse a live one; drop a dead one.
    if let Some(index) = proxies.iter().position(|proxy| proxy.target == target) {
        if listening(proxies[index].port) {
            return Ok(proxies[index].port);
        }
        stop(proxies.remove(index));
    }
    let port = free_port()?;
    let java = find_java(settings)?;
    let jar = ensure_assets()?;
    let child = spawn(&java, &jar, target, port, version, proxy_account())?;
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
    account: Option<usize>,
) -> Result<Child, String> {
    let mut child = Command::new(java)
        .arg("-jar")
        .arg(jar)
        .arg("cli")
        .args(["--bind-address", &format!("127.0.0.1:{port}")])
        .args(["--target-address", target])
        .args(["--target-version", version.unwrap_or("auto")])
        // The client never signs a session hash: a server must not be able to
        // have it sign one for somewhere else. So the proxy authenticates with
        // the account the player added to it, or not at all.
        .args(match account {
            Some(index) => vec![
                "--auth-method".to_owned(),
                "ACCOUNT".to_owned(),
                "--minecraft-account-index".to_owned(),
                index.to_string(),
            ],
            None => vec!["--auth-method".to_owned(), "NONE".to_owned()],
        })
        .current_dir(jar.parent().unwrap_or(Path::new(".")))
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

/// The ViaProxy jar, with its plugin beside it in `plugins/`.
fn ensure_assets() -> Result<PathBuf, String> {
    let folder = data_dir().join("viaproxy");
    let jar = folder.join(format!("ViaProxy-{VIAPROXY_VERSION}.jar"));
    fs::create_dir_all(&folder).map_err(|e| format!("{}: {e}", folder.display()))?;
    // An install upgraded from 0.4.x still has the OpenAuthMod plugin, which is
    // how that version had the client sign session hashes. Remove it.
    if let Ok(entries) = fs::read_dir(folder.join("plugins")) {
        for entry in entries.flatten() {
            if entry
                .file_name()
                .to_string_lossy()
                .starts_with("ViaProxyOpenAuthMod")
            {
                let _ = fs::remove_file(entry.path());
            }
        }
    }
    fetch(
        &format!(
            "https://github.com/ViaVersion/ViaProxy/releases/download/v{VIAPROXY_VERSION}/ViaProxy-{VIAPROXY_VERSION}.jar"
        ),
        &jar,
        VIAPROXY_SHA256,
    )?;
    Ok(jar)
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

/// A Java 17+ to run the proxy with: the one named in the config, a runtime the
/// launcher already keeps under the game home, or one installed on the machine.
/// Nothing is downloaded: a machine without Java says so.
fn find_java(settings: &Config) -> Result<PathBuf, String> {
    if let Some(java) = &settings.java {
        return if java.is_file() {
            Ok(java.clone())
        } else {
            Err(format!(
                "config names {}, which is not there",
                java.display()
            ))
        };
    }
    if let Some(java) = installed_java() {
        return Ok(java);
    }
    Err(format!(
        "no Java {JAVA_MINIMUM}+ found. Install one, or point \"java\" in \
         horizon-mods/.horizon-mod-data/crossversion/config.json at it"
    ))
}

fn java_program(home: &Path) -> PathBuf {
    home.join("bin")
        .join(if cfg!(windows) { "javaw.exe" } else { "java" })
}

/// The first Java home at or above `JAVA_MINIMUM` this machine has.
fn installed_java() -> Option<PathBuf> {
    let mut homes: Vec<PathBuf> = Vec::new();
    // What Horizon itself uses, beside the game: <game home>/runtime follows
    // the official launcher's <component>/<platform>/<component> layout, which
    // macOS fills with a jre.bundle.
    let runtime = crate::logging::game_home().join("runtime");
    for component in std::fs::read_dir(&runtime).into_iter().flatten().flatten() {
        for platform in std::fs::read_dir(component.path())
            .into_iter()
            .flatten()
            .flatten()
        {
            for home in std::fs::read_dir(platform.path())
                .into_iter()
                .flatten()
                .flatten()
            {
                let home = home.path();
                homes.push(home.join("Contents").join("Home"));
                homes.push(home.join("jre.bundle").join("Contents").join("Home"));
                homes.push(home);
            }
        }
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
    homes
        .into_iter()
        .find(|home| match release_major(home) {
            Some(major) => major >= JAVA_MINIMUM && java_program(home).is_file(),
            None => false,
        })
        .map(|home| java_program(&home))
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

// ---- Account ----

/// The index, in ViaProxy's own list, of the Microsoft account the player added
/// there. The proxy logs in to online-mode servers with it; the client never
/// touches the session, so no server can make it sign anything.
pub fn proxy_account() -> Option<usize> {
    let text = fs::read_to_string(data_dir().join("viaproxy").join("saves.json")).ok()?;
    let json: serde_json::Value = serde_json::from_str(&text).ok()?;
    json["accountsV4"].as_array()?.iter().position(|account| {
        account["accountType"]
            .as_str()
            .is_some_and(|kind| kind.ends_with("MicrosoftAccount"))
    })
}

/// Where the sign-in is, as the settings page words it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SignIn {
    /// Nothing started.
    Idle,
    /// Waiting for the player at Microsoft's page with this code.
    Code { url: String, code: String },
    /// The account was added; the proxy uses it from now on.
    Done,
    /// It timed out or failed; starting again asks Microsoft again.
    Failed(String),
}

static SIGN_IN: Mutex<SignIn> = Mutex::new(SignIn::Idle);
static SIGN_IN_CHILD: Mutex<Option<Child>> = Mutex::new(None);

pub fn sign_in_state() -> SignIn {
    if proxy_account().is_some() {
        return SignIn::Done;
    }
    SIGN_IN.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Ask Microsoft for a device code and wait for the player to approve it in a
/// browser or on a phone. No Java window: ViaProxy's console does the same
/// sign-in, and the code goes on the settings page.
pub fn start_sign_in() -> Result<(), String> {
    if proxy_account().is_some() {
        return Ok(());
    }
    {
        let child = SIGN_IN_CHILD.lock().unwrap_or_else(|e| e.into_inner());
        if child.is_some() {
            return Ok(());
        }
    }
    let settings = config();
    let java = find_java(&settings)?;
    let jar = ensure_assets()?;
    let mut process = Command::new(&java)
        .arg("-jar")
        .arg(&jar)
        .args(["cli", "--bind-address", "127.0.0.1:0"])
        .current_dir(jar.parent().unwrap_or(Path::new(".")))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("cannot start the sign-in: {error}"))?;
    let mut stdin = process
        .stdin
        .take()
        .ok_or_else(|| "the sign-in has no console".to_owned())?;
    let stdout = process
        .stdout
        .take()
        .ok_or_else(|| "the sign-in has no output".to_owned())?;
    *SIGN_IN.lock().unwrap_or_else(|e| e.into_inner()) = SignIn::Failed("starting".to_owned());
    *SIGN_IN_CHILD.lock().unwrap_or_else(|e| e.into_inner()) = Some(process);
    std::thread::spawn(move || {
        use std::io::{BufRead, BufReader, Write};
        std::thread::sleep(Duration::from_secs(12));
        let _ = stdin.write_all(b"account add microsoft\n");
        let _ = stdin.flush();
        let reader = BufReader::new(stdout);
        let started = Instant::now();
        for line in reader.lines() {
            let Ok(line) = line else { break };
            if let Some(code) = device_code(&line) {
                let url = format!("https://www.microsoft.com/link?otc={code}");
                eprintln!("[crossversion] sign in at {url} with the code {code}");
                *SIGN_IN.lock().unwrap_or_else(|e| e.into_inner()) = SignIn::Code { url, code };
            }
            if proxy_account().is_some() || started.elapsed() > Duration::from_secs(600) {
                break;
            }
        }
        if proxy_account().is_some() {
            *SIGN_IN.lock().unwrap_or_else(|e| e.into_inner()) = SignIn::Done;
        } else if started.elapsed() > Duration::from_secs(600) {
            *SIGN_IN.lock().unwrap_or_else(|e| e.into_inner()) =
                SignIn::Failed("the code expired; ask again".to_owned());
        }
        let _ = stdin.write_all(b"stop\n");
        let _ = stdin.flush();
        std::thread::sleep(Duration::from_secs(3));
        if let Some(mut child) = SIGN_IN_CHILD
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            let _ = child.kill();
            let _ = child.wait();
        }
    });
    Ok(())
}

/// `... visit https://www.microsoft.com/link?otc=ABCD1234 ...` or `... the code: ABCD1234`.
fn device_code(line: &str) -> Option<String> {
    let text = line.replace("\u{1b}[", "").replace('\x1b', "");
    let tail = text
        .split("otc=")
        .nth(1)
        .or_else(|| text.split("code: ").nth(1))?;
    let code: String = tail
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric())
        .collect();
    (code.len() >= 4).then_some(code)
}

// ---- Settings ----

/// The mod's own version, shown in Options > Mods. Keep in step with
/// `horizon-mixin.toml`.
pub const VERSION: &str = "0.6.0";

/// What the settings page shows.
pub fn summary() -> (bool, String, String) {
    let settings = config();
    let enabled = settings.enabled;
    let java = match &settings.java {
        Some(path) => path.display().to_string(),
        None => match installed_java() {
            Some(path) => path.display().to_string(),
            None => "Mojang's Java, downloaded on first use".to_owned(),
        },
    };
    (enabled, java, VIAPROXY_VERSION.to_owned())
}

/// Turn the mod on or off from its settings page, and keep it.
pub fn toggle_enabled() -> bool {
    let mut slot = CONFIG.lock().unwrap_or_else(|e| e.into_inner());
    let mut settings = slot.clone().unwrap_or_else(Config::default);
    settings.enabled = !settings.enabled;
    let enabled = settings.enabled;
    *slot = Some(settings.clone());
    let path = data_dir().join("config.json");
    let _ = fs::create_dir_all(data_dir());
    let servers: serde_json::Map<String, serde_json::Value> = settings
        .servers
        .iter()
        .map(|(address, version)| (address.clone(), serde_json::json!(version)))
        .collect();
    let body = serde_json::json!({
        "enabled": enabled,
        "always": settings.always,
        "java": settings.java.as_ref().map(|path| path.display().to_string()),
        "servers": servers,
    });
    let _ = fs::write(&path, serde_json::to_vec_pretty(&body).unwrap_or_default());
    enabled
}

/// Drop the downloaded ViaProxy so the next join fetches it again.
pub fn clear_downloads() -> Result<(), String> {
    let folder = data_dir().join("viaproxy");
    match fs::remove_dir_all(&folder) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("{}: {error}", folder.display())),
    }
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
}
