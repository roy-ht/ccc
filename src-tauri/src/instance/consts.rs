/// broadcast channel のバッファサイズ
pub const BROADCAST_CHANNEL_SIZE: usize = 256;

/// mpsc output channel のバッファサイズ
pub const OUTPUT_CHANNEL_SIZE: usize = 256;

/// PTY デフォルト行数
pub const DEFAULT_PTY_ROWS: u16 = 24;

/// PTY デフォルト列数
pub const DEFAULT_PTY_COLS: u16 = 80;

/// ccc 専用 tmux コマンドプレフィックス。
/// -f /dev/null: ユーザーの tmux.conf を読み込まない。
/// -L ccc: 専用ソケットでユーザーの tmux と分離。
pub const TMUX: &str = "tmux -f /dev/null -L ccc";

/// tmux セッション作成後に適用するオプション群。
///
/// `\;` でtmux自身のコマンドセパレータとして渡す（シェルの `;` だと2つ目以降が
/// シェルビルトイン `set` として解釈されて no-op になるため）。Rust文字列では
/// `\\;` と書き、シェル経由でtmuxに `;` として届く。
///
/// `terminal-overrides ',*:smcup@:rmcup@'` で outer terminal (tmux client が xterm.js
/// に送る出力) の smcup/rmcup capability を無効化する。これにより tmux client が
/// `\x1b[?1049h` (alternate screen 切替) を送らなくなり、attach 時の描画が xterm.js の
/// normal buffer に直接行われる。
/// pane 内のアプリは default-terminal (screen-256color) の terminfo を見るので、
/// pane 内 TUI の alternate screen 利用には影響しない。
///
/// `mouse on` でホイール / クリック等のマウスイベントを tmux client が受け、
/// alternate_screen を使う pane (claude code 等) ではアプリに渡し、そうでない pane
/// (shell プロンプト) では copy-mode に入って scrollback navigation する。
/// xterm.js 側の scrollback は 0 にしてスクロールバーを廃しており、履歴ナビゲーションは
/// tmux 側に一元化している。
///
/// `bind -n WheelUpPane/WheelDownPane` を明示的に上書きするのは、tmux 2.x 系の
/// 古いデフォルト bind が末尾に `send-keys -M` を持っており、shell プロンプトで
/// wheel-up が `\x1bOA` (上矢印) として shell に届いて readline の previous-history を
/// 発火してしまう症状を回避するため。tmux 3.x のデフォルトを陽に書き直し、shell pane では
/// 純粋に copy-mode に入る挙動だけにする。
/// テキスト選択 → コピーは tmux 側で行われ、`set-clipboard on` +
/// `terminal-features ',*:clipboard'` の OSC 52 経由でシステムクリップボードに届く。
///
/// `set-clipboard on` + `terminal-features ',*:clipboard'` で、pane 内アプリ (claude code
/// など) が発行する OSC 52 を tmux が xterm.js (client_tty) まで転送するように宣言する。
/// xterm.js 側で OSC 52 を受けて `navigator.clipboard` に書き込むハンドラを
/// `TerminalPanel.tsx` に置いている。これにより、リモートインスタンスで pbcopy 等を
/// 持たない環境でも、選択コピーがローカルのシステムクリップボードまで届く。
///
/// `terminal-features ',*:RGB'` で outer terminal (xterm.js) が 24-bit color を
/// サポートする旨を tmux に伝え、pane 内アプリが発行する `\x1b[38;2;..m` /
/// `\x1b[48;2;..m` (truecolor 前景/背景) を素通しさせる。これがないと tmux は
/// pane 内 TERM=screen-256color の terminfo を見て truecolor を 256色 (または無効)
/// にフォールバックし、claude code の diff の追加行 (緑背景) などが描画されなくなる。
pub const TMUX_SET_OPTS_SUFFIX: &str = "\
set -g status off \\; \
set -g prefix None \\; \
set -g prefix2 None \\; \
set -g mouse on \\; \
set -g escape-time 0 \\; \
set -g set-clipboard on \\; \
set -ga terminal-features ',*:clipboard:RGB' \\; \
set -ga terminal-overrides ',*:smcup@:rmcup@' \\; \
bind -n WheelUpPane if -F '#{?pane_in_mode,1,#{alternate_on}}' 'send-keys -M' 'copy-mode -e' \\; \
bind -n WheelDownPane if -F '#{?pane_in_mode,1,#{alternate_on}}' 'send-keys -M' 'send-keys -M'";

/// セッション作成後に仕込む「コマンド死活検知」コマンドを組み立てる。
///
/// - `remain-on-exit on`: ペイン内コマンド（`exec claude` 等）が終了しても
///   ペインを dead 状態で残し、セッションと最終出力を保全する。
///   これが無いとコマンド終了 = セッション消滅で、エラー出力ごと失われて
///   再接続時の「can't find session」になるまで気づけない。
/// - `pane-died` フック: dead 化した瞬間に `ccc-hook.sh --pane-died` を実行し、
///   exit status 付きの PaneDied イベントを ccc に通知する。
///
/// グローバル (`set -g`) ではなくセッション標的 (`-t`) で設定するのは、
/// 同一 tmux サーバーを共有する別バージョンの ccc が作ったセッションの
/// 挙動を変えないため。古い tmux で set-hook が失敗しても rc≠0 の警告
/// ログが出るだけで接続処理は続行される（start_pty の仕様）。
///
/// クォート構造（3層）: シェル single-quote → tmux double-quote（`\"`）→
/// run-shell が `#{pane_dead_status}` を format 展開して `/bin/sh -c` で実行。
pub fn tmux_death_watch_cmd(tmux_session: &str, instance_id: &str) -> String {
    format!(
        " {TMUX} set-option -w -t '{tmux_session}' remain-on-exit on \\; \
         set-hook -t '{tmux_session}' pane-died \
         'run-shell \"\\\"$HOME/.ccc/bin/ccc-hook.sh\\\" --pane-died \\\"#{{pane_dead_status}}\\\" {instance_id}\"'; echo __CCC__$?\n"
    )
}

/// shell window 名（agent_session 内に追加する補助 window のラベル）。
/// Terminal タブはこの名前で識別する。
pub const SHELL_WINDOW_NAME: &str = "shell";

/// `<agent_session>` から Terminal タブ用の session-group メンバー名を組み立てる。
/// 例: `ccc-abc123` → `ccc-abc123-shell`。`tmux new-session -d -t <agent> -s <group>`
/// で同一 window 集合を共有する別 session を作るのに使う。
pub fn shell_group_session_name(agent_session: &str) -> String {
    format!("{agent_session}-{SHELL_WINDOW_NAME}")
}

/// Terminal タブ用に `<agent_session>:shell` window を作るコマンド。
/// 既に同名 window があると失敗するが、`start_pty` は rc≠0 を警告だけで続行するため
/// 冪等性は既存 setup_cmds と同じく "失敗は無視ログ" で吸収する。
/// `directory` が空文字列なら `-c` を省略し、リモートの $HOME など tmux 側のデフォルトに任せる。
pub fn tmux_new_shell_window_cmd(agent_session: &str, directory: &str) -> String {
    let dir_flag = if directory.is_empty() {
        String::new()
    } else {
        let escaped_dir = directory.replace('\'', "'\\''");
        format!(" -c '{escaped_dir}'")
    };
    format!(
        " {TMUX} new-window -t '{agent_session}' -n '{SHELL_WINDOW_NAME}' -d{dir_flag} -- \"$SHELL\" -l; echo __CCC__$?\n",
    )
}

/// agent_session と同じ window 集合を共有する session-group メンバーを作り、
/// shell window を current にしておく。
/// 既に group session が存在する場合も `start_pty` の rc 許容で続行する。
pub fn tmux_new_shell_group_cmd(agent_session: &str, group_session: &str) -> String {
    format!(
        " {TMUX} new-session -d -t '{agent_session}' -s '{group_session}' \\; \
         select-window -t '{group_session}:{SHELL_WINDOW_NAME}'; echo __CCC__$?\n",
    )
}

/// ステップ完了マーカーのプレフィックス。
/// シェルに "; echo __CCC__$?" を付けて送り、出力でこのパターンを探す。
pub const MARKER_PREFIX: &[u8] = b"__CCC__";

/// 画面クリア系のエスケープシーケンスを探す。
/// \x1b[2J (Erase Display) または \x1b[?1049h (Alternate Screen Buffer) を検知。
/// 見つかった場合、シーケンス終端の次の位置を返す。
pub fn find_clear_sequence(data: &[u8]) -> Option<usize> {
    let patterns: &[&[u8]] = &[b"\x1b[2J", b"\x1b[?1049h"];
    patterns
        .iter()
        .filter_map(|pattern| {
            data.windows(pattern.len())
                .position(|w| w == *pattern)
                .map(|pos| pos + pattern.len())
        })
        .min()
}

/// "__CCC__" マーカーを探し、直後の終了コード文字を返す。
/// 例: "__CCC__0" → Some('0'), "__CCC__1" → Some('1')
/// シェルのコマンドエコー内の "__CCC__$?" に誤マッチしないよう、
/// 直後が ASCII 数字の場合のみマッチする。
pub fn find_marker(data: &[u8]) -> Option<char> {
    let prefix = MARKER_PREFIX;
    data.windows(prefix.len() + 1)
        .find(|w| w.starts_with(prefix) && w[prefix.len()].is_ascii_digit())
        .map(|w| w[prefix.len()] as char)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── tmux_death_watch_cmd ────────────────────────────────────────────

    /// 3層クォート（シェル → tmux → sh）の出力を固定する。
    /// この文字列が変わるときはリモートでの手動検証を必ずやり直すこと。
    #[test]
    fn test_death_watch_cmd_quoting() {
        let cmd = tmux_death_watch_cmd("ccc-abc123", "uuid-1");
        assert_eq!(
            cmd,
            " tmux -f /dev/null -L ccc set-option -w -t 'ccc-abc123' remain-on-exit on \\; \
             set-hook -t 'ccc-abc123' pane-died \
             'run-shell \"\\\"$HOME/.ccc/bin/ccc-hook.sh\\\" --pane-died \\\"#{pane_dead_status}\\\" uuid-1\"'; echo __CCC__$?\n"
        );
    }

    // ── shell window / group session ────────────────────────────────────

    #[test]
    fn test_shell_group_session_name() {
        assert_eq!(shell_group_session_name("ccc-abc123"), "ccc-abc123-shell");
    }

    #[test]
    fn test_new_shell_window_cmd_quoting() {
        let cmd = tmux_new_shell_window_cmd("ccc-abc123", "/Users/me/work");
        assert_eq!(
            cmd,
            " tmux -f /dev/null -L ccc new-window -t 'ccc-abc123' -n 'shell' -d -c '/Users/me/work' -- \"$SHELL\" -l; echo __CCC__$?\n"
        );
    }

    #[test]
    fn test_new_shell_window_cmd_escapes_single_quote() {
        let cmd = tmux_new_shell_window_cmd("ccc-abc123", "/home/o'malley/work");
        assert!(cmd.contains(r"'/home/o'\''malley/work'"));
    }

    #[test]
    fn test_new_shell_window_cmd_empty_directory_drops_c_flag() {
        // 空文字列のときは `-c` をつけず、tmux 側のデフォルト cwd に任せる。
        let cmd = tmux_new_shell_window_cmd("ccc-abc123", "");
        assert_eq!(
            cmd,
            " tmux -f /dev/null -L ccc new-window -t 'ccc-abc123' -n 'shell' -d -- \"$SHELL\" -l; echo __CCC__$?\n"
        );
        assert!(!cmd.contains(" -c "));
    }

    #[test]
    fn test_new_shell_group_cmd_quoting() {
        let cmd = tmux_new_shell_group_cmd("ccc-abc123", "ccc-abc123-shell");
        assert_eq!(
            cmd,
            " tmux -f /dev/null -L ccc new-session -d -t 'ccc-abc123' -s 'ccc-abc123-shell' \\; \
             select-window -t 'ccc-abc123-shell:shell'; echo __CCC__$?\n"
        );
    }

    // ── find_clear_sequence ─────────────────────────────────────────────

    #[test]
    fn test_find_clear_erase_display() {
        let data = b"some text\x1b[2Jmore text";
        assert_eq!(find_clear_sequence(data), Some(13)); // "\x1b[2J" の直後
    }

    #[test]
    fn test_find_clear_alternate_screen() {
        let data = b"some text\x1b[?1049hmore text";
        assert_eq!(find_clear_sequence(data), Some(17)); // "\x1b[?1049h" の直後
    }

    #[test]
    fn test_find_clear_both_returns_earliest() {
        // 両方のパターンがある場合、最も早い位置を返す
        let data = b"\x1b[?1049h\x1b[2J";
        assert_eq!(find_clear_sequence(data), Some(8)); // alternate screen が先
    }

    #[test]
    fn test_find_clear_not_found() {
        let data = b"normal terminal output\x1b[32mgreen\x1b[0m";
        assert_eq!(find_clear_sequence(data), None);
    }

    #[test]
    fn test_find_clear_empty_input() {
        assert_eq!(find_clear_sequence(b""), None);
    }

    #[test]
    fn test_find_clear_partial_sequence() {
        // 不完全なシーケンスにはマッチしない
        assert_eq!(find_clear_sequence(b"\x1b[2"), None);
        assert_eq!(find_clear_sequence(b"\x1b[?1049"), None);
    }

    // ── find_marker ─────────────────────────────────────────────────────

    #[test]
    fn test_find_marker_success_zero() {
        let data = b"some output__CCC__0\n";
        assert_eq!(find_marker(data), Some('0'));
    }

    #[test]
    fn test_find_marker_success_nonzero() {
        let data = b"some output__CCC__1\n";
        assert_eq!(find_marker(data), Some('1'));
    }

    #[test]
    fn test_find_marker_ignores_dollar_question() {
        // シェルエコー "__CCC__$?" にはマッチしない（$はASCII数字でない）
        let data = b"echo __CCC__$?\n";
        assert_eq!(find_marker(data), None);
    }

    #[test]
    fn test_find_marker_not_found() {
        let data = b"normal output without marker";
        assert_eq!(find_marker(data), None);
    }

    #[test]
    fn test_find_marker_empty_input() {
        assert_eq!(find_marker(b""), None);
    }

    #[test]
    fn test_find_marker_prefix_only_no_digit() {
        // プレフィックスだけで直後にデータがない場合
        assert_eq!(find_marker(b"__CCC__"), None);
    }

    #[test]
    fn test_find_marker_multiple_returns_first() {
        let data = b"__CCC__0 something __CCC__1";
        assert_eq!(find_marker(data), Some('0'));
    }
}
