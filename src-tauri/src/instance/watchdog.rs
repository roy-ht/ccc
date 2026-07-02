//! AgentBusy 固着を補正するウォッチドッグ。
//!
//! Claude Code は Esc 割り込みで停止した場合に Stop hook を発火しない仕様のため、
//! hook イベント駆動の状態管理では「作業中」表示のまま固まる。実測（archive DB
//! 直近7日）では全ターンの約15%が Stop なしで終了し、自己修復は起きていなかった。
//!
//! 補正は2トリガー:
//! - Esc 検知: busy 中のインスタンスへ単独 Esc (0x1b) が書き込まれたとき
//!   （`manager::InstanceManager::write` から呼ばれる）
//! - 定期保険: 60秒間隔で「busy かつ 90秒以上 hook 無音」のインスタンスを巡回
//!   （hook 送信欠落や ccc 外からの tmux 直接操作で固まったケースの回収）
//!
//! いずれも idle 化の前に必ず transcript 末尾の中断マーカー
//! （`transcript::INTERRUPT_MARKER_PREFIX`）を確認する。誤検知ゼロを優先し、
//! マーカーが確認できない限り busy のまま維持する。

use dashmap::DashMap;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::ssh_config;

use super::notify;
use super::storage;
use super::transcript;
use super::types::{InstanceId, InstanceInfo, InstanceStatus};

/// Esc 検知から transcript 確認までの猶予。
/// 中断ではなく正常完了だった場合に Stop hook が先に届くのを待つ。
const CONFIRM_DELAY: Duration = Duration::from_millis(2500);
/// transcript 取得失敗時（ssh エラー等）のリトライまでの待ち。リトライは1回のみ。
const RETRY_DELAY: Duration = Duration::from_secs(5);
/// 定期保険ポーリングの間隔。
const POLL_INTERVAL: Duration = Duration::from_secs(60);
/// 定期ポーリングで確認対象とみなす hook 無音時間。
const POLL_SILENCE: Duration = Duration::from_secs(90);
/// リモート transcript tail 取得の ssh 接続タイムアウト（秒）。
const SSH_CONNECT_TIMEOUT_SECS: u32 = 5;

/// ウォッチドッグ1回分の確認に必要なハンドル一式。
/// `InstanceManager` の Arc フィールドをクローンして組み立てる。
pub(crate) struct WatchdogCtx {
    pub infos: Arc<DashMap<InstanceId, InstanceInfo>>,
    pub app_handle: Option<tauri::AppHandle>,
    /// hook 受信時刻（`apply_hook` が更新）。Esc 以降に hook が届いていれば
    /// 状態は最新なので補正しない。
    pub last_hook_at: Arc<DashMap<InstanceId, Instant>>,
    /// 確認タスクの多重起動防止（Esc 連打・ポーリング重複対策）。
    pub suspects: Arc<DashMap<InstanceId, Instant>>,
}

/// 単独 Esc 書き込み検知時のエントリポイント。
/// busy でない・確認タスクが既に走っている場合は何もしない。
pub(crate) fn on_escape_input(ctx: WatchdogCtx, id: &str) {
    if !is_busy(&ctx.infos, id) {
        return;
    }
    spawn_confirm(ctx, id.to_string(), CONFIRM_DELAY);
}

/// 定期保険ポーリングのループを起動する。
/// `set_app_handle` 後（setup 完了後）に1回だけ呼ぶこと。
pub(crate) fn spawn_periodic(make_ctx: impl Fn() -> WatchdogCtx + Send + 'static) {
    tauri::async_runtime::spawn(async move {
        let mut interval = tokio::time::interval(POLL_INTERVAL);
        // 起動直後の tick はスキップ（interval は即時に1回 tick する）
        interval.tick().await;
        loop {
            interval.tick().await;
            let ctx = make_ctx();
            let stuck: Vec<InstanceId> = ctx
                .infos
                .iter()
                .filter(|e| e.value().status == InstanceStatus::AgentBusy)
                .map(|e| e.key().clone())
                .filter(|id| {
                    ctx.last_hook_at
                        .get(id)
                        .is_some_and(|t| t.elapsed() >= POLL_SILENCE)
                })
                .collect();
            for id in stuck {
                spawn_confirm(make_ctx(), id, Duration::ZERO);
            }
        }
    });
}

/// 確認タスクを spawn する（多重起動は suspects で抑止）。
fn spawn_confirm(ctx: WatchdogCtx, id: InstanceId, initial_delay: Duration) {
    let now = Instant::now();
    // entry API で「不在時のみ挿入」をアトミックに行う
    match ctx.suspects.entry(id.clone()) {
        dashmap::mapref::entry::Entry::Occupied(_) => return,
        dashmap::mapref::entry::Entry::Vacant(v) => {
            v.insert(now);
        }
    }
    tauri::async_runtime::spawn(async move {
        confirm_interrupt(&ctx, &id, now, initial_delay).await;
        ctx.suspects.remove(&id);
    });
}

/// 中断疑いの確認本体。`since` 以降に hook が届いた・busy でなくなった時点で打ち切る。
/// transcript 取得失敗は1回だけリトライし、それでも失敗なら busy のまま諦める。
async fn confirm_interrupt(ctx: &WatchdogCtx, id: &str, since: Instant, initial_delay: Duration) {
    for attempt in 0..2u8 {
        let delay = if attempt == 0 {
            initial_delay
        } else {
            RETRY_DELAY
        };
        tokio::time::sleep(delay).await;

        if hook_arrived_after(ctx, id, since) || !is_busy(&ctx.infos, id) {
            return;
        }
        let Some(target) = snapshot_target(&ctx.infos, id) else {
            // transcript_path 未確定（hook を一度も受けていない）→ 判定不能
            return;
        };

        match fetch_tail(&target).await {
            Ok(tail) => {
                // 取得が遅延した場合に備え、判定直前にもう一度状態を確認する
                if hook_arrived_after(ctx, id, since) || !is_busy(&ctx.infos, id) {
                    return;
                }
                if transcript::tail_indicates_interrupt(&tail) {
                    apply_interrupted_idle(ctx, id);
                }
                // マーカー無し = 本当に作業中（長時間ツール実行等）→ busy 維持
                return;
            }
            Err(e) if attempt == 0 => {
                eprintln!("[ccc] watchdog: {id} の transcript 取得失敗（リトライします）: {e}");
            }
            Err(e) => {
                eprintln!("[ccc] watchdog: {id} の transcript 取得失敗（諦めて busy 維持）: {e}");
            }
        }
    }
}

/// 確認対象の transcript 取得方法。
enum TailTarget {
    Local { path: String },
    Remote { host_alias: String, path: String },
}

fn is_busy(infos: &DashMap<InstanceId, InstanceInfo>, id: &str) -> bool {
    infos
        .get(id)
        .is_some_and(|i| i.status == InstanceStatus::AgentBusy)
}

fn hook_arrived_after(ctx: &WatchdogCtx, id: &str, since: Instant) -> bool {
    ctx.last_hook_at.get(id).is_some_and(|t| *t > since)
}

fn snapshot_target(infos: &DashMap<InstanceId, InstanceInfo>, id: &str) -> Option<TailTarget> {
    let info = infos.get(id)?;
    let path = info.last_transcript_path.clone()?;
    Some(match info.host_alias.clone() {
        Some(host_alias) => TailTarget::Remote { host_alias, path },
        None => TailTarget::Local { path },
    })
}

async fn fetch_tail(target: &TailTarget) -> Result<String, String> {
    match target {
        TailTarget::Local { path } => {
            let path = path.clone();
            tokio::task::spawn_blocking(move || {
                transcript::read_tail(Path::new(&path), transcript::TAIL_BYTES)
                    .map(|(tail, _)| tail)
                    .map_err(|e| format!("read_tail 失敗 ({path}): {e}"))
            })
            .await
            .map_err(|e| format!("spawn_blocking 失敗: {e}"))?
        }
        TailTarget::Remote { host_alias, path } => {
            let host = host_alias.clone();
            let path = path.clone();
            tokio::task::spawn_blocking(move || fetch_remote_tail(&host, &path))
                .await
                .map_err(|e| format!("spawn_blocking 失敗: {e}"))?
        }
    }
}

/// リモートの transcript 末尾を ssh 経由で取得する。
/// ccc 専用 ControlMaster があれば multiplex され、無ければ直接続にフォールバック
/// する（BatchMode なので対話認証が必要な場合は即失敗 → busy 維持）。
fn fetch_remote_tail(host_alias: &str, path: &str) -> Result<String, String> {
    let quoted = shell_single_quote(path);
    let output = Command::new("ssh")
        .args([
            "-o",
            "BatchMode=yes",
            "-o",
            &format!("ConnectTimeout={SSH_CONNECT_TIMEOUT_SECS}"),
            "-o",
            &format!("ControlPath={}", ssh_config::CCC_CONTROL_PATH),
            host_alias,
            &format!("tail -c {} {quoted}", transcript::TAIL_BYTES),
        ])
        .output()
        .map_err(|e| format!("ssh 起動失敗: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "ssh tail が非ゼロ終了 ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// POSIX shell の単一引用符エスケープ。
fn shell_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// 中断確認が取れたインスタンスを AgentIdle に補正し、永続化とフロント通知を行う。
fn apply_interrupted_idle(ctx: &WatchdogCtx, id: &str) {
    let snapshot = {
        let Some(mut info) = ctx.infos.get_mut(id) else {
            return;
        };
        if info.status != InstanceStatus::AgentBusy {
            return;
        }
        info.status = InstanceStatus::AgentIdle;
        info.status_message = Some("中断されました".to_string());
        info.pending_prompt = None;
        let _ = storage::save_connection(&info);
        info.clone()
    };
    eprintln!("[ccc] watchdog: {id} を中断（推定）として AgentIdle に補正");
    notify::emit_status_changed(&ctx.app_handle, &snapshot);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_single_quote_escapes_quotes() {
        assert_eq!(shell_single_quote("/a/b.jsonl"), "'/a/b.jsonl'");
        assert_eq!(shell_single_quote("a'b"), r"'a'\''b'");
    }
}
