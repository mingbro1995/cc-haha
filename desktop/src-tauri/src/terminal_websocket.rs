use std::{
    collections::HashMap,
    fs,
    net::SocketAddr,
    path::PathBuf,
    sync::atomic::{AtomicU32, Ordering},
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use futures::{SinkExt, StreamExt};
use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, MasterPty, PtySize};
use serde::{Deserialize, Serialize};
// No Tauri-specific imports — this module can be used both inside Tauri and as a
// standalone binary for H5/browser deployments.
use tokio::net::TcpListener;
use tokio::sync::{mpsc, Mutex};
use tokio_tungstenite::{accept_async, tungstenite::Message};

// ─── Re-use helpers from lib.rs ────────────────────────────────────────────
//
// We duplicate the small helpers here so we don't have to make everything
// pub(crate) in lib.rs.  The heavy logic (PTY creation etc.) is kept inline.

fn claude_config_dir() -> Option<PathBuf> {
    std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .or_else(|| std::env::var_os("USERPROFILE"))
                .map(PathBuf::from)
                .map(|p| p.join(".claude"))
        })
}

fn terminal_config_path(config_dir: Option<&std::path::Path>) -> Option<PathBuf> {
    std::env::var("CLAUDE_CONFIG_DIR")
        .ok()
        .map(|dir| PathBuf::from(&dir).join("terminal-config.json"))
        .or_else(|| config_dir.map(|dir| dir.join("terminal-config.json")))
}

#[derive(Default, serde::Serialize, serde::Deserialize)]
struct TerminalConfig {
    #[serde(default)]
    bash_path: Option<String>,
}

impl TerminalConfig {
    fn load(config_dir: Option<&std::path::Path>) -> Self {
        let path = match terminal_config_path(config_dir) {
            Some(p) => p,
            None => return Self::default(),
        };
        fs::read_to_string(&path)
            .ok()
            .and_then(|data| serde_json::from_str(&data).ok())
            .unwrap_or_default()
    }
}

fn default_shell(custom_bash: Option<&str>) -> String {
    #[cfg(target_os = "windows")]
    if let Some(bash_path) = custom_bash {
        let trimmed = bash_path.trim();
        if !trimmed.is_empty() && PathBuf::from(trimmed).is_file() {
            return trimmed.to_string();
        }
    }

    #[cfg(target_os = "windows")]
    {
        std::env::var("COMSPEC").unwrap_or_else(|_| "powershell.exe".to_string())
    }
    #[cfg(not(target_os = "windows"))]
    {
        std::env::var("SHELL").unwrap_or_else(|_| {
            if PathBuf::from("/bin/zsh").exists() {
                "/bin/zsh".to_string()
            } else {
                "/bin/bash".to_string()
            }
        })
    }
}

#[derive(Debug, Clone, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct DesktopTerminalSettingsFile {
    desktop_terminal: Option<DesktopTerminalConfig>,
}

#[derive(Debug, Clone, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct DesktopTerminalConfig {
    startup_shell: Option<String>,
    custom_shell_path: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TerminalHostPlatform {
    Windows,
    Posix,
}

fn current_terminal_host_platform() -> TerminalHostPlatform {
    #[cfg(target_os = "windows")]
    {
        TerminalHostPlatform::Windows
    }
    #[cfg(not(target_os = "windows"))]
    {
        TerminalHostPlatform::Posix
    }
}

fn desktop_terminal_settings_path() -> Option<PathBuf> {
    claude_config_dir().map(|path| path.join("settings.json"))
}

fn read_desktop_terminal_config() -> Option<DesktopTerminalConfig> {
    let path = desktop_terminal_settings_path()?;
    let contents = fs::read_to_string(path).ok()?;
    let settings: DesktopTerminalSettingsFile = serde_json::from_str(&contents).ok()?;
    settings.desktop_terminal
}

fn resolve_desktop_terminal_shell(
    platform: TerminalHostPlatform,
    config: Option<&DesktopTerminalConfig>,
    _system_default: &str,
) -> Result<Option<String>, String> {
    if platform != TerminalHostPlatform::Windows {
        return Ok(None);
    }
    let Some(config) = config else {
        return Ok(None);
    };
    let Some(startup_shell) = config.startup_shell.as_deref().map(str::trim) else {
        return Ok(None);
    };
    match startup_shell {
        "" | "system" => Ok(None),
        "pwsh" => Ok(Some("pwsh.exe".to_string())),
        "powershell" => Ok(Some("powershell.exe".to_string())),
        "cmd" => Ok(Some("cmd.exe".to_string())),
        "custom" => {
            let path = config
                .custom_shell_path
                .as_deref()
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .ok_or_else(|| "custom terminal shell path is empty".to_string())?;
            Ok(Some(path.to_string()))
        }
        _ => Ok(None),
    }
}

fn resolved_terminal_shell(config_dir: Option<&std::path::Path>) -> Result<String, String> {
    let terminal_config = TerminalConfig::load(config_dir);
    let system_default = default_shell(terminal_config.bash_path.as_deref());
    let platform = current_terminal_host_platform();
    let configured = read_desktop_terminal_config();
    let override_shell =
        resolve_desktop_terminal_shell(platform, configured.as_ref(), &system_default)?;
    Ok(override_shell.unwrap_or(system_default))
}

fn resolve_terminal_cwd(cwd: Option<String>) -> Result<PathBuf, String> {
    let path = match cwd.and_then(|value| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(PathBuf::from(trimmed))
        }
    }) {
        Some(path) => path,
        None => std::env::var_os("CLAUDE_CONFIG_DIR")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME")
                    .or_else(|| std::env::var_os("USERPROFILE"))
                    .map(PathBuf::from)
            })
            .unwrap_or(
                std::env::current_dir()
                    .map_err(|err| format!("resolve current directory: {err}"))?,
            ),
    };
    if path.is_dir() {
        Ok(path)
    } else {
        Err(format!("terminal cwd does not exist: {}", path.display()))
    }
}

fn terminal_environment(shell: &str) -> std::collections::HashMap<String, String> {
    let mut env: std::collections::HashMap<String, String> = std::env::vars().collect();
    env.extend(login_shell_environment(shell));
    ensure_utf8_locale(&mut env);
    env
}

fn ensure_utf8_locale(env: &mut std::collections::HashMap<String, String>) {
    let fallback = default_utf8_locale();
    for key in ["LANG", "LC_CTYPE", "LC_ALL"] {
        let needs_fallback = env
            .get(key)
            .map(|value| !is_utf8_locale(value))
            .unwrap_or(true);
        if needs_fallback {
            env.insert(key.to_string(), fallback.to_string());
        }
    }
}

fn is_utf8_locale(value: &str) -> bool {
    let normalized = value.trim().to_ascii_lowercase().replace('-', "");
    normalized.contains("utf8")
}

fn default_utf8_locale() -> &'static str {
    #[cfg(target_os = "macos")]
    {
        "en_US.UTF-8"
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        "C.UTF-8"
    }
    #[cfg(not(unix))]
    {
        "C.UTF-8"
    }
}

#[cfg(not(target_os = "windows"))]
fn login_shell_environment(shell: &str) -> std::collections::HashMap<String, String> {
    use std::io::Read;
    use std::process::{Command as StdCommand, Stdio};
    use std::time::{Duration, Instant};

    let Ok(mut child) = StdCommand::new(shell)
        .args(["-l", "-c", "env -0"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return std::collections::HashMap::new();
    };

    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    return std::collections::HashMap::new();
                }
                let mut stdout = Vec::new();
                if let Some(mut pipe) = child.stdout.take() {
                    let _ = pipe.read_to_end(&mut stdout);
                }
                return parse_env_block(&stdout);
            }
            Ok(None) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(25));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return std::collections::HashMap::new();
            }
            Err(_) => return std::collections::HashMap::new(),
        }
    }
}

#[cfg(target_os = "windows")]
fn login_shell_environment(_shell: &str) -> std::collections::HashMap<String, String> {
    std::collections::HashMap::new()
}

fn parse_env_block(bytes: &[u8]) -> std::collections::HashMap<String, String> {
    bytes
        .split(|byte| *byte == 0)
        .filter_map(|entry| {
            if entry.is_empty() {
                return None;
            }
            let equals = entry.iter().position(|byte| *byte == b'=')?;
            if equals == 0 {
                return None;
            }
            let key = String::from_utf8_lossy(&entry[..equals]).to_string();
            let value = String::from_utf8_lossy(&entry[equals + 1..]).to_string();
            Some((key, value))
        })
        .collect()
}

// Copy decode_terminal_output from lib.rs
fn decode_terminal_output(pending: &mut Vec<u8>, chunk: &[u8]) -> String {
    pending.extend_from_slice(chunk);
    let mut output = String::new();

    loop {
        match std::str::from_utf8(pending) {
            Ok(text) => {
                output.push_str(text);
                pending.clear();
                break;
            }
            Err(err) => {
                let valid_up_to = err.valid_up_to();
                if valid_up_to > 0 {
                    let text = std::str::from_utf8(&pending[..valid_up_to])
                        .expect("valid_up_to marks a valid UTF-8 prefix");
                    output.push_str(text);
                    pending.drain(..valid_up_to);
                    continue;
                }

                match err.error_len() {
                    Some(error_len) => {
                        output.push('\u{fffd}');
                        pending.drain(..error_len);
                    }
                    None => break,
                }
            }
        }
    }

    output
}

// ─── Port file helpers ─────────────────────────────────────────────────────

const TERMINAL_WS_PORT_FILE: &str = "terminal-ws.json";

fn write_port_file(port: u16) {
    let Some(config_dir) = claude_config_dir() else {
        eprintln!("[terminal-ws] cannot determine config dir for port file");
        return;
    };
    let path = config_dir.join(TERMINAL_WS_PORT_FILE);
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let data = serde_json::json!({ "port": port });
    if let Err(e) = fs::write(&path, data.to_string()) {
        eprintln!(
            "[terminal-ws] failed to write port file {}: {e}",
            path.display()
        );
    }
}

fn remove_port_file() {
    let Some(config_dir) = claude_config_dir() else { return };
    let path = config_dir.join(TERMINAL_WS_PORT_FILE);
    let _ = fs::remove_file(path);
}

// ─── Message protocols ─────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum ClientMessage {
    #[serde(rename = "spawn")]
    Spawn {
        requestId: String,
        cols: u16,
        rows: u16,
        #[serde(default)]
        cwd: Option<String>,
    },
    #[serde(rename = "write")]
    Write {
        requestId: String,
        sessionId: u32,
        data: String,
    },
    #[serde(rename = "resize")]
    Resize {
        requestId: String,
        sessionId: u32,
        cols: u16,
        rows: u16,
    },
    #[serde(rename = "kill")]
    Kill {
        requestId: String,
        sessionId: u32,
    },
    #[serde(rename = "getBashPath")]
    GetBashPath { requestId: String },
    #[serde(rename = "setBashPath")]
    SetBashPath {
        requestId: String,
        #[serde(default)]
        path: Option<String>,
    },
}

#[derive(Debug, Serialize)]
#[serde(tag = "type")]
enum ServerMessage {
    #[serde(rename = "spawnResult")]
    SpawnResult {
        requestId: String,
        sessionId: u32,
        shell: String,
        cwd: String,
    },
    #[serde(rename = "output")]
    Output { sessionId: u32, data: String },
    #[serde(rename = "exit")]
    Exit {
        sessionId: u32,
        code: u32,
        signal: Option<String>,
    },
    #[serde(rename = "bashPath")]
    BashPath { requestId: String, path: Option<String> },
    #[serde(rename = "ok")]
    Ok { requestId: String },
    #[serde(rename = "error")]
    Error { requestId: String, message: String },
}

// ─── Per-connection session state ──────────────────────────────────────────

struct Session {
    master: Box<dyn MasterPty + Send>,
    writer: std::sync::Mutex<Box<dyn std::io::Write + Send>>,
    killer: std::sync::Mutex<Box<dyn ChildKiller + Send + Sync>>,
}

struct ConnectionState {
    next_id: AtomicU32,
    sessions: Arc<Mutex<HashMap<u32, Session>>>,
    tx: Mutex<mpsc::Sender<ServerMessage>>,
}

impl ConnectionState {
    fn new(tx: mpsc::Sender<ServerMessage>) -> Self {
        Self {
            next_id: AtomicU32::new(1),
            sessions: Arc::new(Mutex::new(HashMap::new())),
            tx: Mutex::new(tx),
        }
    }

    async fn send(&self, msg: ServerMessage) {
        let _ = self.tx.lock().await.send(msg).await;
    }
}

// ─── Connection handler ────────────────────────────────────────────────────

async fn handle_connection(config_dir: Option<PathBuf>, raw_stream: tokio::net::TcpStream, addr: SocketAddr) {
    println!("[terminal-ws] Incoming connection from {addr}");

    let ws_stream = match accept_async(raw_stream).await {
        Ok(ws) => ws,
        Err(e) => {
            eprintln!("[terminal-ws] WebSocket handshake failed for {addr}: {e}");
            return;
        }
    };

    let (msg_tx, mut msg_rx) = mpsc::channel::<ServerMessage>(256);
    let conn = Arc::new(ConnectionState::new(msg_tx));

    let (mut ws_sink, mut ws_stream) = ws_stream.split();

    // Task: forward outbound messages from our mpsc channel to the WebSocket
    let forward_handle = tokio::spawn(async move {
        while let Some(msg) = msg_rx.recv().await {
            let payload = match serde_json::to_string(&msg) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("[terminal-ws] failed to serialize message: {e}");
                    continue;
                }
            };
            if let Err(e) = ws_sink.send(Message::Text(payload.into())).await {
                eprintln!("[terminal-ws] send error: {e}");
                break;
            }
        }
    });

    // Process inbound messages
    while let Some(result) = ws_stream.next().await {
        match result {
            Ok(Message::Text(text)) => {
                let msg: ClientMessage = match serde_json::from_str(&text) {
                    Ok(m) => m,
                    Err(e) => {
                        eprintln!("[terminal-ws] invalid client message: {e}");
                        continue;
                    }
                };
                handle_client_message(config_dir.as_deref(), &conn, msg).await;
            }
            Ok(Message::Close(_)) => break,
            Ok(Message::Ping(_)) => {}
            Err(e) => {
                eprintln!("[terminal-ws] stream error: {e}");
                break;
            }
            _ => {}
        }
    }

    // Client disconnected — kill every session we created for it
    let sessionIds: Vec<u32> = {
        let guard = conn.sessions.lock().await;
        guard.keys().copied().collect()
    };
    for id in sessionIds {
        let session = {
            let mut guard = conn.sessions.lock().await;
            guard.remove(&id)
        };
        if let Some(s) = session {
            if let Ok(mut killer) = s.killer.lock() {
                let _ = killer.kill();
            }
        }
    }

    drop(forward_handle);
    println!("[terminal-ws] Connection from {addr} closed");
}

async fn handle_client_message(
    config_dir: Option<&std::path::Path>,
    conn: &Arc<ConnectionState>,
    msg: ClientMessage,
) {
    match msg {
        ClientMessage::Spawn {
            requestId,
            cols,
            rows,
            cwd,
        } => {
            match spawn_session(config_dir, conn, cols, rows, cwd).await {
                Ok((sessionId, shell, cwd_path)) => {
                    conn.send(ServerMessage::SpawnResult {
                        requestId,
                        sessionId,
                        shell,
                        cwd: cwd_path,
                    })
                    .await;
                }
                Err(e) => {
                    conn.send(ServerMessage::Error {
                        requestId,
                        message: e,
                    })
                    .await;
                }
            }
        }
        ClientMessage::Write {
            requestId,
            sessionId,
            data,
        } => {
            let guard = conn.sessions.lock().await;
            let session = guard.get(&sessionId);
            match session {
                Some(s) => {
                    let res = (|| {
                        let mut writer = s
                            .writer
                            .lock()
                            .map_err(|_| "terminal writer is unavailable".to_string())?;
                        writer
                            .write_all(data.as_bytes())
                            .map_err(|err| format!("write terminal input: {err}"))?;
                        writer
                            .flush()
                            .map_err(|err| format!("flush terminal input: {err}"))?;
                        Ok(())
                    })();
                    drop(guard);
                    match res {
                        Ok(()) => {
                            conn.send(ServerMessage::Ok { requestId }).await;
                        }
                        Err(e) => {
                            conn.send(ServerMessage::Error {
                                requestId,
                                message: e,
                            })
                            .await;
                        }
                    }
                }
                None => {
                    drop(guard);
                    conn.send(ServerMessage::Error {
                        requestId,
                        message: "terminal session is not running".to_string(),
                    })
                    .await;
                }
            }
        }
        ClientMessage::Resize {
            requestId,
            sessionId,
            cols,
            rows,
        } => {
            let guard = conn.sessions.lock().await;
            let session = guard.get(&sessionId);
            match session {
                Some(s) => {
                    let res = s.master.resize(PtySize {
                        rows: rows.max(8),
                        cols: cols.max(20),
                        pixel_width: 0,
                        pixel_height: 0,
                    });
                    drop(guard);
                    match res {
                        Ok(()) => {
                            conn.send(ServerMessage::Ok { requestId }).await;
                        }
                        Err(e) => {
                            conn.send(ServerMessage::Error {
                                requestId,
                                message: format!("resize terminal: {e}"),
                            })
                            .await;
                        }
                    }
                }
                None => {
                    drop(guard);
                    conn.send(ServerMessage::Error {
                        requestId,
                        message: "terminal session is not running".to_string(),
                    })
                    .await;
                }
            }
        }
        ClientMessage::Kill {
            requestId,
            sessionId,
        } => {
            let session = {
                let mut guard = conn.sessions.lock().await;
                guard.remove(&sessionId)
            };
            match session {
                Some(s) => {
                    if let Ok(mut killer) = s.killer.lock() {
                        let _ = killer.kill();
                    }
                    conn.send(ServerMessage::Ok { requestId }).await;
                }
                None => {
                    conn.send(ServerMessage::Ok { requestId }).await;
                }
            }
        }
        ClientMessage::GetBashPath { requestId } => {
            let config = TerminalConfig::load(config_dir);
            conn.send(ServerMessage::BashPath {
                requestId,
                path: config.bash_path,
            })
            .await;
        }
        ClientMessage::SetBashPath { requestId, path } => {
            match set_terminal_bash_path(config_dir, path) {
                Ok(()) => {
                    conn.send(ServerMessage::Ok { requestId }).await;
                }
                Err(e) => {
                    conn.send(ServerMessage::Error {
                        requestId,
                        message: e,
                    })
                    .await;
                }
            }
        }
    }
}

async fn spawn_session(
    config_dir: Option<&std::path::Path>,
    conn: &ConnectionState,
    cols: u16,
    rows: u16,
    cwd: Option<String>,
) -> Result<(u32, String, String), String> {
    let cwd_path = resolve_terminal_cwd(cwd)?;
    let shell = resolved_terminal_shell(config_dir)?;
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: rows.max(8),
            cols: cols.max(20),
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|err| format!("open terminal pty: {err}"))?;

    let mut cmd = CommandBuilder::new(&shell);
    cmd.cwd(cwd_path.as_os_str());
    for (key, value) in terminal_environment(&shell) {
        cmd.env(key, value);
    }
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");

    let mut child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|err| format!("spawn terminal shell: {err}"))?;
    drop(pair.slave);

    let reader = pair
        .master
        .try_clone_reader()
        .map_err(|err| format!("clone terminal reader: {err}"))?;
    let writer = pair
        .master
        .take_writer()
        .map_err(|err| format!("open terminal writer: {err}"))?;
    let killer = child.clone_killer();

    let session_id = conn.next_id.fetch_add(1, Ordering::Relaxed);

    let session = Session {
        master: pair.master,
        writer: std::sync::Mutex::new(writer),
        killer: std::sync::Mutex::new(killer),
    };

    conn.sessions.lock().await.insert(session_id, session);

    // Start output reader thread
    let conn_for_reader = conn.tx.lock().await.clone();
    let session_id_for_reader = session_id;
    thread::spawn(move || {
        let mut buffer = [0_u8; 8192];
        let mut pending_utf8 = Vec::new();
        let mut reader = reader;
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => {
                    let data = decode_terminal_output(&mut pending_utf8, &buffer[..n]);
                    if !data.is_empty() {
                        let msg = ServerMessage::Output {
                            sessionId: session_id_for_reader,
                            data,
                        };
                        // Fire-and-forget: if the channel is closed the connection is gone.
                        let _ = conn_for_reader.try_send(msg);
                    }
                }
                Err(err) => {
                    let msg = ServerMessage::Output {
                        sessionId: session_id_for_reader,
                        data: format!("\r\n[terminal read error: {err}]\r\n"),
                    };
                    let _ = conn_for_reader.try_send(msg);
                    break;
                }
            }
        }
        if !pending_utf8.is_empty() {
            let data = String::from_utf8_lossy(&pending_utf8).to_string();
            let msg = ServerMessage::Output {
                sessionId: session_id_for_reader,
                data,
            };
            let _ = conn_for_reader.try_send(msg);
        }
    });

    // Start exit waiter thread
    let conn_for_exit = conn.tx.lock().await.clone();
    let session_id_for_exit = session_id;
    let sessions_for_exit = conn.sessions.clone();
    thread::spawn(move || {
        let status = child.wait();
        // Remove from connection sessions
        let rt = tokio::runtime::Handle::try_current();
        if let Ok(handle) = rt {
            let sessions = sessions_for_exit.clone();
            let sid = session_id_for_exit;
            handle.spawn(async move {
                let mut guard = sessions.lock().await;
                guard.remove(&sid);
            });
        }
        match status {
            Ok(status) => {
                let msg = ServerMessage::Exit {
                    sessionId: session_id_for_exit,
                    code: status.exit_code(),
                    signal: status.signal().map(ToString::to_string),
                };
                let _ = conn_for_exit.try_send(msg);
            }
            Err(err) => {
                let msg = ServerMessage::Output {
                    sessionId: session_id_for_exit,
                    data: format!("\r\n[terminal wait error: {err}]\r\n"),
                };
                let _ = conn_for_exit.try_send(msg);
            }
        }
    });

    Ok((
        session_id,
        shell,
        cwd_path.to_string_lossy().to_string(),
    ))
}

fn set_terminal_bash_path(config_dir: Option<&std::path::Path>, path: Option<String>) -> Result<(), String> {
    let mut config = TerminalConfig::load(config_dir);
    config.bash_path = normalize_terminal_bash_path(path)?;

    let path = terminal_config_path(config_dir).ok_or("terminal config path is unavailable")?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| format!("create terminal config directory: {err}"))?;
    }
    let data = serde_json::to_string_pretty(&config).map_err(|err| format!("serialize terminal config: {err}"))?;
    fs::write(&path, data).map_err(|err| format!("write terminal config: {err}"))?;
    Ok(())
}

fn normalize_terminal_bash_path(path: Option<String>) -> Result<Option<String>, String> {
    let Some(path) = path else { return Ok(None) };
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let bash_path = PathBuf::from(trimmed);
    if !bash_path.is_file() {
        return Err(format!("terminal bash path does not exist: {trimmed}"));
    }
    Ok(Some(trimmed.to_string()))
}

fn warmup_pty_system() {
    // Windows ConPTY (and cmd.exe's first attach to a ConPTY) pays a one-time
    // initialization cost that can surface as an implicit resize for the first
    // real session, causing cmd.exe to reprint its startup banner twice.
    // Warm up by opening a tiny PTY and briefly spawning cmd.exe so the real
    // terminals later avoid that first-attach quirk.
    #[cfg(target_os = "windows")]
    {
        thread::spawn(|| {
            let pty_system = native_pty_system();
            let Ok(pair) = pty_system.openpty(PtySize {
                rows: 8,
                cols: 20,
                pixel_width: 0,
                pixel_height: 0,
            }) else {
                return;
            };
            let mut cmd = CommandBuilder::new("cmd.exe");
            cmd.arg("/C");
            cmd.arg("exit");
            let Ok(mut child) = pair.slave.spawn_command(cmd) else {
                return;
            };
            drop(pair.slave);
            let start = Instant::now();
            loop {
                match child.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) if start.elapsed() < Duration::from_secs(3) => {
                        thread::sleep(Duration::from_millis(25));
                    }
                    _ => break,
                }
            }
        });
    }
}

// ─── Public entry point ────────────────────────────────────────────────────

pub async fn start_terminal_websocket_server(config_dir: Option<PathBuf>) -> Result<(), String> {
    warmup_pty_system();

    let listener = TcpListener::bind("0.0.0.0:0")
        .await
        .map_err(|e| format!("bind terminal websocket server: {e}"))?;

    let port = listener
        .local_addr()
        .map_err(|e| format!("read local address: {e}"))?
        .port();

    write_port_file(port);
    println!("[terminal-ws] Server listening on 0.0.0.0:{port}");

    let config_dir_clone = config_dir.clone();
    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, addr)) => {
                    let config_dir = config_dir_clone.clone();
                    tokio::spawn(handle_connection(config_dir, stream, addr));
                }
                Err(e) => {
                    eprintln!("[terminal-ws] accept error: {e}");
                }
            }
        }
    });

    Ok(())
}
