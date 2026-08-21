use axum::{
    body::Bytes,
    extract::{DefaultBodyLimit, State},
    http::{Method, StatusCode},
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use axum_server::tls_rustls::RustlsConfig;
use enigo::{Button, Coordinate, Direction, Enigo, Key, Keyboard, Mouse, Settings};
use local_ip_address::{list_afinet_netifas, local_ip};
use rcgen::{CertificateParams, KeyPair, SanType};
use serde::{Deserialize, Serialize};
use std::{
    io::{self, Write},
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::PathBuf,
    process::Command,
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};
use tower_http::cors::{Any, CorsLayer};
use tracing::{error, info};
use tracing_subscriber::fmt::time::ChronoLocal;

const MOBILE_INDEX: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../mobile/index.html"));

const PORT: u16 = 8765;
const HTTPS_PORT: u16 = 8766;

#[derive(Clone)]
struct AppState {
    last_text: Arc<Mutex<String>>,
    enigo: Arc<Mutex<Enigo>>,
}

#[derive(Deserialize)]
struct SyncRequest {
    text: String,
}

#[derive(Deserialize)]
struct KeyRequest {
    key: String,
}

#[derive(Deserialize)]
struct MouseMoveRequest {
    dx: i32,
    dy: i32,
}

#[derive(Deserialize)]
struct MouseClickRequest {
    #[serde(default = "default_mouse_button")]
    button: String,
}

#[derive(Serialize)]
struct SyncResponse {
    ok: bool,
    message: String,
}

#[derive(Serialize)]
struct SttResponse {
    ok: bool,
    text: String,
    message: String,
}

#[derive(Serialize)]
struct StatusResponse {
    ok: bool,
    ip: String,
    port: u16,
    last_text: String,
}

fn app_data_dir() -> PathBuf {
    if let Ok(local_app_data) = std::env::var("LOCALAPPDATA") {
        return PathBuf::from(local_app_data).join("phone-input-sync");
    }

    std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(|dir| dir.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("phone-input-sync-data")
}

struct InstanceLock {
    _file: std::fs::File,
}

#[cfg(windows)]
fn is_process_running(pid: u32) -> bool {
    use std::os::windows::process::CommandExt;
    use std::process::Command;

    const CREATE_NO_WINDOW: u32 = 0x08000000;

    let output = Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH"])
        .creation_flags(CREATE_NO_WINDOW)
        .output();

    match output {
        Ok(out) => String::from_utf8_lossy(&out.stdout).contains(&pid.to_string()),
        Err(_) => true,
    }
}

#[cfg(not(windows))]
fn is_process_running(pid: u32) -> bool {
    std::path::Path::new(&format!("/proc/{pid}")).exists()
}

fn acquire_single_instance() -> Result<InstanceLock, String> {
    let data_dir = app_data_dir();
    std::fs::create_dir_all(&data_dir).map_err(|e| format!("创建数据目录失败: {e}"))?;
    let lock_path = data_dir.join("instance.lock");

    if lock_path.exists() {
        let stale = std::fs::read_to_string(&lock_path)
            .ok()
            .and_then(|content| content.trim().parse::<u32>().ok())
            .map(|pid| pid == std::process::id() || !is_process_running(pid))
            .unwrap_or(true);

        if stale {
            let _ = std::fs::remove_file(&lock_path);
        } else {
            return Err(
                "手机输入同步已在运行。\n\n请查看任务栏里的黑色命令行窗口；若找不到，请在任务管理器中结束 phone-input-sync.exe 后再启动。"
                    .into(),
            );
        }
    }

    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&lock_path)
        .map_err(|error| {
            if error.kind() == io::ErrorKind::AlreadyExists {
                "手机输入同步已在运行。请查看任务栏或任务管理器。".into()
            } else {
                format!("创建实例锁失败: {error}")
            }
        })?;

    writeln!(file, "{}", std::process::id()).map_err(|e| format!("写入实例锁失败: {e}"))?;

    Ok(InstanceLock { _file: file })
}

fn pause_before_exit() {
    let _ = writeln!(io::stderr(), "\n按 Enter 键退出...");
    let _ = io::stderr().flush();
    let mut line = String::new();
    let _ = io::stdin().read_line(&mut line);
}

fn startup_failed(message: &str) -> ! {
    eprintln!("\n错误: {message}");
    pause_before_exit();
    std::process::exit(1);
}

fn is_usable_lan_ipv4(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            !v4.is_loopback()
                && !v4.is_link_local()
                && !v4.is_broadcast()
                && !v4.is_unspecified()
                && !v4.is_multicast()
        }
        IpAddr::V6(_) => false,
    }
}

fn lan_ipv4_list() -> Vec<Ipv4Addr> {
    let mut ips = Vec::new();

    if let Ok(interfaces) = list_afinet_netifas() {
        for (_, ip) in interfaces {
            if let IpAddr::V4(v4) = ip {
                if is_usable_lan_ipv4(ip) && !ips.contains(&v4) {
                    ips.push(v4);
                }
            }
        }
    }

    if ips.is_empty() {
        if let Ok(IpAddr::V4(v4)) = local_ip() {
            if is_usable_lan_ipv4(IpAddr::V4(v4)) {
                ips.push(v4);
            }
        }
    }

    ips
}

fn primary_lan_ip() -> String {
    lan_ipv4_list()
        .into_iter()
        .next()
        .map(|ip| ip.to_string())
        .unwrap_or_else(|| "127.0.0.1".into())
}

#[cfg(windows)]
fn run_netsh(args: &[&str]) -> Result<std::process::Output, String> {
    use std::os::windows::process::CommandExt;

    const CREATE_NO_WINDOW: u32 = 0x08000000;

    Command::new("netsh")
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| format!("执行 netsh 失败: {e}"))
}

#[cfg(windows)]
fn netsh_add_rule(args: &[&str]) -> Result<(), String> {
    let output = run_netsh(args)?;
    if output.status.success() {
        return Ok(());
    }

    // 不拼接 netsh 原文：中文 Windows 下多为 GBK，按 UTF-8 读会乱码
    Err("需要管理员权限才能放行防火墙".into())
}

#[cfg(windows)]
fn ensure_firewall_rules(exe_path: &std::path::Path) -> Result<(), String> {
    // 每次启动都清掉旧规则再重建：
    // 1) 避免残留「已禁用」或错误路径的同名规则
    // 2) 清掉 Windows 弹窗点「取消」后自动生成的阻止规则
    const APP_RULE: &str = "phone-input-sync";
    const HTTP_RULE: &str = "phone-input-sync-http";
    const HTTPS_RULE: &str = "phone-input-sync-https";

    let exe = exe_path.to_string_lossy();
    let program_arg = format!("program={exe}");
    let http_port = format!("localport={PORT}");
    let https_port = format!("localport={HTTPS_PORT}");

    let _ = run_netsh(&[
        "advfirewall",
        "firewall",
        "delete",
        "rule",
        &program_arg,
    ]);
    for name in [APP_RULE, HTTP_RULE, HTTPS_RULE] {
        let name_arg = format!("name={name}");
        let _ = run_netsh(&[
            "advfirewall",
            "firewall",
            "delete",
            "rule",
            &name_arg,
        ]);
    }

    let app_name = format!("name={APP_RULE}");
    let http_name = format!("name={HTTP_RULE}");
    let https_name = format!("name={HTTPS_RULE}");

    netsh_add_rule(&[
        "advfirewall",
        "firewall",
        "add",
        "rule",
        &app_name,
        "dir=in",
        "action=allow",
        &program_arg,
        "enable=yes",
        "profile=any",
    ])?;
    netsh_add_rule(&[
        "advfirewall",
        "firewall",
        "add",
        "rule",
        &http_name,
        "dir=in",
        "action=allow",
        "protocol=TCP",
        &http_port,
        "enable=yes",
        "profile=any",
    ])?;
    netsh_add_rule(&[
        "advfirewall",
        "firewall",
        "add",
        "rule",
        &https_name,
        "dir=in",
        "action=allow",
        "protocol=TCP",
        &https_port,
        "enable=yes",
        "profile=any",
    ])?;

    Ok(())
}

#[cfg(not(windows))]
fn ensure_firewall_rules(_exe_path: &std::path::Path) -> Result<(), String> {
    Ok(())
}

fn ensure_tls_cert(lan_ips: &[Ipv4Addr]) -> Result<(PathBuf, PathBuf), String> {
    let cert_dir = app_data_dir().join("certs");
    let cert_path = cert_dir.join("cert.pem");
    let key_path = cert_dir.join("key.pem");
    let meta_path = cert_dir.join("sans.txt");

    let mut expected_sans: Vec<String> = vec!["127.0.0.1".into(), "localhost".into()];
    for ip in lan_ips {
        expected_sans.push(ip.to_string());
    }
    expected_sans.sort();
    expected_sans.dedup();
    let expected_meta = expected_sans.join("\n");

    let reuse = cert_path.exists()
        && key_path.exists()
        && std::fs::read_to_string(&meta_path)
            .map(|content| content.trim() == expected_meta)
            .unwrap_or(false);

    if reuse {
        return Ok((cert_path, key_path));
    }

    std::fs::create_dir_all(&cert_dir).map_err(|e| format!("创建证书目录失败: {e}"))?;

    let mut params = CertificateParams::new(vec!["localhost".into()])
        .map_err(|e| format!("生成证书参数失败: {e}"))?;
    params
        .subject_alt_names
        .push(SanType::IpAddress(IpAddr::V4(Ipv4Addr::LOCALHOST)));
    for ip in lan_ips {
        params
            .subject_alt_names
            .push(SanType::IpAddress(IpAddr::V4(*ip)));
    }

    let key_pair = KeyPair::generate().map_err(|e| format!("生成密钥失败: {e}"))?;
    let cert = params
        .self_signed(&key_pair)
        .map_err(|e| format!("签发证书失败: {e}"))?;

    std::fs::write(&cert_path, cert.pem()).map_err(|e| format!("写入证书失败: {e}"))?;
    std::fs::write(&key_path, key_pair.serialize_pem())
        .map_err(|e| format!("写入密钥失败: {e}"))?;
    std::fs::write(&meta_path, expected_meta).map_err(|e| format!("写入证书元数据失败: {e}"))?;

    Ok((cert_path, key_path))
}

fn build_app(state: AppState) -> Router {
    let cors = CorsLayer::new()
        .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
        .allow_origin(Any)
        .allow_headers(Any);

    Router::new()
        .route("/api/sync", post(sync_handler))
        .route("/api/stt", post(stt_handler))
        .route("/api/key", post(key_handler))
        .route("/api/mouse/move", post(mouse_move_handler))
        .route("/api/mouse/click", post(mouse_click_handler))
        .route("/api/status", get(status_handler))
        .route("/api/health", get(health_handler))
        .route("/", get(mobile_index_handler))
        .route("/index.html", get(mobile_index_handler))
        .layer(DefaultBodyLimit::max(8 * 1024 * 1024))
        .layer(cors)
        .with_state(state)
}

async fn mobile_index_handler() -> impl IntoResponse {
    (
        [(axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8")],
        MOBILE_INDEX,
    )
}

async fn run_http_server(app: Router, addr: SocketAddr) -> Result<(), String> {
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| format!("绑定 HTTP 端口 {addr} 失败: {e}"))?;
    axum::serve(listener, app)
        .await
        .map_err(|e| format!("HTTP 服务异常退出: {e}"))
}

async fn run_https_server(
    app: Router,
    addr: SocketAddr,
    cert_path: PathBuf,
    key_path: PathBuf,
) -> Result<(), String> {
    let tls = RustlsConfig::from_pem_file(cert_path, key_path)
        .await
        .map_err(|e| format!("加载 TLS 证书失败: {e}"))?;

    axum_server::bind_rustls(addr, tls)
        .serve(app.into_make_service())
        .await
        .map_err(|e| format!("HTTPS 服务异常退出: {e}"))
}

fn default_mouse_button() -> String {
    "left".into()
}

fn parse_mouse_button(name: &str) -> Result<Button, String> {
    match name.trim().to_lowercase().as_str() {
        "left" => Ok(Button::Left),
        "right" => Ok(Button::Right),
        "middle" | "mid" => Ok(Button::Middle),
        other => Err(format!("不支持的鼠标键: {other}")),
    }
}

fn move_mouse_relative(state: &AppState, dx: i32, dy: i32) -> Result<(), String> {
    if dx == 0 && dy == 0 {
        return Ok(());
    }

    let mut enigo = state
        .enigo
        .lock()
        .map_err(|_| "鼠标控制器忙，请稍后重试".to_string())?;

    enigo
        .move_mouse(dx, dy, Coordinate::Rel)
        .map_err(|e| format!("移动鼠标失败: {e}"))
}

fn click_mouse_button(state: &AppState, button_name: &str) -> Result<(), String> {
    let button = parse_mouse_button(button_name)?;
    let mut enigo = state
        .enigo
        .lock()
        .map_err(|_| "鼠标控制器忙，请稍后重试".to_string())?;

    enigo
        .button(button, Direction::Click)
        .map_err(|e| format!("鼠标点击失败: {e}"))
}

fn parse_modifier(name: &str) -> Result<Key, String> {
    let normalized = name.trim().to_lowercase();
    match normalized.as_str() {
        "ctrl" | "control" => Ok(Key::Control),
        "shift" => Ok(Key::Shift),
        "alt" => Ok(Key::Alt),
        "win" | "windows" | "meta" | "super" => Ok(Key::Meta),
        other => Err(format!("不支持的修饰键: {other}")),
    }
}

fn parse_key_token(token: &str) -> Result<Key, String> {
    let name = token.trim().to_lowercase();
    if name.is_empty() {
        return Err("键名为空".into());
    }

    match name.as_str() {
        "enter" | "return" => Ok(Key::Return),
        "tab" => Ok(Key::Tab),
        "escape" | "esc" => Ok(Key::Escape),
        "space" | "spacebar" => Ok(Key::Space),
        "backspace" => Ok(Key::Backspace),
        "delete" | "del" => Ok(Key::Delete),
        "insert" | "ins" => Ok(Key::Insert),
        "home" => Ok(Key::Home),
        "end" => Ok(Key::End),
        "pageup" => Ok(Key::PageUp),
        "pagedown" => Ok(Key::PageDown),
        "up" | "uparrow" => Ok(Key::UpArrow),
        "down" | "downarrow" => Ok(Key::DownArrow),
        "left" | "leftarrow" => Ok(Key::LeftArrow),
        "right" | "rightarrow" => Ok(Key::RightArrow),
        "capslock" => Ok(Key::CapsLock),
        "pause" => Ok(Key::Pause),
        "f1" => Ok(Key::F1),
        "f2" => Ok(Key::F2),
        "f3" => Ok(Key::F3),
        "f4" => Ok(Key::F4),
        "f5" => Ok(Key::F5),
        "f6" => Ok(Key::F6),
        "f7" => Ok(Key::F7),
        "f8" => Ok(Key::F8),
        "f9" => Ok(Key::F9),
        "f10" => Ok(Key::F10),
        "f11" => Ok(Key::F11),
        "f12" => Ok(Key::F12),
        _ => {
            if name.len() == 1 {
                let ch = name.chars().next().expect("single char");
                return Ok(Key::Unicode(ch));
            }
            Err(format!(
                "不支持的键名: {token}（例如 enter、tab、ctrl+enter、f1）"
            ))
        }
    }
}

fn press_key_combo(spec: &str) -> Result<(), String> {
    let trimmed = spec.trim();
    if trimmed.is_empty() {
        return Err("键名为空".into());
    }

    let parts: Vec<&str> = trimmed
        .split('+')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect();

    if parts.is_empty() {
        return Err("键名为空".into());
    }

    let mut enigo = Enigo::new(&Settings::default()).map_err(|e| format!("键盘模拟失败: {e}"))?;

    if parts.len() == 1 {
        let key = parse_key_token(parts[0])?;
        enigo
            .key(key, Direction::Click)
            .map_err(|e| format!("按键失败: {e}"))?;
        return Ok(());
    }

    let modifiers = &parts[..parts.len() - 1];
    let main_key = parse_key_token(parts[parts.len() - 1])?;

    for modifier in modifiers {
        let key = parse_modifier(*modifier)?;
        enigo
            .key(key, Direction::Press)
            .map_err(|e| format!("按键失败: {e}"))?;
    }

    enigo
        .key(main_key, Direction::Click)
        .map_err(|e| format!("按键失败: {e}"))?;

    for modifier in modifiers.iter().rev() {
        let key = parse_modifier(*modifier)?;
        enigo
            .key(key, Direction::Release)
            .map_err(|e| format!("按键失败: {e}"))?;
    }

    Ok(())
}

/// Pure number token (e.g. `1`, `0.5`) means wait that many seconds in a multi-step sequence.
fn parse_wait_seconds(token: &str) -> Option<f64> {
    let trimmed = token.trim();
    if trimmed.is_empty() || trimmed == "." {
        return None;
    }

    let mut seen_dot = false;
    for ch in trimmed.chars() {
        if ch == '.' {
            if seen_dot {
                return None;
            }
            seen_dot = true;
        } else if !ch.is_ascii_digit() {
            return None;
        }
    }

    trimmed.parse().ok()
}

fn contains_cjk(text: &str) -> bool {
    text.chars().any(|ch| {
        matches!(
            ch,
            '\u{4E00}'..='\u{9FFF}'   // CJK Unified Ideographs
            | '\u{3400}'..='\u{4DBF}' // CJK Extension A
            | '\u{F900}'..='\u{FAFF}' // CJK Compatibility Ideographs
            | '\u{3000}'..='\u{303F}' // CJK Symbols and Punctuation
            | '\u{FF00}'..='\u{FFEF}' // Halfwidth and Fullwidth Forms
        )
    })
}

/// Press a key combo, or paste text when the step contains Chinese / CJK characters.
fn run_key_or_text_step(step: &str) -> Result<(), String> {
    if contains_cjk(step) {
        paste_into_focused_input(step)?;
        // Give the target app a moment to finish pasting before the next step.
        thread::sleep(Duration::from_millis(80));
        return Ok(());
    }
    press_key_combo(step)
}

/// Run a key sequence. Commas separate steps; a pure number is a wait in seconds.
/// Chinese / CJK text in a step is pasted into the focused input.
/// Examples: `w,f` → press w then f; `继续,enter` → paste 继续 then Enter;
/// `w,1,f` → press w, wait 1s, press f.
/// Without commas, behaves like a single combo (`ctrl+s`, `enter`) or a text paste.
fn run_key_sequence(spec: &str) -> Result<(), String> {
    let trimmed = spec.trim();
    if trimmed.is_empty() {
        return Err("键名为空".into());
    }

    // Accept Chinese full-width commas as step separators too.
    let normalized = trimmed.replace('，', ",");

    if !normalized.contains(',') {
        return run_key_or_text_step(&normalized);
    }

    let steps: Vec<&str> = normalized
        .split(',')
        .map(str::trim)
        .filter(|step| !step.is_empty())
        .collect();

    if steps.is_empty() {
        return Err("键名为空".into());
    }

    for step in steps {
        if let Some(seconds) = parse_wait_seconds(step) {
            if seconds < 0.0 {
                return Err(format!("等待时间不能为负数: {step}"));
            }
            if seconds > 0.0 {
                thread::sleep(Duration::from_secs_f64(seconds));
            }
            continue;
        }
        run_key_or_text_step(step)?;
    }

    Ok(())
}

fn paste_into_focused_input(text: &str) -> Result<(), String> {
    if text.is_empty() {
        return Err("文字为空".into());
    }

    let mut clipboard = arboard::Clipboard::new().map_err(|e| format!("剪贴板不可用: {e}"))?;
    clipboard
        .set_text(text)
        .map_err(|e| format!("写入剪贴板失败: {e}"))?;

    thread::sleep(Duration::from_millis(80));

    let mut enigo = Enigo::new(&Settings::default()).map_err(|e| format!("键盘模拟失败: {e}"))?;
    enigo
        .key(Key::Control, Direction::Press)
        .map_err(|e| format!("按键失败: {e}"))?;
    enigo
        .key(Key::Unicode('v'), Direction::Click)
        .map_err(|e| format!("粘贴失败: {e}"))?;
    enigo
        .key(Key::Control, Direction::Release)
        .map_err(|e| format!("按键失败: {e}"))?;

    Ok(())
}

async fn mouse_move_handler(
    State(state): State<AppState>,
    Json(body): Json<MouseMoveRequest>,
) -> impl IntoResponse {
    match move_mouse_relative(&state, body.dx, body.dy) {
        Ok(()) => (
            StatusCode::OK,
            Json(SyncResponse {
                ok: true,
                message: "ok".into(),
            }),
        ),
        Err(message) => (
            StatusCode::BAD_REQUEST,
            Json(SyncResponse { ok: false, message }),
        ),
    }
}

async fn mouse_click_handler(
    State(state): State<AppState>,
    Json(body): Json<MouseClickRequest>,
) -> impl IntoResponse {
    match click_mouse_button(&state, &body.button) {
        Ok(()) => {
            info!("已模拟鼠标点击: {}", body.button);
            (
                StatusCode::OK,
                Json(SyncResponse {
                    ok: true,
                    message: format!("已点击 {}", body.button),
                }),
            )
        }
        Err(message) => (
            StatusCode::BAD_REQUEST,
            Json(SyncResponse { ok: false, message }),
        ),
    }
}

async fn key_handler(Json(body): Json<KeyRequest>) -> impl IntoResponse {
    let key_spec = body.key.trim().to_string();
    let result = tokio::task::spawn_blocking(move || run_key_sequence(&key_spec))
        .await
        .unwrap_or_else(|e| Err(format!("按键任务失败: {e}")));

    match result {
        Ok(()) => {
            info!("已模拟按键: {}", body.key.trim());
            (
                StatusCode::OK,
                Json(SyncResponse {
                    ok: true,
                    message: format!("已在电脑上按下 {}", body.key.trim()),
                }),
            )
        }
        Err(message) => {
            error!("{message}");
            (
                StatusCode::BAD_REQUEST,
                Json(SyncResponse { ok: false, message }),
            )
        }
    }
}

async fn sync_handler(
    State(state): State<AppState>,
    Json(body): Json<SyncRequest>,
) -> impl IntoResponse {
    let text = body.text;

    match paste_into_focused_input(&text) {
        Ok(()) => {
            if let Ok(mut last) = state.last_text.lock() {
                *last = text.clone();
            }
            info!("已同步 {} 个字符到当前焦点", text.chars().count());
            (
                StatusCode::OK,
                Json(SyncResponse {
                    ok: true,
                    message: "已发送到电脑当前输入框".into(),
                }),
            )
        }
        Err(message) => {
            error!("{message}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(SyncResponse { ok: false, message }),
            )
        }
    }
}

fn transcribe_wav_with_windows_speech(wav_bytes: &[u8]) -> Result<String, String> {
    if wav_bytes.len() < 44 {
        return Err("录音太短或无效".into());
    }

    let stt_dir = app_data_dir().join("stt");
    std::fs::create_dir_all(&stt_dir).map_err(|e| format!("创建语音目录失败: {e}"))?;

    let id = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let wav_path = stt_dir.join(format!("utterance-{id}.wav"));
    let script_path = stt_dir.join(format!("recognize-{id}.ps1"));
    let out_path = stt_dir.join(format!("result-{id}.txt"));

    std::fs::write(&wav_path, wav_bytes).map_err(|e| format!("保存录音失败: {e}"))?;

    let wav_literal = wav_path.to_string_lossy().replace('\'', "''");
    let out_literal = out_path.to_string_lossy().replace('\'', "''");
    let script = format!(
        r#"$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Speech
$wav = '{wav_literal}'
$out = '{out_literal}'
$engine = $null
try {{
  $engine = New-Object System.Speech.Recognition.SpeechRecognitionEngine ([System.Globalization.CultureInfo]::new('zh-CN'))
}} catch {{
  $engine = New-Object System.Speech.Recognition.SpeechRecognitionEngine
}}
try {{
  $engine.LoadGrammar((New-Object System.Speech.Recognition.DictationGrammar))
  $engine.SetInputToWaveFile($wav)
  $engine.InitialSilenceTimeout = [TimeSpan]::FromSeconds(2)
  $engine.BabbleTimeout = [TimeSpan]::FromSeconds(2)
  $engine.EndSilenceTimeout = [TimeSpan]::FromSeconds(0.6)
  $result = $engine.Recognize([TimeSpan]::FromSeconds(45))
  $text = if ($null -eq $result) {{ '' }} else {{ $result.Text }}
  $utf8 = New-Object System.Text.UTF8Encoding $false
  [System.IO.File]::WriteAllText($out, $text, $utf8)
}} finally {{
  if ($null -ne $engine) {{ $engine.Dispose() }}
}}
"#
    );

    std::fs::write(&script_path, script).map_err(|e| format!("写入识别脚本失败: {e}"))?;

    let output = Command::new("powershell")
        .args([
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
            script_path.to_string_lossy().as_ref(),
        ])
        .output()
        .map_err(|e| format!("启动语音识别失败: {e}"))?;

    let text_result = if output.status.success() {
        std::fs::read_to_string(&out_path).map_err(|e| format!("读取识别结果失败: {e}"))
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr = stderr.trim();
        if stderr.contains("zh-CN") || stderr.contains("culture") || stderr.contains("Culture") {
            Err(
                "电脑未安装中文语音识别。请到 Windows 设置 → 时间和语言 → 语音，安装中文语音包后重试。"
                    .into(),
            )
        } else {
            Err(if stderr.is_empty() {
                "Windows 语音识别失败".into()
            } else {
                format!("Windows 语音识别失败: {stderr}")
            })
        }
    };

    let _ = std::fs::remove_file(&wav_path);
    let _ = std::fs::remove_file(&script_path);
    let _ = std::fs::remove_file(&out_path);

    let text = text_result?.trim().to_string();
    if text.is_empty() {
        return Err("没有识别到内容，请靠近麦克风再说一次".into());
    }
    Ok(text)
}

async fn stt_handler(body: Bytes) -> impl IntoResponse {
    if body.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(SttResponse {
                ok: false,
                text: String::new(),
                message: "录音为空".into(),
            }),
        );
    }

    let wav_bytes = body.to_vec();
    let result =
        tokio::task::spawn_blocking(move || transcribe_wav_with_windows_speech(&wav_bytes))
            .await
            .unwrap_or_else(|e| Err(format!("语音识别任务失败: {e}")));

    match result {
        Ok(text) => {
            info!("语音识别成功：{} 字", text.chars().count());
            (
                StatusCode::OK,
                Json(SttResponse {
                    ok: true,
                    text,
                    message: "识别完成".into(),
                }),
            )
        }
        Err(message) => {
            error!("语音识别失败: {message}");
            (
                StatusCode::BAD_REQUEST,
                Json(SttResponse {
                    ok: false,
                    text: String::new(),
                    message,
                }),
            )
        }
    }
}

async fn status_handler(State(state): State<AppState>) -> impl IntoResponse {
    let last_text = state
        .last_text
        .lock()
        .map(|guard| guard.clone())
        .unwrap_or_default();

    Json(StatusResponse {
        ok: true,
        ip: primary_lan_ip(),
        port: PORT,
        last_text,
    })
}

async fn health_handler() -> impl IntoResponse {
    Json(SyncResponse {
        ok: true,
        message: "phone-input-sync desktop is running".into(),
    })
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter("info")
        .with_target(false)
        .with_level(false)
        .with_ansi(false)
        .with_timer(ChronoLocal::new("%H:%M:%S".to_string()))
        .init();

    let _instance = match acquire_single_instance() {
        Ok(lock) => lock,
        Err(message) => startup_failed(&message),
    };

    let enigo = match Enigo::new(&Settings::default()) {
        Ok(enigo) => enigo,
        Err(error) => startup_failed(&format!("初始化鼠标/键盘模拟器失败: {error}")),
    };

    let lan_ips = lan_ipv4_list();

    match std::env::current_exe() {
        Ok(exe_path) => match ensure_firewall_rules(&exe_path) {
            Ok(()) => info!("已放行 Windows 防火墙（允许手机访问本程序）"),
            Err(message) => {
                error!("防火墙未放行: {message}");
                error!("手机打不开时：右键本程序 → 以管理员身份运行一次");
                error!("或在防火墙入站规则里删掉所有 phone-input-sync 相关项后重试");
            }
        },
        Err(error) => error!("无法获取程序路径，跳过防火墙配置: {error}"),
    }

    let (cert_path, key_path) = match ensure_tls_cert(&lan_ips) {
        Ok(paths) => paths,
        Err(message) => startup_failed(&message),
    };

    let state = AppState {
        last_text: Arc::new(Mutex::new(String::new())),
        enigo: Arc::new(Mutex::new(enigo)),
    };

    let http_app = build_app(state.clone());
    let https_app = build_app(state);

    let http_addr = SocketAddr::from(([0, 0, 0, 0], PORT));
    let https_addr = SocketAddr::from(([0, 0, 0, 0], HTTPS_PORT));

    info!("========================================");
    info!("  手机输入同步 - 电脑端已启动");
    if lan_ips.is_empty() {
        info!("  未检测到局域网 IP，请确认电脑已连 WiFi");
        info!("  本机测试: http://127.0.0.1:{PORT}");
    } else {
        for ip in &lan_ips {
            info!("  文字输入: http://{ip}:{PORT}");
            info!("  语音/陀螺仪: https://{ip}:{HTTPS_PORT}");
        }
    }
    info!("  手机与电脑须在同一 WiFi（不要用访客网络）");
    info!("  首次用 HTTPS 需在浏览器中信任证书（语音输入、陀螺仪鼠标需 HTTPS）");
    info!("  先在电脑上点好要输入的框，再点手机发送");
    info!("  请勿关闭此窗口，关闭后手机将无法连接");
    info!("========================================");

    tokio::select! {
        result = run_http_server(http_app, http_addr) => {
            if let Err(error) = result {
                startup_failed(&format!("HTTP 服务已停止: {error}"));
            }
        }
        result = run_https_server(https_app, https_addr, cert_path, key_path) => {
            if let Err(error) = result {
                startup_failed(&format!("HTTPS 服务已停止: {error}"));
            }
        }
        _ = tokio::signal::ctrl_c() => {
            info!("正在退出...");
        }
    }
}
