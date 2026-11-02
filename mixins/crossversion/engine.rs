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
const VIAPROXY_SHA256: &str =
    "2894bbfb2f4342f8fde887af6c689313efd5b0e46a8006775eea0e21e2948ab5";
/// The OpenAuthMod plugin, GPL-3.0: lets the player's own account log in to
/// online-mode servers through the proxy.
const OPENAUTHMOD_VERSION: &str = "1.0.2";
const OPENAUTHMOD_SHA256: &str =
    "95085aac381ae92fc45d476732958b826254be4fbe2876f2d8b619b2373898fe";

/// Mojang's Java runtimes, as the launcher downloads them.
const JAVA_RUNTIMES: &str =
    "https://launchermeta.mojang.com/v1/products/java-runtime/2ec0cc96c44e5a76b9c8b7c39df7210883d12871/all.json";
/// The runtime installed when no usable Java is on the machine.
const JAVA_COMPONENT: &str = "java-runtime-delta";
const JAVA_MAJOR: u32 = 21;
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
            eprintln!("[crossversion] {} is not JSON; using defaults", path.display());
            return defaults;
        };
        Config {
            enabled: json["enabled"].as_bool().unwrap_or(defaults.enabled),
            always: json["always"].as_bool().unwrap_or(defaults.always),
            java: json["java"].as_str().map(PathBuf::from).or(defaults.java),
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

/// Where a join should go: `Some("127.0.0.1:<port>")` when the server needs the
/// proxy, `None` to leave the client alone.
pub fn route(address: &str) -> Option<String> {
    if address.starts_with(crate::net::FRIEND) || address == "local" {
        return None;
    }
    let settings = config();
    if !settings.enabled {
        return None;
    }
    reap_once();

    let protocol = ping_protocol(address);
    let translated = protocol.is_some_and(|protocol| protocol != horizon_protocol::PROTOCOL_VERSION);
    if !translated && !settings.always {
        horizon_protocol::crossversion::disarm();
        return None;
    }

    eprintln!(
        "[crossversion] {address} speaks protocol {}; starting the proxy",
        protocol.map_or_else(|| "?".to_owned(), |protocol| protocol.to_string())
    );
    match ensure_proxy(address, &settings) {
        Ok(port) => {
            arm_auth();
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

/// ViaProxy asks the client to authenticate the target server's session hash
/// (`oam:join`); the client holds the player's token, so it signs it.
fn arm_auth() {
    // Testing hook: answer every challenge with a fixed result instead of the
    // signed-in account (HORIZON_CROSSVERSION_AUTH=ok|fail).
    if let Some(mode) = std::env::var_os("HORIZON_CROSSVERSION_AUTH") {
        let ok = mode == "ok";
        eprintln!("[crossversion] debug authenticator answers {ok}");
        horizon_protocol::crossversion::arm(Box::new(move |_| ok));
        return;
    }
    let Some(session) = crate::auth::current() else {
        eprintln!("[crossversion] no account signed in: online-mode servers will refuse the client");
        return;
    };
    horizon_protocol::crossversion::arm(Box::new(move |hash| {
        match crate::auth::join_server(&session, hash) {
            Ok(()) => true,
            Err(error) => {
                eprintln!("[crossversion] joining the server's session failed: {error:?}");
                false
            }
        }
    }));
}

/// A session ended: the proxy that served it has nothing left to do.
pub fn session_ended(address: &str) {
    horizon_protocol::crossversion::disarm();
    let port = address
        .strip_prefix("127.0.0.1:")
        .and_then(|port| port.parse::<u16>().ok());
    let Some(port) = port else {
        return;
    };
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
fn ensure_proxy(target: &str, settings: &Config) -> Result<u16, String> {
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
    let child = spawn(&java, &jar, target, port)?;
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

fn spawn(java: &Path, jar: &Path, target: &str, port: u16) -> Result<Child, String> {
    let mut child = Command::new(java)
        .arg("-jar")
        .arg(jar)
        .arg("cli")
        .args(["--bind-address", &format!("127.0.0.1:{port}")])
        .args(["--target-address", target])
        .args(["--target-version", "auto"])
        .args(["--auth-method", "OPENAUTHMOD"])
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
        drain(stdout, None);
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
            return Err(format!("ViaProxy stopped during startup ({status})\n{tail}"));
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
    let plugin = folder
        .join("plugins")
        .join(format!("ViaProxyOpenAuthMod-{OPENAUTHMOD_VERSION}.jar"));
    fs::create_dir_all(folder.join("plugins")).map_err(|e| format!("{}: {e}", folder.display()))?;
    fetch(
        &format!(
            "https://github.com/ViaVersion/ViaProxy/releases/download/v{VIAPROXY_VERSION}/ViaProxy-{VIAPROXY_VERSION}.jar"
        ),
        &jar,
        VIAPROXY_SHA256,
    )?;
    fetch(
        &format!(
            "https://github.com/ViaVersionAddons/ViaProxyOpenAuthMod/releases/download/v{OPENAUTHMOD_VERSION}/ViaProxyOpenAuthMod-{OPENAUTHMOD_VERSION}.jar"
        ),
        &plugin,
        OPENAUTHMOD_SHA256,
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

/// A Java 17+ to run the proxy with, installed from Mojang if this machine has
/// none.
fn find_java(settings: &Config) -> Result<PathBuf, String> {
    if let Some(java) = &settings.java {
        return if java.is_file() {
            Ok(java.clone())
        } else {
            Err(format!("config names {}, which is not there", java.display()))
        };
    }
    // Testing hook: skip every Java on the machine and install Mojang's.
    if std::env::var_os("HORIZON_CROSSVERSION_INSTALL_JAVA").is_none() {
        if let Some(java) = installed_java() {
            return Ok(java);
        }
    }
    eprintln!("[crossversion] no Java {JAVA_MINIMUM}+ found; installing Mojang's Java {JAVA_MAJOR}");
    install_java()
}

fn java_program(home: &Path) -> PathBuf {
    home.join("bin")
        .join(if cfg!(windows) { "javaw.exe" } else { "java" })
}

/// The first Java home at or above `JAVA_MINIMUM` we can find.
fn installed_java() -> Option<PathBuf> {
    let mut homes: Vec<PathBuf> = Vec::new();
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
        if let Ok(entries) = fs::read_dir(&parent) {
            for entry in entries.flatten() {
                homes.push(entry.path());
                // macOS bundles keep the runtime under Contents/Home.
                homes.push(entry.path().join("Contents").join("Home"));
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
        parents.push(home.join("Library").join("Java").join("JavaVirtualMachines"));
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

/// Installs Mojang's runtime (the files the official launcher uses) into
/// `<game home>/runtime/` so every instance shares it.
fn install_java() -> Result<PathBuf, String> {
    let platform = java_platform().ok_or("Mojang has no Java for this system")?;
    let runtime = crate::logging::game_home().join("runtime");
    let home = runtime
        .join(JAVA_COMPONENT)
        .join(platform)
        .join(JAVA_COMPONENT);
    if release_major(&home).is_some_and(|major| major >= JAVA_MINIMUM)
        && java_program(&home).is_file()
    {
        return Ok(java_program(&home));
    }

    let agent = agent();
    let index_text = agent
        .get(JAVA_RUNTIMES)
        .call()
        .map_err(|e| format!("Mojang's Java index: {e}"))?
        .into_string()
        .map_err(|e| format!("Mojang's Java index: {e}"))?;
    let index: serde_json::Value =
        serde_json::from_str(&index_text).map_err(|e| format!("Mojang's Java index: {e}"))?;
    let entry = &index[platform][JAVA_COMPONENT][0];
    if entry.is_null() {
        return Err(format!("Mojang has no {JAVA_COMPONENT} for {platform}"));
    }
    let manifest_url = entry["manifest"]["url"]
        .as_str()
        .ok_or("Mojang's Java index has no manifest")?;
    let manifest_sha1 = entry["manifest"]["sha1"]
        .as_str()
        .ok_or("Mojang's Java index has no manifest sha1")?;
    // The manifest itself is sha1-checked.
    let manifest_bytes = fetch_bytes(&agent, manifest_url)?;
    if fingerprint(&manifest_bytes) != manifest_sha1 {
        return Err(format!("Mojang's {JAVA_COMPONENT} manifest is not the one it signed"));
    }
    let manifest: serde_json::Value = serde_json::from_slice(&manifest_bytes)
        .map_err(|e| format!("Mojang's {JAVA_COMPONENT} manifest: {e}"))?;
    let files = manifest["files"]
        .as_object()
        .ok_or_else(|| format!("Mojang's {JAVA_COMPONENT} manifest lists no files"))?;

    fs::create_dir_all(&home).map_err(|e| format!("{}: {e}", home.display()))?;
    eprintln!(
        "[crossversion] installing Minecraft's Java ({} files) into {}",
        files.len(),
        home.display()
    );
    for (name, file) in files {
        let Some(path) = safe_path(&home, name) else {
            return Err(format!("unsafe path {name:?} in Mojang's Java"));
        };
        match file["type"].as_str() {
            Some("directory") => {
                fs::create_dir_all(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            }
            Some("file") => {
                let url = file["downloads"]["raw"]["url"]
                    .as_str()
                    .ok_or_else(|| format!("{name}: no download url"))?;
                let sha1 = file["downloads"]["raw"]["sha1"]
                    .as_str()
                    .ok_or_else(|| format!("{name}: no download sha1"))?;
                let bytes = fetch_bytes(&agent, url)?;
                if fingerprint(&bytes) != sha1 {
                    return Err(format!("{name}: download does not match its sha1"));
                }
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
                }
                fs::write(&path, &bytes).map_err(|e| format!("{}: {e}", path.display()))?;
                if file["executable"] == true {
                    make_executable(&path)?;
                }
            }
            Some("link") => {
                let target = file["target"]
                    .as_str()
                    .ok_or_else(|| format!("{name}: no link target"))?;
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
                }
                let _ = fs::remove_file(&path);
                #[cfg(unix)]
                let _ = std::os::unix::fs::symlink(target, &path);
                #[cfg(windows)]
                let _ = std::os::windows::fs::symlink_file(target, &path);
            }
            _ => {}
        }
    }
    if !java_program(&home).is_file() {
        return Err(format!("Mojang's Java has no java in {}", home.display()));
    }
    Ok(java_program(&home))
}

fn fetch_bytes(agent: &ureq::Agent, url: &str) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    agent
        .get(url)
        .call()
        .map_err(|e| format!("{url}: {e}"))?
        .into_reader()
        .take(DOWNLOAD_LIMIT)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("{url}: {e}"))?;
    Ok(bytes)
}

fn fingerprint(bytes: &[u8]) -> String {
    use sha1::Digest as _;
    format!("{:x}", sha1::Sha1::digest(bytes))
}

/// A manifest path joined under `root`, refused if it could leave it.
fn safe_path(root: &Path, name: &str) -> Option<PathBuf> {
    if name.contains('\\') {
        return None;
    }
    let mut path = root.to_owned();
    for part in name.split('/') {
        if part.is_empty() || part == "." || part == ".." {
            return None;
        }
        path.push(part);
    }
    Some(path)
}

fn make_executable(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755))
            .map_err(|e| format!("{}: {e}", path.display()))?;
    }
    #[cfg(windows)]
    let _ = path;
    Ok(())
}

fn java_platform() -> Option<&'static str> {
    Some(match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "mac-os-arm64",
        ("macos", "x86_64") => "mac-os",
        ("linux", "x86_64") => "linux",
        ("linux", "x86") => "linux-i386",
        ("windows", "x86_64") => "windows-x64",
        ("windows", "aarch64") => "windows-arm64",
        ("windows", "x86") => "windows-x86",
        _ => return None,
    })
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
        let Some(pid) = entry["pid"].as_u64().and_then(|pid| u32::try_from(pid).ok()) else {
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
    fn manifest_paths_stay_inside() {
        let root = Path::new("/rt");
        assert_eq!(
            safe_path(root, "bin/java"),
            Some(root.join("bin").join("java"))
        );
        assert_eq!(safe_path(root, "legal/x"), Some(root.join("legal").join("x")));
        assert_eq!(safe_path(root, "../escape"), None);
        assert_eq!(safe_path(root, "bin/../../escape"), None);
        assert_eq!(safe_path(root, "/abs"), None);
        assert_eq!(safe_path(root, "a\\b"), None);
    }

    #[test]
    fn platform_names_match_mojang() {
        if let Some(platform) = java_platform() {
            assert!(platform.contains('-') || platform == "linux");
        }
    }
}
