//! Explorer タブのバックエンド: ローカル/リモート両対応のファイル閲覧 + ripgrep 検索。
//!
//! 公開フロー:
//! - フロントの `invoke("explorer_list_directory", { instance_id, path })` 等 → ここの Tauri commands
//! - `make_source(&InstanceInfo)` で `Box<dyn FileSource>` を組み立て、ローカル/リモートの違いを吸収
//! - 入力 `path` は POSIX 相対パス。`path_guard` でルート外参照を弾く

pub mod local;
pub mod path_guard;
pub mod remote;
pub mod ripgrep;
pub mod source;
pub mod types;

use tauri::State;

use crate::instance::InstanceManager;
use source::{make_source, FileSource};
use types::{CopySummary, FileMeta, FileNode, Preview, PreviewOpts, SearchHit, SearchOpts};

/// インスタンス ID と任意の `root` から FileSource を組み立てる共通処理。
/// `root` を渡すと `InstanceInfo.directory` ではなく当該パスをルートにする。
/// I/O ブロッキングの可能性があるので呼び出し側は `spawn_blocking` でラップする想定。
fn source_for(
    manager: &InstanceManager,
    instance_id: &str,
    root: Option<&str>,
) -> Result<Box<dyn FileSource>, String> {
    let info = manager
        .list()
        .into_iter()
        .find(|i| i.id == instance_id)
        .ok_or_else(|| format!("インスタンスが見つかりません: {instance_id}"))?;
    make_source(&info, root).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn explorer_list_directory(
    instance_id: String,
    root: Option<String>,
    path: String,
    manager: State<'_, InstanceManager>,
) -> Result<Vec<FileNode>, String> {
    let src = source_for(&manager, &instance_id, root.as_deref())?;
    tauri::async_runtime::spawn_blocking(move || {
        src.list_directory(&path).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| format!("join error: {e}"))?
}

#[tauri::command]
pub async fn explorer_stat(
    instance_id: String,
    root: Option<String>,
    path: String,
    manager: State<'_, InstanceManager>,
) -> Result<FileMeta, String> {
    let src = source_for(&manager, &instance_id, root.as_deref())?;
    tauri::async_runtime::spawn_blocking(move || src.stat(&path).map_err(|e| e.to_string()))
        .await
        .map_err(|e| format!("join error: {e}"))?
}

#[tauri::command]
pub async fn explorer_get_preview(
    instance_id: String,
    root: Option<String>,
    path: String,
    manager: State<'_, InstanceManager>,
) -> Result<Preview, String> {
    let src = source_for(&manager, &instance_id, root.as_deref())?;
    let opts = PreviewOpts::default();
    tauri::async_runtime::spawn_blocking(move || {
        src.read_preview(&path, &opts).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| format!("join error: {e}"))?
}

#[tauri::command]
pub async fn explorer_copy_into(
    instance_id: String,
    root: Option<String>,
    dest_rel: String,
    sources: Vec<String>,
    manager: State<'_, InstanceManager>,
) -> Result<CopySummary, String> {
    let src = source_for(&manager, &instance_id, root.as_deref())?;
    tauri::async_runtime::spawn_blocking(move || {
        src.copy_into(&dest_rel, &sources)
            .map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| format!("join error: {e}"))?
}

/// ダウンロード結果。フロントには保存先ローカル絶対パスを返す。
#[derive(Debug, Clone, serde::Serialize)]
pub struct DownloadResult {
    pub saved_path: String,
}

/// `<root>/<path>` をローカルの `~/Downloads/<basename>` にコピーする。
/// 既に同名ファイルがある場合は `name (1).ext`、`name (2).ext` … と連番を付ける。
/// 保存先の絶対パスを返す。
#[tauri::command]
pub async fn explorer_download(
    instance_id: String,
    root: Option<String>,
    path: String,
    manager: State<'_, InstanceManager>,
) -> Result<DownloadResult, String> {
    let src = source_for(&manager, &instance_id, root.as_deref())?;
    tauri::async_runtime::spawn_blocking(move || -> Result<DownloadResult, String> {
        let dest = unique_download_path(&path).map_err(|e| e.to_string())?;
        src.download_to(&path, &dest).map_err(|e| e.to_string())?;
        Ok(DownloadResult { saved_path: dest })
    })
    .await
    .map_err(|e| format!("join error: {e}"))?
}

/// `~/Downloads` 配下に、basename(path) を元にしたユニークな保存先パスを構築する。
/// `~/Downloads` が無ければ作成する。
fn unique_download_path(rel: &str) -> anyhow::Result<String> {
    use std::path::PathBuf;
    let home = std::env::var("HOME").map_err(|_| anyhow::anyhow!("HOME が未設定です"))?;
    let mut dir = PathBuf::from(home);
    dir.push("Downloads");
    std::fs::create_dir_all(&dir)
        .map_err(|e| anyhow::anyhow!("~/Downloads を作成できません: {e}"))?;

    let basename = std::path::Path::new(rel)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("download");
    let candidate = dir.join(basename);
    if !candidate.exists() {
        return Ok(candidate.to_string_lossy().to_string());
    }

    // `foo.tar.gz` のような複合拡張子では最後の `.` のみ拡張子扱いにする（簡素化）。
    let (stem, ext) = match basename.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s.to_string(), format!(".{e}")),
        _ => (basename.to_string(), String::new()),
    };
    for n in 1..1000 {
        let try_name = format!("{stem} ({n}){ext}");
        let p = dir.join(&try_name);
        if !p.exists() {
            return Ok(p.to_string_lossy().to_string());
        }
    }
    anyhow::bail!("保存先名の候補が枯渇しました: {basename}")
}

#[tauri::command]
pub async fn explorer_search(
    instance_id: String,
    root: Option<String>,
    query: String,
    glob: Option<String>,
    max_results: Option<usize>,
    manager: State<'_, InstanceManager>,
) -> Result<Vec<SearchHit>, String> {
    let src = source_for(&manager, &instance_id, root.as_deref())?;
    let mut opts = SearchOpts::default();
    if let Some(n) = max_results {
        opts.max_results = n;
    }
    tauri::async_runtime::spawn_blocking(move || {
        src.search(&query, glob.as_deref(), &opts)
            .map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| format!("join error: {e}"))?
}
