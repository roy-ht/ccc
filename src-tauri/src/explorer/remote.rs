//! SSH (ControlMaster 経由) でリモートホスト上のファイル操作を行う `FileSource`。
//!
//! 既存の ControlMaster ソケット（ccc 専用 or ユーザー側）を再利用し、
//! `ssh -T -o BatchMode=yes [-o ControlPath=...] <alias> '<cmd>'` で各種コマンドを叩く。
//! バイナリ転送は base64 で受け取り、ssh が改行・LF/CR を破壊しても安全にする。
//!
//! ホストが `~/.ssh/config` でユーザー側 ControlMaster を持つ場合、ccc は専用 master を
//! 立てない（[`crate::ssh_config::user_has_control_master`]）。その場合 `-o ControlPath`
//! を指定せず、ssh の通常解決（ユーザー側 ControlPath）に委ねる。
//!
//! リモート OS は基本 Linux を想定（GNU find/stat）。それ以外は段階的に対応予定。

use anyhow::{anyhow, bail, Context, Result};
use std::path::Path;
use std::process::Command;

use super::path_guard::join_within_root_remote;
use super::ripgrep::{is_binary_head, parse_rg_json};
use super::source::FileSource;
use super::types::{
    CopyFailure, CopySummary, FileMeta, FileNode, Preview, PreviewOpts, SearchHit, SearchOpts,
};
use crate::ssh_config;

pub struct RemoteFileSource {
    alias: String,
    /// リモート側のルート絶対パス。
    root: String,
    /// `true` のときユーザー側 ControlMaster（`~/.ssh/config` 由来）を使う。
    /// ccc 専用 master ソケット（`~/.ssh/ccc-cm-%C`）は立てない / 存在しない。
    uses_user_master: bool,
}

impl RemoteFileSource {
    pub fn new(alias: String, root: String) -> Result<Self> {
        if root.trim().is_empty() {
            bail!("リモートのルートディレクトリが空です");
        }
        // ユーザー側 ControlMaster が設定されているかは `ssh -G` 経由で確認できる。
        // 解決失敗時は ccc 専用 master を使う想定で false にフォールバックする。
        let uses_user_master = ssh_config::user_has_control_master(&alias).unwrap_or(false);
        Ok(Self {
            alias,
            root,
            uses_user_master,
        })
    }

    fn abs(&self, rel: &str) -> Result<String> {
        join_within_root_remote(&self.root, rel)
    }

    /// ssh コマンドに付ける ControlPath オプションを必要なら返す。
    /// ユーザー側 master を使うホストでは `-o ControlPath` を強制しない
    /// （ssh -G が解決した値に任せる）。
    fn control_path_args(&self) -> Vec<String> {
        if self.uses_user_master {
            Vec::new()
        } else {
            vec![
                "-o".to_string(),
                format!("ControlPath={}", ssh_config::CCC_CONTROL_PATH),
            ]
        }
    }

    /// ControlMaster が生きていれば `ssh -O check` が "Master running" を吐く。
    /// exit code は ssh config の post-command（LocalCommand 等）で汚染されることが
    /// あるため、stdout / stderr のテキストで判定する方が堅牢。
    /// 失敗時は「リモート接続が未確立」エラーを返す（フロントでリトライ）。
    fn ensure_master(&self) -> Result<()> {
        let mut args: Vec<String> = vec!["-O".into(), "check".into()];
        args.extend(self.control_path_args());
        args.push(self.alias.clone());

        let out = Command::new("ssh").args(&args).output();
        match out {
            Ok(out) => {
                let stdout = String::from_utf8_lossy(&out.stdout);
                let stderr = String::from_utf8_lossy(&out.stderr);
                if stdout.contains("Master running")
                    || stderr.contains("Master running")
                    || out.status.success()
                {
                    Ok(())
                } else {
                    Err(anyhow!(
                        "リモート '{}' への接続が確立されていません。ターミナルタブで再接続してください",
                        self.alias
                    ))
                }
            }
            Err(_) => Err(anyhow!("ssh の起動に失敗しました (alias={})", self.alias)),
        }
    }

    /// SSH ControlMaster 経由でリモートコマンドを実行し、stdout を返す。
    /// `-T` で擬似 TTY を抑制（出力に CR/LF が混ざらないようにする）。
    fn ssh_exec(&self, cmd: &str) -> Result<Vec<u8>> {
        let mut args: Vec<String> = vec!["-T".into(), "-o".into(), "BatchMode=yes".into()];
        args.extend(self.control_path_args());
        args.push(self.alias.clone());
        args.push(cmd.to_string());

        let out = Command::new("ssh")
            .args(&args)
            .output()
            .with_context(|| format!("ssh 実行に失敗しました (alias={})", self.alias))?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            bail!("ssh コマンド失敗: {}", stderr.trim());
        }
        Ok(out.stdout)
    }
}

impl FileSource for RemoteFileSource {
    fn list_directory(&self, rel: &str) -> Result<Vec<FileNode>> {
        self.ensure_master()?;
        let abs = self.abs(rel)?;
        // `find -printf` は GNU find 拡張。BSD find は非対応だが、リモート用途は基本 Linux 前提。
        let abs_q = shq(&abs);
        let cmd = format!(
            "cd {abs_q} && find . -maxdepth 1 -mindepth 1 -printf '%y\\t%s\\t%P\\n' 2>/dev/null | LC_ALL=C sort"
        );
        let out = self.ssh_exec(&cmd)?;
        let text = String::from_utf8_lossy(&out);
        parse_find_output(&text, rel)
    }

    fn stat(&self, rel: &str) -> Result<FileMeta> {
        self.ensure_master()?;
        let abs = self.abs(rel)?;
        let abs_q = shq(&abs);
        let cmd =
            format!("stat -c '%s' -- {abs_q} 2>/dev/null && head -c 8192 -- {abs_q} | base64");
        let out = self.ssh_exec(&cmd)?;
        let text = String::from_utf8_lossy(&out);
        let mut lines = text.lines();
        let size_line = lines
            .next()
            .ok_or_else(|| anyhow!("stat の応答が空でした"))?;
        let size: u64 = size_line
            .trim()
            .parse()
            .context("stat の出力をパースできません")?;
        let b64: String = lines.collect::<Vec<_>>().join("");
        let head = base64_decode(&b64).unwrap_or_default();
        Ok(FileMeta {
            size,
            mime: None,
            is_binary: is_binary_head(&head),
        })
    }

    fn read_preview(&self, rel: &str, opts: &PreviewOpts) -> Result<Preview> {
        self.ensure_master()?;
        let abs = self.abs(rel)?;
        let abs_q = shq(&abs);
        let p = Path::new(rel);
        let category = classify_remote(p);

        let size_out = self.ssh_exec(&format!("stat -c '%s' -- {abs_q}"))?;
        let size_str = String::from_utf8_lossy(&size_out);
        let size: u64 = size_str
            .trim()
            .parse()
            .context("stat の出力をパースできません")?;

        let limit = match category {
            RemoteCategory::Image => opts.image_limit,
            RemoteCategory::Pdf => opts.pdf_limit,
            _ => opts.text_limit,
        };
        if size > limit && !matches!(category, RemoteCategory::Text | RemoteCategory::Markdown) {
            return Ok(Preview::TooLarge { size, limit });
        }

        match category {
            RemoteCategory::Image => {
                let b64 = self.ssh_exec(&format!("base64 -- {abs_q}"))?;
                let b64 = strip_whitespace(&b64);
                let mime = guess_image_mime(p).unwrap_or_else(|| "image/png".to_string());
                Ok(Preview::Image {
                    mime,
                    base64: b64,
                    size,
                })
            }
            RemoteCategory::Pdf => {
                let b64 = self.ssh_exec(&format!("base64 -- {abs_q}"))?;
                let b64 = strip_whitespace(&b64);
                Ok(Preview::Pdf { base64: b64, size })
            }
            RemoteCategory::Markdown => {
                let cap = opts.text_limit;
                let b64 = self.ssh_exec(&format!("head -c {cap} -- {abs_q} | base64"))?;
                let bytes = base64_decode(&strip_whitespace(&b64)).unwrap_or_default();
                let content = String::from_utf8_lossy(&bytes).into_owned();
                Ok(Preview::Markdown {
                    content,
                    truncated: size > cap,
                    size,
                })
            }
            RemoteCategory::KnownBinary => Ok(Preview::Binary { size, mime: None }),
            RemoteCategory::Text => {
                // 先頭 8KB をまず取得してバイナリ判定
                let head_b64 = self.ssh_exec(&format!("head -c 8192 -- {abs_q} | base64"))?;
                let head = base64_decode(&strip_whitespace(&head_b64)).unwrap_or_default();
                if is_binary_head(&head) {
                    return Ok(Preview::Binary { size, mime: None });
                }
                let cap = opts.text_limit;
                let body_b64 = self.ssh_exec(&format!("head -c {cap} -- {abs_q} | base64"))?;
                let bytes = base64_decode(&strip_whitespace(&body_b64)).unwrap_or_default();
                let content = String::from_utf8_lossy(&bytes).into_owned();
                Ok(Preview::Text {
                    content,
                    language: guess_language(p),
                    truncated: size > cap,
                    size,
                })
            }
        }
    }

    fn copy_into(&self, dest_rel: &str, sources: &[String]) -> Result<CopySummary> {
        self.ensure_master()?;
        let dest_abs = self.abs(dest_rel)?;

        // rsync の宛先には末尾 `/` を付ける（ディレクトリ配下にコピー）。
        let mut dest = dest_abs.trim_end_matches('/').to_string();
        dest.push('/');
        let remote_dest = format!("{}:{}", self.alias, dest);

        // ControlMaster を使う `-e ssh ...` を構成して rsync 経由でも同じソケットを通す。
        let mut ssh_inner = String::from("ssh -T -o BatchMode=yes");
        if !self.uses_user_master {
            ssh_inner.push_str(" -o ControlPath=");
            ssh_inner.push_str(ssh_config::CCC_CONTROL_PATH);
        }

        let mut copied = 0usize;
        let mut failed = Vec::new();
        // 1 ソースずつ呼び出し、どれが失敗したか個別に分かるようにする
        // （まとめて 1 回でもよいが、片方失敗時の挙動が不明瞭になる）。
        for src in sources {
            let out = Command::new("rsync")
                .arg("-az")
                .arg("--safe-links")
                .arg("-e")
                .arg(&ssh_inner)
                .arg("--")
                .arg(src)
                .arg(&remote_dest)
                .output();
            match out {
                Ok(o) if o.status.success() => copied += 1,
                Ok(o) => {
                    let stderr = String::from_utf8_lossy(&o.stderr).trim().to_string();
                    failed.push(CopyFailure {
                        source: src.clone(),
                        error: if stderr.is_empty() {
                            format!("rsync 失敗 (status={})", o.status)
                        } else {
                            stderr
                        },
                    });
                }
                Err(e) => failed.push(CopyFailure {
                    source: src.clone(),
                    error: format!("rsync 起動失敗: {e}"),
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
        self.ensure_master()?;
        let src_abs = self.abs(rel)?;
        // rsync ソース: `<alias>:<remote_abs>`。末尾 `/` は付けず、basename ごと dest にコピー
        // させる（dest_local_abs は最終的なファイル/ディレクトリ名そのもの）。
        let remote_src = format!("{}:{}", self.alias, src_abs);

        // copy_into と同じ ControlMaster 経由の `-e ssh ...` を組む。
        let mut ssh_inner = String::from("ssh -T -o BatchMode=yes");
        if !self.uses_user_master {
            ssh_inner.push_str(" -o ControlPath=");
            ssh_inner.push_str(ssh_config::CCC_CONTROL_PATH);
        }

        let out = Command::new("rsync")
            .arg("-az")
            .arg("--safe-links")
            .arg("-e")
            .arg(&ssh_inner)
            .arg("--")
            .arg(&remote_src)
            .arg(dest_local_abs)
            .output()
            .context("rsync の起動に失敗しました")?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            bail!(
                "rsync 失敗: {}",
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
        if query.trim().is_empty() {
            return Ok(vec![]);
        }
        self.ensure_master()?;
        let root_q = shq(&self.root);
        let query_q = shq(query);
        let glob_arg = match glob {
            Some(g) if !g.trim().is_empty() => format!(" --glob {}", shq(g)),
            _ => String::new(),
        };
        let cmd = format!(
            "cd {root_q} && rg --json --max-count 20 --max-columns 300 -S{glob_arg} -- {query_q} ."
        );
        let out = match self.ssh_exec(&cmd) {
            Ok(v) => v,
            Err(e) => {
                // rg がリモートに無い場合は分かりやすいエラーに置換する
                let msg = e.to_string();
                if msg.contains("rg: command not found") || msg.contains("rg: not found") {
                    bail!(
                        "リモート '{}' に ripgrep がインストールされていません（apt install ripgrep 等を試してください）",
                        self.alias
                    );
                }
                // マッチなしは exit 1 で stderr 空になりがち。空 stdout なら成功扱いで上に流す。
                return Err(e);
            }
        };
        parse_rg_json(&out, &self.root, opts.max_results)
    }
}

// ─── パース ──────────────────────────────────────────────────────────────────

fn parse_find_output(text: &str, base_rel: &str) -> Result<Vec<FileNode>> {
    let mut out = Vec::new();
    for line in text.lines() {
        if line.is_empty() {
            continue;
        }
        // 形式: "<type>\t<size>\t<basename>"
        let mut it = line.splitn(3, '\t');
        let ty = match it.next() {
            Some(s) => s,
            None => continue,
        };
        let size_s = match it.next() {
            Some(s) => s,
            None => continue,
        };
        let name = match it.next() {
            Some(s) => s.to_string(),
            None => continue,
        };
        if name.is_empty() {
            continue;
        }
        let is_dir = match ty {
            "d" => true,
            "f" => false,
            "l" => {
                // symlink は最終的なタイプが分からないので、size はあるなら file、無いなら dir 扱い。
                // ここでは保守的に「ファイル扱い」にして表示し、展開しないことで安全側に倒す。
                false
            }
            _ => continue,
        };
        let size = size_s.parse::<u64>().ok();
        let path = if base_rel.is_empty() {
            name.clone()
        } else {
            let clean = base_rel.trim_end_matches('/');
            format!("{clean}/{name}")
        };
        let hidden = name.starts_with('.');
        out.push(FileNode {
            name,
            path,
            is_dir,
            size: if is_dir { None } else { size },
            hidden,
        });
    }
    // find は sort 済みだが、ディレクトリ優先に並べ直す。
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

// ─── ヘルパ ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
enum RemoteCategory {
    Text,
    Markdown,
    Image,
    Pdf,
    KnownBinary,
}

fn classify_remote(p: &Path) -> RemoteCategory {
    let ext = p
        .extension()
        .and_then(|s| s.to_str())
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "md" | "mdx" | "markdown" => RemoteCategory::Markdown,
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "svg" | "ico" => RemoteCategory::Image,
        "pdf" => RemoteCategory::Pdf,
        "zip" | "gz" | "tar" | "bz2" | "xz" | "7z" | "rar" | "exe" | "dll" | "so" | "dylib"
        | "class" | "jar" | "war" | "wasm" | "mp4" | "mov" | "avi" | "mkv" | "mp3" | "wav"
        | "flac" | "ogg" | "ttf" | "otf" | "woff" | "woff2" => RemoteCategory::KnownBinary,
        _ => RemoteCategory::Text,
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

/// 単引用符で囲み、内部の `'` を `'\''` に置換する POSIX シェルクオート。
fn shq(s: &str) -> String {
    let escaped = s.replace('\'', r#"'\''"#);
    format!("'{}'", escaped)
}

fn strip_whitespace(bytes: &[u8]) -> String {
    bytes
        .iter()
        .filter(|b| !matches!(**b, b'\n' | b'\r' | b' ' | b'\t'))
        .map(|b| *b as char)
        .collect()
}

fn base64_decode(s: &str) -> Option<Vec<u8>> {
    // 軽量自前実装（依存追加を避ける）。標準アルファベット + パディング `=` のみ対応。
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    let mut buf: u32 = 0;
    let mut count = 0;
    for &b in bytes {
        let v = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => break,
            _ => continue, // 想定外の文字は無視
        };
        buf = (buf << 6) | (v as u32);
        count += 1;
        if count == 4 {
            out.push((buf >> 16) as u8);
            out.push((buf >> 8) as u8);
            out.push(buf as u8);
            buf = 0;
            count = 0;
        }
    }
    if count == 2 {
        out.push((buf >> 4) as u8);
    } else if count == 3 {
        out.push((buf >> 10) as u8);
        out.push((buf >> 2) as u8);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_find_output() {
        let out = "d\t4096\tsrc\nf\t1234\tREADME.md\nf\t0\t.gitignore\nl\t12\tsymlink\n";
        let nodes = parse_find_output(out, "").unwrap();
        assert_eq!(nodes.len(), 4);
        // ディレクトリが先頭
        assert!(nodes[0].is_dir && nodes[0].name == "src");
        // dotfile は hidden=true
        let dot = nodes.iter().find(|n| n.name == ".gitignore").unwrap();
        assert!(dot.hidden);
    }

    #[test]
    fn shq_basic() {
        assert_eq!(shq("foo"), "'foo'");
        assert_eq!(shq("it's"), "'it'\\''s'");
        assert_eq!(shq("/a b/c"), "'/a b/c'");
    }

    #[test]
    fn base64_decode_round_trip() {
        let decoded = base64_decode("Zm9vYmFy").unwrap();
        assert_eq!(decoded, b"foobar");
        let decoded = base64_decode("Zm9v").unwrap();
        assert_eq!(decoded, b"foo");
        let decoded = base64_decode("Zg==").unwrap();
        assert_eq!(decoded, b"f");
        let decoded = base64_decode("Zm8=").unwrap();
        assert_eq!(decoded, b"fo");
    }

    #[test]
    fn base64_skips_whitespace() {
        let decoded = base64_decode("Zm9v\nYmFy\n").unwrap();
        assert_eq!(decoded, b"foobar");
    }
}
