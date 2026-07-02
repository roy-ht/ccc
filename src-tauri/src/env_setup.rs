//! ccc プロセスの実行環境セットアップ。
//!
//! Tauri アプリを Finder / Dock から起動すると、子プロセスの `PATH` は
//! `/usr/bin:/bin:/usr/sbin:/sbin` 程度しか引き継がれない。これだと
//! `Command::new("tmux")` などが Homebrew/mise/asdf 配下のバイナリを
//! 解決できず、ローカル tmux セッションの存在確認 (`has-session`) が
//! 偽陰性で false を返してインスタンスが Terminated 扱いになる事故に繋がる。
//!
//! 起動直後に login shell を一回走らせて `$PATH` を取り込み、
//! 以降の `Command::new(...)` がユーザのシェル設定と同じ PATH で動くようにする。
//!
//! login + interactive で起動する理由: `.zshrc` 等で Homebrew / mise / asdf の
//! `shellenv` を `[[ -o interactive ]]` ガード下に置いている設定が一般的で、
//! `-l` だけだとそれらが評価されず Homebrew パスが抜ける。`-i` を付けると
//! interactive 扱いになり、shellenv が走って正しい PATH が得られる。
//! 副作用 (プロンプト等のシーケンスが stderr に出る) は捨てる。

use std::process::Command;

/// login + interactive shell の `$PATH` を取得し、本プロセスの `PATH` を上書きする。
///
/// 失敗してもアプリ起動は止めず、警告のみ出して既存の `PATH` を維持する。
pub fn inherit_login_shell_path() {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    let output = match Command::new(&shell)
        .args(["-l", "-i", "-c", "printf %s \"$PATH\""])
        .output()
    {
        Ok(o) => o,
        Err(e) => {
            eprintln!("[ccc] login shell の起動に失敗 (PATH 継承スキップ): {e}");
            return;
        }
    };
    if !output.status.success() {
        eprintln!(
            "[ccc] login shell の PATH 取得に失敗 (exit={}, stderr={:?})",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
        return;
    }
    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if path.is_empty() {
        eprintln!("[ccc] login shell が空の PATH を返したため継承をスキップ");
        return;
    }
    std::env::set_var("PATH", &path);
    eprintln!("[ccc] PATH を login shell から継承: {path}");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `inherit_login_shell_path` を実行しても panic しないこと、
    /// および `PATH` が空にならないこと（最低限の健全性チェック）。
    /// CI 環境差を吸収するため、具体的な内容には依存しない。
    #[test]
    fn inherit_does_not_panic_and_keeps_path_nonempty() {
        let before = std::env::var("PATH").unwrap_or_default();
        inherit_login_shell_path();
        let after = std::env::var("PATH").unwrap_or_default();
        assert!(
            !after.is_empty(),
            "PATH が空になってはいけない (before={before:?})"
        );
    }
}
