//! シャドウスクリーンによる画面状態検出（v0.12）。
//!
//! 全インスタンスの PTY バイト列は relay を全量通過するため、headless 端末
//! エミュレータ（vt100）を挟んで「描画後の画面」をバックエンドに保持する。
//! リモートも既存ストリームの分岐なので追加ネットワークコストはゼロ。
//!
//! 役割分担:
//! - hook = 意味論とデータ配管（主信号。archive 連携含め不変）
//! - 画面 = 表示状態の補正と付加情報。**hook が沈黙しているときだけ**補正する
//!
//! 検出パターン（稼働中17セッションの実測に基づく）:
//! - busy: スピナー行 `✢ Catapulting… (1m 53s · ↓ 4.5k tokens)`。
//!   グリフ・動詞は可変だが「列0開始の非ASCIIグリフ + `… (` + tokens/esc」は安定。
//!   引用・echo は字下げまたは `❯`/`⏺` 前置になるため列0判定で除外できる
//! - 選択待ち: `❯ 1. Yes` 形式の番号付き選択肢ブロック（permission /
//!   AskUserQuestion / plan 承認）。`❯` 単体は idle のプロンプトにも出るため
//!   「`❯ + 番号 + .` を含む 1 始まり連番ブロック（2〜6件）」で判定する
//!
//! 誤検知ゼロ優先: 判定不能・画面が小さい・hook が活動中の場合は何もしない。

use dashmap::DashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::debug_log;
use super::notify;
use super::storage;
use super::types::{InstanceId, InstanceInfo, InstanceStatus, PendingPrompt, PromptOption};

/// 評価タスクの巡回間隔。
pub(crate) const EVAL_INTERVAL: Duration = Duration::from_secs(2);
/// 補正に必要な同一シグナルの連続評価回数（約 EVAL_INTERVAL × N 秒の安定）。
const STABLE_EVALS: u8 = 2;
/// 補正を許可する hook 無音時間。これより最近 hook が届いていたら画面は口を出さない。
const HOOK_SILENCE: Duration = Duration::from_secs(5);
/// 画面検出を有効にする最小サイズ。
/// 検出限界は信号ごとに異なる（ダイアログ ~25桁・スピナー断片 ~20桁・claude UI 自体
/// が ~8行で破綻）ため、ゲートは下限近くに置く。危険な「不在ベース」の busy→idle
/// 補正はスピナー切り詰めの曖昧判定（`busy_ambiguous`）側で別途保護する。
const MIN_COLS: u16 = 30;
const MIN_ROWS: u16 = 8;
/// PendingPrompt description の最大文字数。
const MAX_DESC_CHARS: usize = 120;

// ─── シグナル抽出 ─────────────────────────────────────────────────────────────

/// 画面1枚から抽出した状態シグナル。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ScreenSignal {
    /// スピナー行（busy 表示）が確実に画面内にあるか（strict 判定）
    pub busy_line: bool,
    /// スピナー行の可能性がある行（strict のマーカー部が画面右端の切り詰めで
    /// 欠けた形）があるか。busy の主張には使わず、「不在ベース」の busy→idle
    /// 補正を保留するためだけに使う（狭い画面での誤補正防止）。
    pub busy_ambiguous: bool,
    /// 番号付き選択ダイアログ
    pub dialog: Option<ExtractedDialog>,
}

/// 画面から抽出した選択ダイアログ。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ExtractedDialog {
    /// 選択肢ブロック直前の質問行（見つからなければ空文字）
    pub description: String,
    /// 番号付き選択肢（1 始まり連番、2〜6件）
    pub options: Vec<PromptOption>,
}

/// インスタンス1つ分のシャドウスクリーン。relay がバイトを供給し、
/// グローバル評価タスクが定期的に evaluate する。
pub(crate) struct ScreenMonitor {
    parser: vt100::Parser,
    /// 最終評価以降にバイトを受信したか
    dirty: bool,
    /// 一度でもバイトを受信したか（受信前の空画面で誤判定しないため）
    seen_bytes: bool,
    last_signal: Option<ScreenSignal>,
    /// 同一シグナルの連続評価回数
    stable_count: u8,
}

impl ScreenMonitor {
    pub fn new(rows: u16, cols: u16) -> Self {
        Self {
            parser: vt100::Parser::new(rows, cols, 0),
            dirty: false,
            seen_bytes: false,
            last_signal: None,
            stable_count: 0,
        }
    }

    /// relay から毎チャンク呼ぶ。
    /// vt100 0.16.2 は特定のエスケープシーケンスで内部 `unwrap()` が None を踏んで
    /// panic することがある（観測例: SGR mouse sequence 混入時に screen.rs:870）。
    /// シャドウスクリーンは状態検出の補助であり、panic でアプリ全体が落ちる方が
    /// 害が大きいので、`catch_unwind` で吸収し parser をリセットして続行する。
    pub fn process(&mut self, bytes: &[u8]) {
        let rows = self.parser.screen().size().0;
        let cols = self.parser.screen().size().1;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.parser.process(bytes);
        }));
        if result.is_err() {
            eprintln!("[ccc] vt100 parser panicked, resetting screen monitor ({rows}x{cols})");
            self.parser = vt100::Parser::new(rows, cols, 0);
            self.last_signal = None;
            self.stable_count = 0;
        }
        self.dirty = true;
        self.seen_bytes = true;
    }

    /// PTY リサイズの転送。tmux が直後に全再描画するため自己回復する。
    pub fn resize(&mut self, rows: u16, cols: u16) {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.parser.screen_mut().set_size(rows, cols);
        }));
        if result.is_err() {
            eprintln!("[ccc] vt100 parser panicked on resize, resetting screen monitor ({rows}x{cols})");
            self.parser = vt100::Parser::new(rows, cols, 0);
        }
        // サイズ変更直後は崩れた中間状態があり得るので安定カウントをリセット
        self.last_signal = None;
        self.stable_count = 0;
    }

    /// シグナルを評価して (シグナル, 連続安定回数) を返す。
    /// バイト未受信・画面が小さい場合は None（検出無効）。
    pub fn evaluate(&mut self) -> Option<(ScreenSignal, u8)> {
        if !self.seen_bytes {
            return None;
        }
        let (rows, cols) = self.parser.screen().size();
        if cols < MIN_COLS || rows < MIN_ROWS {
            self.last_signal = None;
            self.stable_count = 0;
            return None;
        }
        let signal = if self.dirty {
            extract_signal(&self.parser.screen().contents())
        } else {
            // 変化なし = 前回シグナルが継続している
            self.last_signal.clone()?
        };
        self.dirty = false;
        if self.last_signal.as_ref() == Some(&signal) {
            self.stable_count = self.stable_count.saturating_add(1);
        } else {
            self.last_signal = Some(signal.clone());
            self.stable_count = 1;
        }
        Some((signal, self.stable_count))
    }
}

/// 画面テキスト（エスケープ解釈済み）からシグナルを抽出する。
pub(crate) fn extract_signal(contents: &str) -> ScreenSignal {
    let lines: Vec<&str> = contents.lines().collect();
    let busy_line = lines.iter().any(|l| is_busy_spinner_line(l));
    ScreenSignal {
        busy_line,
        busy_ambiguous: !busy_line && lines.iter().any(|l| is_busy_spinner_line_loose(l)),
        dialog: detect_dialog(&lines),
    }
}

/// スピナー行（busy 表示）か（strict 判定。busy の主張に使う）。
/// 実サンプル:
/// - `✢ Catapulting… (1m 53s · ↓ 4.5k tokens)`
/// - `✻ Thinking… (3s · esc to interrupt)`
/// - `✽ Clauding… (8s · thinking with high effort)` ← thinking フェーズは
///   tokens / esc を含まないため、括弧が経過時間で始まることでも判定する
///   （v0.12 初期版でこの変種を取りこぼし busy→idle を誤補正した実績あり）
///
/// - 列0開始であること（引用・会話 echo は字下げされる）
/// - 先頭1文字が非 ASCII のグリフで、構造マーカー（❯ ⏺ ⎿ 罫線）でないこと
/// - `… (` に続けて `tokens` / `esc to interrupt` / 経過時間（`8s` `1m` 等）の
///   いずれかを含むこと
fn is_busy_spinner_line(line: &str) -> bool {
    if !is_busy_spinner_line_loose(line) {
        return false;
    }
    if line.contains(" tokens") || line.contains("esc to interrupt") {
        return true;
    }
    // `… (` 直後が経過時間（数字 + s/m/h）で始まる変種
    let Some(pos) = line.find("… (") else {
        return false;
    };
    let rest = &line[pos + "… (".len()..];
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    digits > 0 && matches!(rest.as_bytes().get(digits), Some(b's' | b'm' | b'h'))
}

/// スピナー行の可能性判定（loose）。スピナー行は最長 ~65 桁あり、狭い画面では
/// 右端の切り詰めで `tokens` / `esc to interrupt` が欠け得る。グリフ + `… (` の
/// 前半構造（~20桁）だけで「busy かもしれない」とみなし、busy→idle 補正を保留する。
fn is_busy_spinner_line_loose(line: &str) -> bool {
    let Some(first) = line.chars().next() else {
        return false;
    };
    if first.is_ascii() {
        return false;
    }
    if matches!(first, '❯' | '⏺' | '⎿' | '…') || ('\u{2500}'..='\u{257F}').contains(&first)
    {
        return false;
    }
    line.contains("… (")
}

/// 行を番号付き選択肢として解釈する。`(選択中か, 番号, ラベル)`。
/// 例: `❯ 1. Yes` / `  2. Yes, and don't ask again` / `│ ❯ 1. Yes │`（box 内）
fn parse_numbered_option(line: &str) -> Option<(bool, u32, String)> {
    let t = line.trim_start().trim_start_matches('│').trim_start();
    let (selected, rest) = match t.strip_prefix('❯') {
        Some(r) => (true, r.trim_start()),
        None => (false, t),
    };
    let dot = rest.find('.')?;
    // 番号は高々2桁（"3.5 seconds" のような小数の文章を弾く意図もある）
    if dot == 0 || dot > 2 || !rest[..dot].bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let num: u32 = rest[..dot].parse().ok()?;
    let label = rest[dot + 1..].trim_end_matches('│').trim();
    if label.is_empty() {
        return None;
    }
    Some((selected, num, label.to_string()))
}

/// 番号付き選択ダイアログの検出。
/// `❯ N.` 行を含む連続した選択肢ブロックで、番号が 1 始まりの連番（2〜6件）の
/// 場合のみ採用する。description はブロック直前 6 行以内の最初の非空行。
fn detect_dialog(lines: &[&str]) -> Option<ExtractedDialog> {
    let selected_idx = lines
        .iter()
        .position(|l| matches!(parse_numbered_option(l), Some((true, _, _))))?;

    let mut start = selected_idx;
    while start > 0 && parse_numbered_option(lines[start - 1]).is_some() {
        start -= 1;
    }
    let mut end = selected_idx;
    while end + 1 < lines.len() && parse_numbered_option(lines[end + 1]).is_some() {
        end += 1;
    }

    let parsed: Vec<(bool, u32, String)> = (start..=end)
        .filter_map(|i| parse_numbered_option(lines[i]))
        .collect();
    if !(2..=6).contains(&parsed.len()) {
        return None;
    }
    // 1 始まりの連番でなければ選択 UI とはみなさない（本文中の番号リスト対策）
    if !parsed
        .iter()
        .enumerate()
        .all(|(k, (_, n, _))| *n == k as u32 + 1)
    {
        return None;
    }

    let description = lines[..start]
        .iter()
        .rev()
        .take(6)
        .map(|l| l.trim_matches(|c: char| c == '│' || c.is_whitespace()))
        .find(|l| !l.is_empty())
        .map(|l| truncate_chars(l, MAX_DESC_CHARS))
        .unwrap_or_default();

    Some(ExtractedDialog {
        description,
        options: parsed
            .into_iter()
            .map(|(_, n, label)| PromptOption {
                key: n.to_string(),
                label,
            })
            .collect(),
    })
}

fn truncate_chars(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((end, _)) => format!("{}…", &s[..end]),
        None => s.to_string(),
    }
}

// ─── 補正（fusion） ───────────────────────────────────────────────────────────

/// 補正に必要なハンドル一式。`InstanceManager` の Arc フィールドから組み立てる。
pub(crate) struct FusionCtx {
    pub infos: Arc<DashMap<InstanceId, InstanceInfo>>,
    pub app_handle: Option<tauri::AppHandle>,
    /// hook 受信時刻（apply_hook が更新）。未登録 = hook が一度も来ていない = 沈黙扱い
    pub last_hook_at: Arc<DashMap<InstanceId, Instant>>,
    /// 画面補正が設定した status の控え。hook 適用時にクリアされる。
    /// 「画面補正で waiting にした状態」だけはダイアログ消失で idle に戻せる
    /// （スクロールで過去のダイアログが映った誤検知の自己回復用）。
    pub screen_set: Arc<DashMap<InstanceId, InstanceStatus>>,
}

/// 補正内容。`message` / `prompt` が None のときは既存値を保持する。
#[derive(Debug, PartialEq)]
struct Correction {
    status: InstanceStatus,
    message: Option<String>,
    prompt: Option<Option<PendingPrompt>>,
}

/// 1インスタンス分のシグナルを状態に反映する。
pub(crate) fn apply_correction(ctx: &FusionCtx, id: &str, signal: &ScreenSignal, stable: u8) {
    if stable < STABLE_EVALS {
        return;
    }
    let (current, log_path) = {
        let Some(info) = ctx.infos.get(id) else {
            return;
        };
        (info.status.clone(), info.instance_dir.join(".debug.txt"))
    };
    let screen_originated = ctx
        .screen_set
        .get(id)
        .map(|s| *s == current)
        .unwrap_or(false);
    let Some(correction) = decide(&current, signal, screen_originated) else {
        return;
    };

    // hook が活動中なら画面は口を出さない。ただし不一致はチューニング用に
    // 1 エピソード 1 回（stable がしきい値に達した瞬間）だけログする。
    let hook_active = ctx
        .last_hook_at
        .get(id)
        .is_some_and(|t| t.elapsed() < HOOK_SILENCE);
    if hook_active {
        if stable == STABLE_EVALS {
            debug_log::append(
                Some(&log_path),
                &format!(
                    "[screen] 不一致検出（hook 活動中のため補正せず）: {current:?} → {:?} (busy_line={}, dialog={})",
                    correction.status, signal.busy_line, signal.dialog.is_some()
                ),
            );
        }
        return;
    }

    {
        let Some(mut info) = ctx.infos.get_mut(id) else {
            return;
        };
        // ロック取り直しの間に状態が変わっていたら何もしない
        if info.status != current {
            return;
        }
        info.status = correction.status.clone();
        if let Some(msg) = correction.message.clone() {
            info.status_message = Some(msg);
        }
        if let Some(prompt) = correction.prompt.clone() {
            info.pending_prompt = prompt;
        }
        let _ = storage::save_connection(&info);
        notify::emit_status_changed(&ctx.app_handle, &info);
    }
    ctx.screen_set
        .insert(id.to_string(), correction.status.clone());
    let line = format!(
        "[screen] 画面検出による補正: {current:?} → {:?} (busy_line={}, dialog={})",
        correction.status,
        signal.busy_line,
        signal.dialog.is_some()
    );
    eprintln!("[ccc] {id}: {line}");
    debug_log::append(Some(&log_path), &line);
}

/// 補正規則本体（pure・テスト対象）。None = 補正不要。
fn decide(
    current: &InstanceStatus,
    signal: &ScreenSignal,
    screen_originated: bool,
) -> Option<Correction> {
    use InstanceStatus::*;
    match current {
        // 接続系・終了状態は画面の管轄外
        Connecting | Disconnected | Terminated => None,

        AgentBusy => {
            if let Some(dialog) = &signal.dialog {
                // PermissionRequest hook の欠落: ダイアログが出ているのに busy のまま
                Some(waiting_correction(dialog))
            } else if !signal.busy_line && !signal.busy_ambiguous {
                // Stop hook の欠落・Esc 中断: スピナーが消えているのに busy のまま。
                // 切り詰められたスピナー候補（busy_ambiguous）がある場合は保留。
                // status_message は直前の narration を保持する
                Some(Correction {
                    status: AgentIdle,
                    message: None,
                    prompt: Some(None),
                })
            } else {
                None
            }
        }

        AgentIdle | Running => {
            if let Some(dialog) = &signal.dialog {
                Some(waiting_correction(dialog))
            } else if signal.busy_line {
                // UserPromptSubmit hook の欠落・tmux 直接操作: 動いているのに idle 表示
                Some(Correction {
                    status: AgentBusy,
                    message: None,
                    prompt: Some(None),
                })
            } else {
                None
            }
        }

        AgentWaitingInput => {
            if signal.busy_line && signal.dialog.is_none() {
                // 選択が解決されて作業再開（PreToolUse 欠落時の補正）
                Some(Correction {
                    status: AgentBusy,
                    message: None,
                    prompt: Some(None),
                })
            } else if signal.dialog.is_none()
                && !signal.busy_line
                && !signal.busy_ambiguous
                && screen_originated
            {
                // 画面補正で waiting にしたが根拠のダイアログが消えた
                // （スクロール由来の誤検知）→ idle に自己回復。
                // hook 由来の waiting はスクロールでダイアログが画面外でも維持する
                Some(Correction {
                    status: AgentIdle,
                    message: None,
                    prompt: Some(None),
                })
            } else {
                None
            }
        }
    }
}

fn waiting_correction(dialog: &ExtractedDialog) -> Correction {
    let message = if dialog.description.is_empty() {
        "選択待ち".to_string()
    } else {
        dialog.description.clone()
    };
    Correction {
        status: InstanceStatus::AgentWaitingInput,
        message: Some(message),
        prompt: Some(Some(PendingPrompt::Permission {
            description: dialog.description.clone(),
            options: dialog.options.clone(),
        })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── is_busy_spinner_line ─────────────────────────────────────────────

    #[test]
    fn busy_line_detects_real_samples() {
        // 稼働セッションからの実サンプル
        assert!(is_busy_spinner_line(
            "✢ Catapulting… (1m 53s · ↓ 4.5k tokens)"
        ));
        assert!(is_busy_spinner_line(
            "✻ Bloviating… (2m 22s · ↓ 6.0k tokens)"
        ));
        assert!(is_busy_spinner_line("✶ Thinking… (3s · esc to interrupt)"));
    }

    #[test]
    fn busy_line_detects_thinking_phase_variant() {
        // thinking フェーズの実サンプル（tokens / esc を含まない。
        // 取りこぼして busy→idle を誤補正した実績のある変種）
        assert!(is_busy_spinner_line(
            "✽ Clauding… (8s · thinking with high effort)"
        ));
        // 経過時間が分単位のケース
        assert!(is_busy_spinner_line("✻ Pondering… (1m 12s · thinking)"));
    }

    #[test]
    fn busy_line_rejects_paren_without_time_or_marker() {
        // 括弧はあるが時間でもマーカーでもない（日本語の補足など）
        assert!(!is_busy_spinner_line("✦ 補足… (参考情報)"));
        // loose には引っかかる（曖昧扱いで補正保留になる）
        assert!(is_busy_spinner_line_loose("✦ 補足… (参考情報)"));
    }

    #[test]
    fn busy_line_rejects_indented_quotes() {
        // 会話内の引用・echo は字下げされる
        assert!(!is_busy_spinner_line(
            "  ✢ Catapulting… (1m 53s · ↓ 4.5k tokens)"
        ));
    }

    #[test]
    fn busy_line_rejects_structural_markers() {
        assert!(!is_busy_spinner_line(
            "❯ ✢ Catapulting… (1m 53s · ↓ 4.5k tokens)"
        ));
        assert!(!is_busy_spinner_line("⏺ 完了… (参考 tokens)"));
        assert!(!is_busy_spinner_line("│ ✻ x… (1s · ↓ 1k tokens) │"));
        assert!(!is_busy_spinner_line("─────… ( tokens)"));
    }

    #[test]
    fn busy_line_rejects_ascii_and_plain_text() {
        assert!(!is_busy_spinner_line("Loading… (3s · ↓ 1k tokens)"));
        assert!(!is_busy_spinner_line("✢ 進捗はこちら（参照）"));
        assert!(!is_busy_spinner_line(""));
    }

    // ── detect_dialog ────────────────────────────────────────────────────

    fn lines(s: &str) -> Vec<&str> {
        s.lines().collect()
    }

    #[test]
    fn dialog_detects_permission_prompt() {
        let screen = "\
⏺ Bash(rm -rf build/)

 Do you want to proceed?
 ❯ 1. Yes
   2. Yes, and don't ask again for rm commands
   3. No, and tell Claude what to do differently (esc)
";
        let d = detect_dialog(&lines(screen)).expect("dialog");
        assert_eq!(d.description, "Do you want to proceed?");
        assert_eq!(d.options.len(), 3);
        assert_eq!(d.options[0].key, "1");
        assert_eq!(d.options[0].label, "Yes");
        assert_eq!(
            d.options[2].label,
            "No, and tell Claude what to do differently (esc)"
        );
    }

    #[test]
    fn dialog_detects_boxed_options() {
        let screen = "\
│ どのアプローチにしますか？ │
│ ❯ 1. 方式A               │
│   2. 方式B               │
";
        let d = detect_dialog(&lines(screen)).expect("dialog");
        assert_eq!(d.description, "どのアプローチにしますか？");
        assert_eq!(d.options.len(), 2);
        assert_eq!(d.options[1].label, "方式B");
    }

    #[test]
    fn dialog_rejects_idle_prompt_and_history() {
        // idle 画面に出る ❯（入力プロンプト・履歴 echo・スラッシュコマンド）
        let screen = "\
❯ at 03:31:33 ❯ aws --region us-west-2 ec2 describe-instances
❯ /model
❯
";
        assert!(detect_dialog(&lines(screen)).is_none());
    }

    #[test]
    fn dialog_rejects_numbered_list_without_cursor() {
        // 本文中の番号リスト（❯ 無し）は選択 UI ではない
        let screen = "\
 手順:
 1. ビルドする
 2. テストする
";
        assert!(detect_dialog(&lines(screen)).is_none());
    }

    #[test]
    fn dialog_rejects_non_sequential_numbers() {
        let screen = "\
 ❯ 2. これは連番でない
   5. ので選択UIではない
";
        assert!(detect_dialog(&lines(screen)).is_none());
    }

    #[test]
    fn dialog_rejects_single_option() {
        let screen = " ❯ 1. ひとつだけ\n";
        assert!(detect_dialog(&lines(screen)).is_none());
    }

    // ── 切り詰めスピナーの曖昧判定 ───────────────────────────────────────

    #[test]
    fn truncated_spinner_marks_ambiguous_not_busy() {
        // 極端に狭い画面で経過時間ごと切り詰められたスピナー行
        let s = extract_signal("✻ Hyperventilating… (1");
        assert!(!s.busy_line);
        assert!(s.busy_ambiguous);
    }

    #[test]
    fn truncated_spinner_with_time_prefix_is_still_busy() {
        // 後半が切れていても経過時間まで見えていれば strict（busy 確定）
        let s = extract_signal("✻ Hyperventilating… (12m 45s · ↓ 12");
        assert!(s.busy_line);
        assert!(!s.busy_ambiguous);
    }

    #[test]
    fn full_spinner_is_busy_not_ambiguous() {
        let s = extract_signal("✢ Catapulting… (1m 53s · ↓ 4.5k tokens)");
        assert!(s.busy_line);
        assert!(!s.busy_ambiguous);
    }

    #[test]
    fn loose_pattern_rejects_indented_and_structural() {
        // loose 判定も列0・構造マーカー除外は strict と共通
        let s = extract_signal("  ✻ 引用… (3s\n⏺ 完了… (参考\n│ ✻ x… (1s │");
        assert!(!s.busy_line);
        assert!(!s.busy_ambiguous);
    }

    #[test]
    fn decide_busy_with_ambiguous_spinner_holds() {
        // 切り詰めの可能性がある間は busy→idle 補正を保留する
        let signal = ScreenSignal {
            busy_line: false,
            busy_ambiguous: true,
            dialog: None,
        };
        assert!(decide(&InstanceStatus::AgentBusy, &signal, false).is_none());
        // 画面補正由来 waiting の自己回復も保留
        assert!(decide(&InstanceStatus::AgentWaitingInput, &signal, true).is_none());
    }

    // ── ScreenMonitor（vt100 経由） ──────────────────────────────────────

    #[test]
    fn monitor_detects_busy_through_vt100() {
        let mut m = ScreenMonitor::new(24, 80);
        m.process(b"\x1b[2J\x1b[H\xe2\x9c\xa2 Working\xe2\x80\xa6 (3s \xc2\xb7 \xe2\x86\x93 1k tokens)\r\n");
        let (signal, stable) = m.evaluate().expect("signal");
        assert!(signal.busy_line);
        assert!(signal.dialog.is_none());
        assert_eq!(stable, 1);
        // 変化が無ければ同一シグナルで安定カウントが伸びる
        let (signal2, stable2) = m.evaluate().expect("signal");
        assert_eq!(signal, signal2);
        assert_eq!(stable2, 2);
    }

    #[test]
    fn monitor_disabled_below_min_size() {
        // 行数不足（8 未満）
        let mut m = ScreenMonitor::new(7, 80);
        m.process(b"\xe2\x9c\xa2 Working\xe2\x80\xa6 (3s \xc2\xb7 \xe2\x86\x93 1k tokens)");
        assert!(m.evaluate().is_none());
        // 桁数不足（30 未満）
        let mut m = ScreenMonitor::new(24, 25);
        m.process(b"\xe2\x9c\xa2 W\xe2\x80\xa6 (3s)");
        assert!(m.evaluate().is_none());
    }

    #[test]
    fn monitor_silent_before_first_bytes() {
        let mut m = ScreenMonitor::new(24, 80);
        assert!(m.evaluate().is_none());
    }

    #[test]
    fn monitor_resets_stability_on_resize() {
        let mut m = ScreenMonitor::new(24, 80);
        m.process(b"\xe2\x9c\xa2 W\xe2\x80\xa6 (3s \xc2\xb7 \xe2\x86\x93 1k tokens)");
        let _ = m.evaluate();
        m.resize(50, 120);
        m.process(b"x");
        let (_, stable) = m.evaluate().expect("signal");
        assert_eq!(stable, 1);
    }

    // ── decide ───────────────────────────────────────────────────────────

    fn sig(busy: bool, dialog: bool) -> ScreenSignal {
        ScreenSignal {
            busy_line: busy,
            busy_ambiguous: false,
            dialog: dialog.then(|| ExtractedDialog {
                description: "Do you want to proceed?".into(),
                options: vec![
                    PromptOption {
                        key: "1".into(),
                        label: "Yes".into(),
                    },
                    PromptOption {
                        key: "2".into(),
                        label: "No".into(),
                    },
                ],
            }),
        }
    }

    #[test]
    fn decide_busy_without_spinner_goes_idle() {
        let c = decide(&InstanceStatus::AgentBusy, &sig(false, false), false).unwrap();
        assert_eq!(c.status, InstanceStatus::AgentIdle);
        assert_eq!(c.message, None, "narration は保持する");
    }

    #[test]
    fn decide_busy_with_spinner_no_change() {
        assert!(decide(&InstanceStatus::AgentBusy, &sig(true, false), false).is_none());
    }

    #[test]
    fn decide_idle_with_spinner_goes_busy() {
        let c = decide(&InstanceStatus::AgentIdle, &sig(true, false), false).unwrap();
        assert_eq!(c.status, InstanceStatus::AgentBusy);
        let c = decide(&InstanceStatus::Running, &sig(true, false), false).unwrap();
        assert_eq!(c.status, InstanceStatus::AgentBusy);
    }

    #[test]
    fn decide_dialog_goes_waiting_with_real_options() {
        let c = decide(&InstanceStatus::AgentBusy, &sig(false, true), false).unwrap();
        assert_eq!(c.status, InstanceStatus::AgentWaitingInput);
        match c.prompt {
            Some(Some(PendingPrompt::Permission { options, .. })) => {
                assert_eq!(options.len(), 2);
                assert_eq!(options[0].label, "Yes");
            }
            other => panic!("Permission を期待: {other:?}"),
        }
    }

    #[test]
    fn decide_waiting_with_spinner_goes_busy() {
        let c = decide(&InstanceStatus::AgentWaitingInput, &sig(true, false), false).unwrap();
        assert_eq!(c.status, InstanceStatus::AgentBusy);
    }

    #[test]
    fn decide_screen_set_waiting_reverts_when_dialog_gone() {
        // 画面補正由来の waiting はダイアログ消失で idle に自己回復
        let c = decide(&InstanceStatus::AgentWaitingInput, &sig(false, false), true).unwrap();
        assert_eq!(c.status, InstanceStatus::AgentIdle);
        // hook 由来の waiting は維持（スクロールで画面外でも信用する）
        assert!(decide(
            &InstanceStatus::AgentWaitingInput,
            &sig(false, false),
            false
        )
        .is_none());
    }

    #[test]
    fn decide_connection_states_untouched() {
        for st in [
            InstanceStatus::Connecting,
            InstanceStatus::Disconnected,
            InstanceStatus::Terminated,
        ] {
            assert!(decide(&st, &sig(true, true), false).is_none());
            assert!(decide(&st, &sig(false, false), false).is_none());
        }
    }
}
