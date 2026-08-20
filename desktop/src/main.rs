use axum::{
    extract::State,
    http::{Method, StatusCode},
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use enigo::{Direction, Enigo, Key, Keyboard, Settings};
use local_ip_address::local_ip;
use serde::{Deserialize, Serialize};
use std::{
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};
use tower_http::cors::{Any, CorsLayer};
use tower_http::services::ServeDir;
use tracing::{error, info};

const PORT: u16 = 8765;

#[derive(Clone)]
struct AppState {
    last_text: Arc<Mutex<String>>,
}

#[derive(Deserialize)]
struct SyncRequest {
    text: String,
}

#[derive(Serialize)]
struct SyncResponse {
    ok: bool,
    message: String,
}

#[derive(Serialize)]
struct StatusResponse {
    ok: bool,
    ip: String,
    port: u16,
    last_text: String,
}

fn mobile_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("mobile")
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

async fn status_handler(State(state): State<AppState>) -> impl IntoResponse {
    let ip = local_ip()
        .map(|addr| addr.to_string())
        .unwrap_or_else(|_| "127.0.0.1".into());
    let last_text = state
        .last_text
        .lock()
        .map(|guard| guard.clone())
        .unwrap_or_default();

    Json(StatusResponse {
        ok: true,
        ip,
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
        .init();

    let mobile_path = mobile_dir();
    if !mobile_path.exists() {
        error!("找不到 mobile 目录: {}", mobile_path.display());
        std::process::exit(1);
    }

    let state = AppState {
        last_text: Arc::new(Mutex::new(String::new())),
    };

    let cors = CorsLayer::new()
        .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
        .allow_origin(Any)
        .allow_headers(Any);

    let app = Router::new()
        .route("/api/sync", post(sync_handler))
        .route("/api/status", get(status_handler))
        .route("/api/health", get(health_handler))
        .nest_service("/", ServeDir::new(mobile_path))
        .layer(cors)
        .with_state(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], PORT));
    let ip = local_ip()
        .map(|a| a.to_string())
        .unwrap_or_else(|_| "本机IP".into());

    info!("========================================");
    info!("  手机输入同步 - 电脑端已启动");
    info!("  局域网地址: http://{ip}:{PORT}");
    info!("  手机浏览器打开上面的地址即可使用");
    info!("  先在电脑上点好要输入的框，再点手机发送");
    info!("========================================");

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("绑定端口失败，请检查 {PORT} 是否被占用");

    axum::serve(listener, app).await.expect("服务异常退出");
}
