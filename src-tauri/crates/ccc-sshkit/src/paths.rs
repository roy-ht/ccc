//! `~/.ccc[/dev]` の解決。ccc 本体の `paths.rs` はここへ委譲する
//! （CLI と本体で必ず同じディレクトリを見るための単一の正）。

use std::path::PathBuf;

/// ccc のルートディレクトリ（常に `~/.ccc/`）を返す。
///
/// `agent_settings/` を含む共有資産はこの直下に置く。
/// 開発版・配布版で並行運用しても Claude の認証情報を共有できるよう、
/// このパスは環境変数で上書きしない。
pub fn ccc_root() -> anyhow::Result<PathBuf> {
    let home =
        std::env::var("HOME").map_err(|_| anyhow::anyhow!("HOME environment variable not set"))?;
    Ok(PathBuf::from(home).join(".ccc"))
}

/// 可変データの保存先を返す。
/// `CCC_DEV` が設定されていれば `~/.ccc/dev/`、そうでなければ `~/.ccc/`。
pub fn data_root() -> anyhow::Result<PathBuf> {
    let root = ccc_root()?;
    if is_dev_mode() {
        Ok(root.join("dev"))
    } else {
        Ok(root)
    }
}

pub fn is_dev_mode() -> bool {
    matches!(std::env::var("CCC_DEV").as_deref(), Ok(v) if !v.is_empty() && v != "0")
}

/// `~/.ccc/forwards/` または dev 時 `~/.ccc/dev/forwards/`（port forward 台帳の置き場）
pub fn forwards_dir() -> anyhow::Result<PathBuf> {
    Ok(data_root()?.join("forwards"))
}
