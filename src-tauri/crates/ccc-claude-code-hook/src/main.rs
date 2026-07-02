use std::io::Read;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use serde_json::{json, Value};

const VERSION: &str = env!("CARGO_PKG_VERSION");
const HEALTH_TIMEOUT_SECS: u64 = 3;
/// hook 転送のタイムアウト。リモート（ssh -R 逆転送）経由で1秒を超える
/// ケースがあり、短すぎるとイベントが黙って欠落して状態表示が固まる。
/// リトライ込み最悪 ~4秒。Claude Code 側の hook timeout (60s) には収まる。
const HOOK_TIMEOUT_SECS: u64 = 2;
/// 転送失敗時のリトライ回数（初回 + リトライ1回）。
const HOOK_RETRIES: u32 = 1;
/// リトライまでの待ち時間。
const HOOK_RETRY_DELAY_MS: u64 = 100;

#[derive(Parser)]
#[command(
    name = "ccc-claude-code-hook",
    version,
    about = "ccc hook bridge for Claude Code"
)]
struct Cli {
    /// ヘルスチェック payload を送信して終了する
    #[arg(long)]
    health_check: bool,
}

struct Env {
    endpoint: String,
    instance_id: String,
    token: String,
}

impl Env {
    /// 必須env3つが揃っていれば取得、欠けていれば None（no-op）
    fn from_env() -> Option<Self> {
        let endpoint = std::env::var("CCC_HOOK_ENDPOINT")
            .ok()
            .filter(|s| !s.is_empty())?;
        let instance_id = std::env::var("CCC_INSTANCE_ID")
            .ok()
            .filter(|s| !s.is_empty())?;
        let token = std::env::var("CCC_SESSION_TOKEN")
            .ok()
            .filter(|s| !s.is_empty())?;
        Some(Self {
            endpoint,
            instance_id,
            token,
        })
    }
}

fn platform_string() -> &'static str {
    // ビルド時に確定するtarget。ccc側のバイナリ選択と整合させる
    if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        "darwin-arm64"
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        "linux-arm64"
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        "linux-amd64"
    } else {
        "unknown"
    }
}

fn read_stdin() -> Result<String> {
    let mut buf = String::new();
    std::io::stdin()
        .read_to_string(&mut buf)
        .context("stdinの読み取りに失敗")?;
    Ok(buf)
}

/// hook event を ccc 受信サーバへ転送する。失敗は ccc 側の状態更新を欠落させるだけで、
/// claude code の動作はブロックしないため exit 0 を返す。
fn forward_hook(env: &Env, payload: Value) -> Result<()> {
    let hook_event = payload
        .get("hook_event_name")
        .and_then(|v| v.as_str())
        .unwrap_or("Unknown")
        .to_string();

    let url = format!("{}/{}", env.endpoint.trim_end_matches('/'), env.instance_id);
    // 送信側マシンの時刻（epoch マイクロ秒）。1インスタンスの hook は全て同一
    // マシンから送られるため単一の時刻ソースとして信頼でき、受信側は並行 POST で
    // 順序が入れ替わっても古いイベントを判別して捨てられる。
    let sent_at_us = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_micros() as u64);
    let body = json!({
        "instance_id": env.instance_id,
        "hook_event": hook_event,
        "sent_at_us": sent_at_us,
        "payload": payload,
    });

    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(HOOK_TIMEOUT_SECS))
        .build();
    for attempt in 0..=HOOK_RETRIES {
        let result = agent
            .post(&url)
            .set("Authorization", &format!("Bearer {}", env.token))
            .set("Content-Type", "application/json")
            .send_json(body.clone());
        match result {
            Ok(_) => return Ok(()),
            Err(e) if attempt < HOOK_RETRIES => {
                eprintln!("ccc-claude-code-hook: {hook_event} の転送に失敗（リトライします）: {e}");
                std::thread::sleep(Duration::from_millis(HOOK_RETRY_DELAY_MS));
            }
            Err(e) => {
                // 欠落は ccc 側の状態表示の劣化に留まる。claude code をブロック
                // しないため exit 0 のまま、調査用に stderr へ残す。
                eprintln!("ccc-claude-code-hook: {hook_event} の転送に失敗（諦めます）: {e}");
            }
        }
    }
    Ok(())
}

fn run_health_check(env: &Env) -> Result<()> {
    let url = format!("{}/health", env.endpoint.trim_end_matches('/'));
    let body = json!({
        "type": "health_check",
        "instance_id": env.instance_id,
        "hook_binary_version": VERSION,
        "platform": platform_string(),
    });

    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(HEALTH_TIMEOUT_SECS))
        .build();
    let resp = agent
        .post(&url)
        .set("Authorization", &format!("Bearer {}", env.token))
        .set("Content-Type", "application/json")
        .send_json(body)
        .map_err(|e| anyhow!("ヘルスチェックPOSTに失敗: {e}"))?;

    if resp.status() != 200 {
        return Err(anyhow!(
            "ヘルスチェックが200以外を返しました: {}",
            resp.status()
        ));
    }
    Ok(())
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    let Some(env) = Env::from_env() else {
        // ccc 管轄外の claude code 実行: 何もせず正常終了
        if cli.health_check {
            eprintln!("ccc-claude-code-hook: 必須環境変数(CCC_HOOK_ENDPOINT/CCC_INSTANCE_ID/CCC_SESSION_TOKEN)が未設定です");
            return ExitCode::from(2);
        }
        return ExitCode::SUCCESS;
    };

    if cli.health_check {
        return match run_health_check(&env) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("{e}");
                ExitCode::from(1)
            }
        };
    }

    let raw = match read_stdin() {
        Ok(s) => s,
        Err(_) => return ExitCode::SUCCESS,
    };
    let payload: Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(_) => return ExitCode::SUCCESS,
    };

    let _ = forward_hook(&env, payload);
    ExitCode::SUCCESS
}
