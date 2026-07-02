use std::net::SocketAddr;
use std::sync::Arc;

use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    routing::post,
    Json, Router,
};
use rand::distributions::{Alphanumeric, DistString};
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::mpsc;

use super::events::{HookEvent, HookEventKind};

/// HookReceiver から `InstanceManager` に渡されるディスパッチ用メッセージ。
#[derive(Debug, Clone)]
pub struct ReceivedHook {
    pub instance_id: String,
    pub kind: HookEventKind,
    /// 送信側マシンの時刻（epoch マイクロ秒）。順序逆転検出用。旧バイナリは None。
    pub sent_at_us: Option<u64>,
    pub payload: Value,
}

/// 起動済みサーバのハンドル。`endpoint`/`token` を tmux env に注入し、
/// `rx` から hook 通知を受け取って状態を更新する。
pub struct HookReceiverHandle {
    /// `http://127.0.0.1:<port>/hook` 形式
    pub endpoint: String,
    /// 認証トークン
    pub token: String,
    /// listen 中のポート番号（リモートで `ssh -R` するときに使う）
    pub port: u16,
    /// hook 通知の受信側
    pub rx: mpsc::Receiver<ReceivedHook>,
}

#[derive(Clone)]
struct AppState {
    token: Arc<String>,
    tx: mpsc::Sender<ReceivedHook>,
}

pub struct HookReceiver;

impl HookReceiver {
    /// 127.0.0.1 の動的ポートで HTTP サーバを起動し、ハンドルを返す。
    pub async fn start() -> anyhow::Result<HookReceiverHandle> {
        let token = Alphanumeric.sample_string(&mut rand::thread_rng(), 32);
        let token_arc = Arc::new(token.clone());
        let (tx, rx) = mpsc::channel::<ReceivedHook>(256);

        let state = AppState {
            token: Arc::clone(&token_arc),
            tx,
        };

        let app = Router::new()
            .route("/hook/health", post(health_handler))
            .route("/hook/:instance_id", post(hook_handler))
            .with_state(state);

        let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).await?;
        let local_addr = listener.local_addr()?;
        let port = local_addr.port();

        tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, app).await {
                eprintln!("[ccc] HookReceiver サーバ終了: {e}");
            }
        });

        Ok(HookReceiverHandle {
            endpoint: format!("http://127.0.0.1:{port}/hook"),
            token,
            port,
            rx,
        })
    }
}

fn check_auth(headers: &HeaderMap, expected: &str) -> bool {
    headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.strip_prefix("Bearer ").unwrap_or(s) == expected)
        .unwrap_or(false)
}

#[derive(Debug, Deserialize)]
struct HealthBody {
    #[allow(dead_code)]
    #[serde(rename = "type")]
    ty: Option<String>,
    instance_id: Option<String>,
    hook_binary_version: Option<String>,
    platform: Option<String>,
}

async fn health_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<HealthBody>,
) -> StatusCode {
    if !check_auth(&headers, &state.token) {
        return StatusCode::UNAUTHORIZED;
    }
    eprintln!(
        "[ccc] hook health_check: instance={:?} version={:?} platform={:?}",
        body.instance_id, body.hook_binary_version, body.platform
    );
    StatusCode::OK
}

async fn hook_handler(
    State(state): State<AppState>,
    Path(instance_id): Path<String>,
    headers: HeaderMap,
    Json(event): Json<HookEvent>,
) -> StatusCode {
    if !check_auth(&headers, &state.token) {
        return StatusCode::UNAUTHORIZED;
    }
    if event.instance_id != instance_id {
        return StatusCode::BAD_REQUEST;
    }

    let received = ReceivedHook {
        instance_id,
        kind: HookEventKind::from_str(&event.hook_event),
        sent_at_us: event.sent_at_us,
        payload: event.payload,
    };

    if state.tx.send(received).await.is_err() {
        return StatusCode::INTERNAL_SERVER_ERROR;
    }
    StatusCode::OK
}
