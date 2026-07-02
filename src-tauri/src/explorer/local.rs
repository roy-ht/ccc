//! ローカルファイルシステムに対する `FileSource` 実装。

use anyhow::{anyhow, Context, Result};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use super::path_guard::join_within_root_local;
use super::ripgrep::{is_binary_head, parse_rg_json};
use super::source::FileSource;
use super::types::{
    CopyFailure, CopySummary, FileMeta, FileNode, Preview, PreviewOpts, SearchHit, SearchOpts,
};

pub struct LocalFileSource {
    root: PathBuf,
    root_str: String,
}

impl LocalFileSource {
    pub fn new(root: String) -> Result<Self> {
        let p = PathBuf::from(&root);
        if !p.is_absolute() {
            anyhow::bail!("ルートは絶対パスで指定してください: {root}");
        }
        let canon = p
            .canonicalize()
            .with_context(|| format!("ルートを解決できません: {root}"))?;
        let root_str = canon.to_string_lossy().to_string();
        Ok(Self {
            root: canon,
            root_str,
        })
    }

    fn abs(&self, rel: &str) -> Result<PathBuf> {
        join_within_root_local(&self.root_str, rel)
    }
}

impl FileSource for LocalFileSource {
    fn list_directory(&self, rel: &str) -> Result<Vec<FileNode>> {
        let abs = self.abs(rel)?;
        let md = fs::metadata(&abs).with_context(|| format!("stat 失敗: {}", abs.display()))?;
        if !md.is_dir() {
            anyhow::bail!("ディレクトリではありません: {}", abs.display());
        }
        let mut out = Vec::new();
        for entry in fs::read_dir(&abs)? {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };
            let name = entry.file_name().to_string_lossy().to_string();
            let path_rel = if rel.is_empty() {
                name.clone()
            } else {
                let clean = rel.trim_end_matches('/');
                format!("{clean}/{name}")
            };
            let ft = match entry.file_type() {
                Ok(t) => t,
                Err(_) => continue,
            };
            let (is_dir, size) = if ft.is_symlink() {
                // symlink は最終的に指す先のタイプで分類する。リンク切れは無視。
                match fs::metadata(entry.path()) {
                    Ok(m) => (m.is_dir(), if m.is_file() { Some(m.len()) } else { None }),
                    Err(_) => continue,
                }
            } else if ft.is_dir() {
                (true, None)
            } else {
                let size = entry.metadata().ok().map(|m| m.len());
                (false, size)
            };
            let hidden = name.starts_with('.');
            out.push(FileNode {
                name,
                path: path_rel,
                is_dir,
                size,
                hidden,
            });
        }
        // ディレクトリ優先 → 名前昇順
        out.sort_by(|a, b| match (a.is_dir, b.is_dir) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a
                .name
                .to_ascii_lowercase()
                .cmp(&b.name.to_ascii_lowercase()),
        });
        Ok(out)
    }

    fn stat(&self, rel: &str) -> Result<FileMeta> {
        let abs = self.abs(rel)?;
        let md = fs::metadata(&abs)?;
        let size = md.len();
        let head = read_head(&abs, 8192).unwrap_or_default();
        Ok(FileMeta {
            size,
            mime: None,
            is_binary: is_binary_head(&head),
        })
    }

    fn read_preview(&self, rel: &str, opts: &PreviewOpts) -> Result<Preview> {
        let abs = self.abs(rel)?;
        let md = fs::metadata(&abs)?;
        if md.is_dir() {
            anyhow::bail!("ディレクトリはプレビューできません");
        }
        let size = md.len();
        let category = classify_path(&abs);
        let limit = match category {
            PathCategory::Image => opts.image_limit,
            PathCategory::Pdf => opts.pdf_limit,
            _ => opts.text_limit,
        };
        if size > limit && !matches!(category, PathCategory::Text | PathCategory::Markdown) {
            return Ok(Preview::TooLarge { size, limit });
        }

        match category {
            PathCategory::Image => {
                let mut buf = Vec::with_capacity(size as usize);
                fs::File::open(&abs)?.read_to_end(&mut buf)?;
                let mime = guess_image_mime(&abs).unwrap_or_else(|| "image/png".to_string());
                Ok(Preview::Image {
                    mime,
                    base64: base64_encode(&buf),
                    size,
                })
            }
            PathCategory::Pdf => {
                let mut buf = Vec::with_capacity(size as usize);
                fs::File::open(&abs)?.read_to_end(&mut buf)?;
                Ok(Preview::Pdf {
                    base64: base64_encode(&buf),
                    size,
                })
            }
            PathCategory::Markdown => {
                let (content, truncated) = read_text_capped(&abs, opts.text_limit)?;
                Ok(Preview::Markdown {
                    content,
                    truncated,
                    size,
                })
            }
            PathCategory::KnownBinary => Ok(Preview::Binary { size, mime: None }),
            PathCategory::Text => {
                // 先頭 8KB でバイナリ判定。NUL 含めば Binary、それ以外はテキスト先頭 5MB。
                let head = read_head(&abs, 8192)?;
                if is_binary_head(&head) {
                    Ok(Preview::Binary { size, mime: None })
                } else {
                    let (content, truncated) = read_text_capped(&abs, opts.text_limit)?;
                    let language = guess_language(&abs);
                    Ok(Preview::Text {
                        content,
                        language,
                        truncated,
                        size,
                    })
                }
            }
        }
    }

    fn copy_into(&self, dest_rel: &str, sources: &[String]) -> Result<CopySummary> {
        use std::process::Command;
        let dest_abs = self.abs(dest_rel)?;
        let md = fs::metadata(&dest_abs).with_context(|| {
            format!(
                "コピー先ディレクトリを stat できません: {}",
                dest_abs.display()
            )
        })?;
        if !md.is_dir() {
            anyhow::bail!(
                "コピー先はディレクトリではありません: {}",
                dest_abs.display()
            );
        }

        let mut copied = 0usize;
        let mut failed = Vec::new();
        for src in sources {
            // `cp -aR <src> <dest>/` で属性保持 + 再帰コピー。macOS/Linux 両対応。
            let result = Command::new("cp")
                .arg("-aR")
                .arg("--")
                .arg(src)
                .arg(&dest_abs)
                .output();
            match result {
                Ok(out) if out.status.success() => copied += 1,
                Ok(out) => {
                    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
                    failed.push(CopyFailure {
                        source: src.clone(),
                        error: if stderr.is_empty() {
                            format!("cp 失敗 (status={})", out.status)
                        } else {
                            stderr
                        },
                    });
                }
                Err(e) => failed.push(CopyFailure {
                    source: src.clone(),
                    error: format!("cp 起動失敗: {e}"),
                }),
            }
        }
        Ok(CopySummary {
            copied,
            failed,
            dest_rel: dest_rel.to_string(),
        })
    }

    fn download_to(&self, rel: &str, dest_local_abs: &str) -> Result<()> {
        use std::process::Command;
        let src_abs = self.abs(rel)?;
        let out = Command::new("cp")
            .arg("-aR")
            .arg("--")
            .arg(&src_abs)
            .arg(dest_local_abs)
            .output()
            .context("cp の起動に失敗しました")?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            anyhow::bail!(
                "cp 失敗: {}",
                if stderr.is_empty() {
                    format!("status={}", out.status)
                } else {
                    stderr
                }
            );
        }
        Ok(())
    }

    fn search(&self, query: &str, glob: Option<&str>, opts: &SearchOpts) -> Result<Vec<SearchHit>> {
        use std::process::Command;
        if query.trim().is_empty() {
            return Ok(vec![]);
        }
        let mut cmd = Command::new("rg");
        cmd.arg("--json")
            .arg("--max-count")
            .arg("20")
            .arg("--max-columns")
            .arg("300")
            .arg("-S");
        if let Some(g) = glob {
            if !g.trim().is_empty() {
                cmd.arg("--glob").arg(g);
            }
        }
        cmd.arg("--").arg(query).arg(&self.root);
        let out = cmd.output().map_err(|e| {
            anyhow!(
                "rg 起動に失敗: {e} — `rg` (ripgrep) がインストールされているか確認してください"
            )
        })?;
        // rg はマッチ無しで終了コード 1。stderr が空ならそれは正常扱い。
        if !out.status.success() && !out.stderr.is_empty() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            // exit 1 は「マッチなし」を意味するので、stderr に有意なメッセージがある場合のみ失敗扱い。
            if out.status.code() != Some(1) || !stderr.trim().is_empty() {
                anyhow::bail!("rg 失敗: {stderr}");
            }
        }
        parse_rg_json(&out.stdout, &self.root_str, opts.max_results)
    }
}

// ─── ヘルパ ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
enum PathCategory {
    Text,
    Markdown,
    Image,
    Pdf,
    KnownBinary,
}

fn classify_path(p: &Path) -> PathCategory {
    let ext = p
        .extension()
        .and_then(|s| s.to_str())
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "md" | "mdx" | "markdown" => PathCategory::Markdown,
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "svg" | "ico" => PathCategory::Image,
        "pdf" => PathCategory::Pdf,
        // 既知の純バイナリ拡張子（大きい場合があるので即 Binary 扱い）
        "zip" | "gz" | "tar" | "bz2" | "xz" | "7z" | "rar" | "exe" | "dll" | "so" | "dylib"
        | "class" | "jar" | "war" | "wasm" | "mp4" | "mov" | "avi" | "mkv" | "mp3" | "wav"
        | "flac" | "ogg" | "ttf" | "otf" | "woff" | "woff2" => PathCategory::KnownBinary,
        _ => PathCategory::Text,
    }
}

fn guess_image_mime(p: &Path) -> Option<String> {
    let ext = p
        .extension()
        .and_then(|s| s.to_str())
        .map(|s| s.to_ascii_lowercase())?;
    Some(
        match ext.as_str() {
            "png" => "image/png",
            "jpg" | "jpeg" => "image/jpeg",
            "gif" => "image/gif",
            "webp" => "image/webp",
            "bmp" => "image/bmp",
            "svg" => "image/svg+xml",
            "ico" => "image/x-icon",
            _ => return None,
        }
        .to_string(),
    )
}

fn guess_language(p: &Path) -> Option<String> {
    let ext = p.extension().and_then(|s| s.to_str())?;
    Some(ext.to_ascii_lowercase())
}

fn read_head(p: &Path, limit: usize) -> Result<Vec<u8>> {
    let mut f = fs::File::open(p)?;
    let mut buf = vec![0u8; limit];
    let n = f.read(&mut buf)?;
    buf.truncate(n);
    Ok(buf)
}

fn read_text_capped(p: &Path, cap: u64) -> Result<(String, bool)> {
    let mut f = fs::File::open(p)?;
    let md = f.metadata()?;
    let total = md.len();
    let to_read = total.min(cap) as usize;
    let mut buf = vec![0u8; to_read];
    use std::io::Read;
    f.read_exact(&mut buf)?;
    let content = match String::from_utf8(buf) {
        Ok(s) => s,
        Err(e) => {
            // UTF-8 でない場合は lossy 変換（後段で truncated=true 扱い）
            String::from_utf8_lossy(&e.into_bytes()).into_owned()
        }
    };
    Ok((content, total > cap))
}

fn base64_encode(bytes: &[u8]) -> String {
    // 軽量化のため標準ライブラリ範囲で base64 を自前実装する
    // （新規依存を増やさず最小実装）
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    let mut i = 0;
    while i + 3 <= bytes.len() {
        let b0 = bytes[i];
        let b1 = bytes[i + 1];
        let b2 = bytes[i + 2];
        out.push(ALPHABET[(b0 >> 2) as usize] as char);
        out.push(ALPHABET[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
        out.push(ALPHABET[(((b1 & 0x0F) << 2) | (b2 >> 6)) as usize] as char);
        out.push(ALPHABET[(b2 & 0x3F) as usize] as char);
        i += 3;
    }
    let rem = bytes.len() - i;
    if rem == 1 {
        let b0 = bytes[i];
        out.push(ALPHABET[(b0 >> 2) as usize] as char);
        out.push(ALPHABET[((b0 & 0x03) << 4) as usize] as char);
        out.push('=');
        out.push('=');
    } else if rem == 2 {
        let b0 = bytes[i];
        let b1 = bytes[i + 1];
        out.push(ALPHABET[(b0 >> 2) as usize] as char);
        out.push(ALPHABET[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
        out.push(ALPHABET[((b1 & 0x0F) << 2) as usize] as char);
        out.push('=');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_round_trip_small() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn classify_extensions() {
        assert!(matches!(
            classify_path(Path::new("a.md")),
            PathCategory::Markdown
        ));
        assert!(matches!(
            classify_path(Path::new("a.png")),
            PathCategory::Image
        ));
        assert!(matches!(
            classify_path(Path::new("a.pdf")),
            PathCategory::Pdf
        ));
        assert!(matches!(
            classify_path(Path::new("a.zip")),
            PathCategory::KnownBinary
        ));
        assert!(matches!(
            classify_path(Path::new("a.rs")),
            PathCategory::Text
        ));
        assert!(matches!(
            classify_path(Path::new("README")),
            PathCategory::Text
        ));
    }
}
