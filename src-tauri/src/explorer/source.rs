//! `FileSource` トレイトとインスタンス情報からの dispatch。

use anyhow::{anyhow, Result};

use super::local::LocalFileSource;
use super::remote::RemoteFileSource;
use super::types::{CopySummary, FileMeta, FileNode, Preview, PreviewOpts, SearchHit, SearchOpts};
use crate::instance::types::{InstanceInfo, InstanceKind};

/// ローカル/リモートで共通の読み取り + コピー API。
pub trait FileSource: Send + Sync {
    fn list_directory(&self, rel: &str) -> Result<Vec<FileNode>>;
    fn stat(&self, rel: &str) -> Result<FileMeta>;
    fn read_preview(&self, rel: &str, opts: &PreviewOpts) -> Result<Preview>;
    fn search(&self, query: &str, glob: Option<&str>, opts: &SearchOpts) -> Result<Vec<SearchHit>>;
    /// ローカル絶対パスの `sources`（ファイル / ディレクトリ）を当該 FS の
    /// `<root>/<dest_rel>/` 配下にコピーする。
    /// - ローカル FS: `cp -aR` 相当
    /// - リモート FS: `rsync -az` で SSH 越し転送
    fn copy_into(&self, dest_rel: &str, sources: &[String]) -> Result<CopySummary>;

    /// 当該 FS の `<root>/<rel>`（ファイル or ディレクトリ）をローカル絶対パス
    /// `dest_local_abs` にコピーする。`dest_local_abs` の親ディレクトリは存在する前提。
    /// - ローカル FS: `cp -aR` 相当
    /// - リモート FS: `rsync -az` で SSH 越しダウンロード
    fn download_to(&self, rel: &str, dest_local_abs: &str) -> Result<()>;
}

/// `InstanceInfo.kind` と任意の `root` から FileSource を組み立てる。
/// `root` が `None` または空のときは `instance.directory` を採用する。
/// これにより Explorer が起動 CWD から外れた場所を閲覧することを可能にする。
pub fn make_source(instance: &InstanceInfo, root: Option<&str>) -> Result<Box<dyn FileSource>> {
    let root = match root {
        Some(r) if !r.trim().is_empty() => r.to_string(),
        _ => instance
            .directory
            .clone()
            .ok_or_else(|| anyhow!("インスタンスに作業ディレクトリが設定されていません"))?,
    };
    if root.trim().is_empty() {
        return Err(anyhow!("作業ディレクトリが空です"));
    }
    match (&instance.kind, instance.host_alias.as_deref()) {
        (InstanceKind::Local, _) => Ok(Box::new(LocalFileSource::new(root)?)),
        (InstanceKind::Remote, Some(alias)) => {
            Ok(Box::new(RemoteFileSource::new(alias.to_string(), root)?))
        }
        (InstanceKind::Remote, None) => Err(anyhow!(
            "リモートインスタンスに host_alias が設定されていません"
        )),
    }
}
