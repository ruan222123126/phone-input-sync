use axum::{
    body::Bytes,
    extract::{DefaultBodyLimit, State},
    http::{Method, StatusCode},
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use axum_server::tls_rustls::RustlsConfig;
use enigo::{Axis, Button, Coordinate, Direction, Enigo, Key, Keyboard, Mouse, Settings};
use local_ip_address::{list_afinet_netifas, local_ip};
use rcgen::{CertificateParams, KeyPair, SanType};
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    io::{self, Write},
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
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

#[derive(Clone, Debug)]
struct LockedWindow {
    hwnd: isize,
    title: String,
}

#[derive(Clone)]
struct AppState {
    last_text: Arc<Mutex<String>>,
    enigo: Arc<Mutex<Enigo>>,
    locked_window: Arc<Mutex<Option<LockedWindow>>>,
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

#[derive(Deserialize)]
struct MouseScrollRequest {
    #[serde(default)]
    dx: i32,
    #[serde(default)]
    dy: i32,
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
struct SttStatusResponse {
    ok: bool,
    ready: bool,
    message: String,
}

#[derive(Serialize)]
struct StatusResponse {
    ok: bool,
    ip: String,
    port: u16,
    last_text: String,
}

#[derive(Serialize)]
struct LockResponse {
    ok: bool,
    locked: bool,
    title: String,
    message: String,
}

#[cfg(windows)]
mod win_focus {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    use std::thread;
    use std::time::Duration;

    #[link(name = "user32")]
    extern "system" {
        fn GetForegroundWindow() -> isize;
        fn SetForegroundWindow(hwnd: isize) -> i32;
        fn IsWindow(hwnd: isize) -> i32;
        fn GetWindowTextW(hwnd: isize, lp_string: *mut u16, n_max_count: i32) -> i32;
        fn ShowWindow(hwnd: isize, n_cmd_show: i32) -> i32;
        fn GetWindowThreadProcessId(hwnd: isize, lpdw_process_id: *mut u32) -> u32;
        fn AttachThreadInput(id_attach: u32, id_attach_to: u32, f_attach: i32) -> i32;
        fn BringWindowToTop(hwnd: isize) -> i32;
        fn GetCurrentThreadId() -> u32;
        fn keybd_event(b_vk: u8, b_scan: u8, dw_flags: u32, dw_extra_info: usize);
    }

    const SW_RESTORE: i32 = 9;
    const VK_MENU: u8 = 0x12;
    const KEYEVENTF_KEYUP: u32 = 0x0002;

    pub fn capture_foreground_window() -> Option<(isize, String)> {
        unsafe {
            let hwnd = GetForegroundWindow();
            if hwnd == 0 {
                return None;
            }
            Some((hwnd, window_title(hwnd)))
        }
    }

    pub fn is_window_valid(hwnd: isize) -> bool {
        unsafe { hwnd != 0 && IsWindow(hwnd) != 0 }
    }

    pub fn focus_window(hwnd: isize) -> Result<(), String> {
        if !is_window_valid(hwnd) {
            return Err("锁定的窗口已关闭，请重新锁定".into());
        }

        unsafe {
            ShowWindow(hwnd, SW_RESTORE);

            let foreground = GetForegroundWindow();
            if foreground == hwnd {
                return Ok(());
            }

            let current_thread = GetCurrentThreadId();
            let fg_thread = GetWindowThreadProcessId(foreground, std::ptr::null_mut());
            let target_thread = GetWindowThreadProcessId(hwnd, std::ptr::null_mut());

            if fg_thread != 0 && fg_thread != current_thread {
                AttachThreadInput(current_thread, fg_thread, 1);
            }
            if target_thread != 0 && target_thread != current_thread {
                AttachThreadInput(current_thread, target_thread, 1);
            }

            // Windows 限制后台进程抢焦点，模拟 Alt 键可解除部分限制。
            keybd_event(VK_MENU, 0, 0, 0);
            keybd_event(VK_MENU, 0, KEYEVENTF_KEYUP, 0);

            let _ = BringWindowToTop(hwnd);
            let _ = SetForegroundWindow(hwnd);

            if fg_thread != 0 && fg_thread != current_thread {
                AttachThreadInput(current_thread, fg_thread, 0);
            }
            if target_thread != 0 && target_thread != current_thread {
                AttachThreadInput(current_thread, target_thread, 0);
            }
        }

        thread::sleep(Duration::from_millis(50));
        Ok(())
    }

    fn window_title(hwnd: isize) -> String {
        unsafe {
            let mut buf = vec![0u16; 512];
            let len = GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32);
            if len > 0 {
                buf.truncate(len as usize);
                OsString::from_wide(&buf).to_string_lossy().into_owned()
            } else {
                "(无标题)".into()
            }
        }
    }
}

#[cfg(not(windows))]
mod win_focus {
    pub fn capture_foreground_window() -> Option<(isize, String)> {
        None
    }

    pub fn is_window_valid(_hwnd: isize) -> bool {
        false
    }

    pub fn focus_window(_hwnd: isize) -> Result<(), String> {
        Err("窗口锁定仅支持 Windows".into())
    }
}

fn prepare_input_target(state: &AppState) -> Result<(), String> {
    let guard = state
        .locked_window
        .lock()
        .map_err(|_| "窗口锁定状态忙，请稍后重试".to_string())?;

    if let Some(win) = guard.as_ref() {
        if !win_focus::is_window_valid(win.hwnd) {
            return Err(format!("锁定的窗口「{}」已关闭，请重新锁定", win.title));
        }
        win_focus::focus_window(win.hwnd)?;
    }

    Ok(())
}

fn locked_window_message(state: &AppState) -> String {
    state
        .locked_window
        .lock()
        .ok()
        .and_then(|guard| guard.as_ref().map(|win| format!("已发送到锁定窗口「{}」", win.title)))
        .unwrap_or_else(|| "已发送到电脑当前输入框".into())
}

fn app_data_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(|dir| dir.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
}

fn whisper_dir() -> PathBuf {
    app_data_dir().join("whisper")
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

    // 同目录已有证书则直接复用，避免反复重建导致手机要重新信任
    if cert_path.is_file() && key_path.is_file() {
        info!(
            "已使用程序目录下已有的 HTTPS 证书: {}",
            cert_path.display()
        );
        return Ok((cert_path, key_path));
    }

    let mut expected_sans: Vec<String> = vec!["127.0.0.1".into(), "localhost".into()];
    for ip in lan_ips {
        expected_sans.push(ip.to_string());
    }
    expected_sans.sort();
    expected_sans.dedup();
    let expected_meta = expected_sans.join("\n");

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
    info!("已在程序目录生成 HTTPS 证书: {}", cert_path.display());

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
        .route("/api/stt/status", get(stt_status_handler))
        .route("/api/stt/prepare", post(stt_prepare_handler))
        .route("/api/key", post(key_handler))
        .route("/api/mouse/move", post(mouse_move_handler))
        .route("/api/mouse/click", post(mouse_click_handler))
        .route("/api/mouse/scroll", post(mouse_scroll_handler))
        .route("/api/lock", post(lock_handler))
        .route("/api/unlock", post(unlock_handler))
        .route("/api/lock/status", get(lock_status_handler))
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

fn scroll_mouse(state: &AppState, dx: i32, dy: i32) -> Result<(), String> {
    if dx == 0 && dy == 0 {
        return Ok(());
    }

    let mut enigo = state
        .enigo
        .lock()
        .map_err(|_| "鼠标控制器忙，请稍后重试".to_string())?;

    // enigo: Vertical 正数向下滚，负数向上滚；Horizontal 正数向右，负数向左
    if dy != 0 {
        enigo
            .scroll(dy, Axis::Vertical)
            .map_err(|e| format!("鼠标滚轮失败: {e}"))?;
    }
    if dx != 0 {
        enigo
            .scroll(dx, Axis::Horizontal)
            .map_err(|e| format!("鼠标滚轮失败: {e}"))?;
    }

    Ok(())
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

async fn mouse_scroll_handler(
    State(state): State<AppState>,
    Json(body): Json<MouseScrollRequest>,
) -> impl IntoResponse {
    match scroll_mouse(&state, body.dx, body.dy) {
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

async fn key_handler(
    State(state): State<AppState>,
    Json(body): Json<KeyRequest>,
) -> impl IntoResponse {
    let key_spec = body.key.trim().to_string();
    let state_for_task = state.clone();
    let result = tokio::task::spawn_blocking(move || {
        prepare_input_target(&state_for_task)?;
        run_key_sequence(&key_spec)
    })
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
    let char_count = text.chars().count();
    let text_for_last = text.clone();
    let state_for_task = state.clone();

    let result = tokio::task::spawn_blocking(move || {
        prepare_input_target(&state_for_task)?;
        paste_into_focused_input(&text)
    })
    .await
    .unwrap_or_else(|e| Err(format!("同步任务失败: {e}")));

    match result {
        Ok(()) => {
            if let Ok(mut last) = state.last_text.lock() {
                *last = text_for_last;
            }
            let message = locked_window_message(&state);
            info!("已同步 {} 个字符", char_count);
            (
                StatusCode::OK,
                Json(SyncResponse { ok: true, message }),
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

async fn lock_handler(State(state): State<AppState>) -> impl IntoResponse {
    match win_focus::capture_foreground_window() {
        Some((hwnd, title)) => {
            if let Ok(mut locked) = state.locked_window.lock() {
                *locked = Some(LockedWindow { hwnd, title: title.clone() });
            }
            info!("已锁定窗口: {title}");
            (
                StatusCode::OK,
                Json(LockResponse {
                    ok: true,
                    locked: true,
                    title,
                    message: "已锁定当前电脑窗口，之后发送会自动输入到该窗口".into(),
                }),
            )
        }
        None => (
            StatusCode::BAD_REQUEST,
            Json(LockResponse {
                ok: false,
                locked: false,
                title: String::new(),
                message: "无法获取当前窗口，请先在电脑上点击目标窗口".into(),
            }),
        ),
    }
}

async fn unlock_handler(State(state): State<AppState>) -> impl IntoResponse {
    if let Ok(mut locked) = state.locked_window.lock() {
        *locked = None;
    }
    info!("已取消窗口锁定");
    (
        StatusCode::OK,
        Json(LockResponse {
            ok: true,
            locked: false,
            title: String::new(),
            message: "已取消锁定，将发送到电脑当前焦点".into(),
        }),
    )
}

async fn lock_status_handler(State(state): State<AppState>) -> impl IntoResponse {
    let mut locked = false;
    let mut title = String::new();
    let mut message = "未锁定窗口".to_string();

    if let Ok(mut guard) = state.locked_window.lock() {
        if let Some(win) = guard.as_ref() {
            if win_focus::is_window_valid(win.hwnd) {
                locked = true;
                title = win.title.clone();
                message = format!("已锁定「{title}」");
            } else {
                *guard = None;
                message = "锁定的窗口已关闭，请重新锁定".into();
            }
        }
    }

    Json(LockResponse {
        ok: true,
        locked,
        title,
        message,
    })
}

const WHISPER_BIN_URLS: &[&str] = &[
    "https://github.com/ggml-org/whisper.cpp/releases/download/b4938/whisper-bin-x64.zip",
    "https://ghfast.top/https://github.com/ggml-org/whisper.cpp/releases/download/b4938/whisper-bin-x64.zip",
];

const WHISPER_MODEL_NAME: &str = "ggml-base.bin";
const WHISPER_MODEL_URLS: &[&str] = &[
    "https://hf-mirror.com/ggerganov/whisper.cpp/resolve/main/ggml-base.bin",
    "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.bin",
];

fn download_file(urls: &[&str], dest: &Path, label: &str) -> Result<(), String> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("创建目录失败: {e}"))?;
    }

    let tmp = dest.with_extension("download");
    let mut last_error = String::from("无可用下载地址");

    for url in urls {
        info!("正在下载{label}: {url}");
        let dest_literal = tmp.to_string_lossy().replace('\'', "''");
        let url_literal = url.replace('\'', "''");
        let script = format!(
            "$ErrorActionPreference = 'Stop'; Invoke-WebRequest -Uri '{url_literal}' -OutFile '{dest_literal}' -UseBasicParsing"
        );

        let output = Command::new("powershell")
            .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-Command", &script])
            .output()
            .map_err(|e| format!("启动下载失败: {e}"))?;

        if output.status.success() && tmp.is_file() {
            std::fs::rename(&tmp, dest).map_err(|e| format!("保存文件失败: {e}"))?;
            info!("已下载{label}: {}", dest.display());
            return Ok(());
        }

        last_error = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if last_error.is_empty() {
            last_error = String::from_utf8_lossy(&output.stdout).trim().to_string();
        }
        if last_error.is_empty() {
            last_error = "下载失败".into();
        }
        let _ = std::fs::remove_file(&tmp);
    }

    Err(format!("下载{label}失败: {last_error}"))
}

fn unzip_whisper_bin(zip_path: &Path, dest_dir: &Path) -> Result<(), String> {
    let file = File::open(zip_path).map_err(|e| format!("打开压缩包失败: {e}"))?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| format!("读取压缩包失败: {e}"))?;

    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| format!("读取压缩项失败: {e}"))?;
        let name = entry.name().replace('\\', "/");
        let file_name = Path::new(&name)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .filter(|n| !n.is_empty());
        let Some(file_name) = file_name else {
            continue;
        };
        if entry.is_dir() {
            continue;
        }

        let out_path = dest_dir.join(&file_name);
        let mut out = File::create(&out_path).map_err(|e| format!("解压写入失败: {e}"))?;
        io::copy(&mut entry, &mut out).map_err(|e| format!("解压复制失败: {e}"))?;
    }

    Ok(())
}

fn find_whisper_exe(dir: &Path) -> Option<PathBuf> {
    const CANDIDATES: &[&str] = &["whisper-cli.exe", "main.exe", "whisper.exe"];
    for name in CANDIDATES {
        let path = dir.join(name);
        if path.is_file() {
            return Some(path);
        }
    }
    None
}

fn file_nonempty(path: &Path, min_bytes: u64) -> bool {
    std::fs::metadata(path)
        .map(|meta| meta.is_file() && meta.len() >= min_bytes)
        .unwrap_or(false)
}

fn find_whisper_model(dirs: &[PathBuf]) -> Option<PathBuf> {
    const PREFERRED: &[&str] = &[
        "ggml-base.bin",
        "ggml-small.bin",
        "ggml-tiny.bin",
        "ggml-medium.bin",
    ];
    // 至少几 MB，避免把下到一半的空文件当成可用模型
    const MIN_MODEL_BYTES: u64 = 5 * 1024 * 1024;

    for dir in dirs {
        for name in PREFERRED {
            let path = dir.join(name);
            if file_nonempty(&path, MIN_MODEL_BYTES) {
                return Some(path);
            }
        }

        if let Ok(entries) = std::fs::read_dir(dir) {
            let mut matches: Vec<PathBuf> = entries
                .filter_map(|entry| entry.ok())
                .map(|entry| entry.path())
                .filter(|path| {
                    path.extension()
                        .and_then(|ext| ext.to_str())
                        .map(|ext| ext.eq_ignore_ascii_case("bin"))
                        .unwrap_or(false)
                        && path
                            .file_name()
                            .and_then(|name| name.to_str())
                            .map(|name| name.to_ascii_lowercase().starts_with("ggml-"))
                            .unwrap_or(false)
                        && file_nonempty(path, MIN_MODEL_BYTES)
                })
                .collect();
            matches.sort();
            if let Some(path) = matches.into_iter().next() {
                return Some(path);
            }
        }
    }

    None
}

fn whisper_search_dirs() -> Vec<PathBuf> {
    let root = app_data_dir();
    let nested = whisper_dir();
    if nested == root {
        vec![nested]
    } else {
        vec![nested, root]
    }
}

fn whisper_runtime_status() -> (bool, String) {
    let search_dirs = whisper_search_dirs();
    let has_model = find_whisper_model(&search_dirs).is_some();
    let has_exe = search_dirs.iter().any(|dir| find_whisper_exe(dir).is_some());
    match (has_exe, has_model) {
        (true, true) => (true, "语音模型已就绪".into()),
        (false, true) => (false, "已有模型，但缺少语音引擎".into()),
        (true, false) => (false, "已有引擎，但缺少语音模型".into()),
        (false, false) => (false, "未安装语音模型".into()),
    }
}

fn ensure_whisper_runtime() -> Result<(PathBuf, PathBuf), String> {
    let search_dirs = whisper_search_dirs();
    let install_dir = whisper_dir();

    let existing_model = find_whisper_model(&search_dirs);
    let existing_exe = search_dirs.iter().find_map(|dir| find_whisper_exe(dir));

    if let (Some(exe), Some(model)) = (existing_exe.clone(), existing_model.clone()) {
        info!(
            "已使用本地 Whisper（跳过下载）：引擎={} 模型={}",
            exe.display(),
            model.display()
        );
        return Ok((exe, model));
    }

    std::fs::create_dir_all(&install_dir).map_err(|e| format!("创建 whisper 目录失败: {e}"))?;

    let model_path = if let Some(model) = existing_model {
        info!("已使用本地语音模型: {}", model.display());
        model
    } else {
        let model_path = install_dir.join(WHISPER_MODEL_NAME);
        info!("未找到本地语音模型，开始下载（约 148MB）…");
        download_file(WHISPER_MODEL_URLS, &model_path, "语音模型")?;
        model_path
    };

    let exe = if let Some(exe) = existing_exe {
        info!("已使用本地语音引擎: {}", exe.display());
        exe
    } else {
        let zip_path = install_dir.join("whisper-bin-x64.zip");
        info!("未找到本地语音引擎，开始下载…");
        download_file(WHISPER_BIN_URLS, &zip_path, "语音引擎")?;
        unzip_whisper_bin(&zip_path, &install_dir)?;
        let _ = std::fs::remove_file(&zip_path);
        find_whisper_exe(&install_dir).ok_or_else(|| {
            "未找到 whisper 可执行文件，请删除程序目录下的 whisper 文件夹后重试".to_string()
        })?
    };

    Ok((exe, model_path))
}

fn cleanup_whisper_hallucinations(text: &str) -> String {
    let trimmed = text.trim();
    const NOISE: &[&str] = &[
        "字幕by索兰娅",
        "字幕 by",
        "谢谢观看",
        "感謝收看",
        "请不吝点赞",
        "订阅",
        "打赏",
        "明镜与点点栏目",
    ];
    for noise in NOISE {
        if trimmed == *noise || trimmed.contains(noise) && trimmed.chars().count() <= 20 {
            return String::new();
        }
    }
    trimmed.to_string()
}

fn transcribe_wav_with_whisper(wav_bytes: &[u8]) -> Result<String, String> {
    if wav_bytes.len() < 44 {
        return Err("录音太短或无效".into());
    }

    let (exe, model_path) = ensure_whisper_runtime()?;

    let stt_dir = app_data_dir().join("stt");
    std::fs::create_dir_all(&stt_dir).map_err(|e| format!("创建语音目录失败: {e}"))?;

    let id = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let wav_path = stt_dir.join(format!("utterance-{id}.wav"));
    let out_base = stt_dir.join(format!("result-{id}"));
    let out_txt = stt_dir.join(format!("result-{id}.txt"));

    std::fs::write(&wav_path, wav_bytes).map_err(|e| format!("保存录音失败: {e}"))?;

    let model_arg = model_path.to_string_lossy().into_owned();
    let wav_arg = wav_path.to_string_lossy().into_owned();
    let out_arg = out_base.to_string_lossy().into_owned();
    // Whisper 默认中文常出繁体；用简体 prompt 引导输出简体
    let prompt_arg = "以下是普通话的句子。";

    let output = Command::new(&exe)
        .current_dir(whisper_dir())
        .args([
            "-m",
            &model_arg,
            "-f",
            &wav_arg,
            "-l",
            "zh",
            "--prompt",
            prompt_arg,
            "-nt",
            "-np",
            "-otxt",
            "-of",
            &out_arg,
        ])
        .output()
        .map_err(|e| format!("启动 Whisper 失败: {e}"))?;

    let text_result = if out_txt.is_file() {
        std::fs::read_to_string(&out_txt).map_err(|e| format!("读取识别结果失败: {e}"))
    } else if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(format!(
            "Whisper 识别失败: {}",
            stderr.trim().chars().take(240).collect::<String>()
        ))
    };

    let _ = std::fs::remove_file(&wav_path);
    let _ = std::fs::remove_file(&out_txt);

    let text = cleanup_whisper_hallucinations(&text_result?);
    if text.is_empty() {
        return Err("没有识别到内容，请靠近麦克风、说清楚后再试".into());
    }
    Ok(text)
}

async fn stt_status_handler() -> impl IntoResponse {
    let (ready, message) = whisper_runtime_status();
    Json(SttStatusResponse {
        ok: true,
        ready,
        message,
    })
}

async fn stt_prepare_handler() -> impl IntoResponse {
    let result = tokio::task::spawn_blocking(|| ensure_whisper_runtime().map(|_| ()))
        .await
        .unwrap_or_else(|e| Err(format!("准备语音模型任务失败: {e}")));

    match result {
        Ok(()) => {
            info!("语音模型已准备就绪");
            (
                StatusCode::OK,
                Json(SttStatusResponse {
                    ok: true,
                    ready: true,
                    message: "语音模型已准备就绪".into(),
                }),
            )
        }
        Err(message) => {
            error!("准备语音模型失败: {message}");
            (
                StatusCode::BAD_REQUEST,
                Json(SttStatusResponse {
                    ok: false,
                    ready: false,
                    message,
                }),
            )
        }
    }
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
    let result = tokio::task::spawn_blocking(move || transcribe_wav_with_whisper(&wav_bytes))
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
        locked_window: Arc::new(Mutex::new(None)),
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
    info!("  先在电脑上点好要输入的框，手机可点「锁定电脑窗口」固定目标");
    info!("  未锁定时仍发送到电脑当前焦点；锁定后发送会自动切回该窗口");
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
