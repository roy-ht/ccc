//! リモートへの 1 ファイル配信（rsync over ssh）。
//!
//! scp は使わない。OpenSSH 9 以降の scp は既定で SFTP プロトコルを使うため、
//! リモートに sftp-server が無いホスト（最小構成のコンテナ等）では
//! `sftp-server: No such file or directory` で失敗する。rsync は ssh 越しに
//! リモートの rsync を起動するだけなので sftp-server に依存せず、
//! `agent_settings/` の同期で既に必須となっているため追加の前提条件も無い。

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use crate::exec::{run_with_timeout, ExecOutcome};

/// ローカルの `local` をリモートの `~/<remote_rel>` へ送る。
///
/// `remote_rel` はホーム相対パス（rsync のリモートパスはホーム起点で解釈される）。
/// パーミッションは `-p` でローカルのものを引き継ぐが、確実を期すなら呼び出し側で
/// 後から `chmod` すること。
pub fn upload_file(
    host_alias: &str,
    local: &Path,
    remote_rel: &str,
    timeout: Duration,
) -> std::io::Result<ExecOutcome> {
    run_with_timeout(
        Command::new("rsync")
            .args(["-q", "-p", "-e", "ssh -o BatchMode=yes"])
            .arg(local)
            .arg(format!("{host_alias}:{remote_rel}")),
        timeout,
    )
}
