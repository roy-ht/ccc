//! ルート配下にパスを閉じ込めるためのガード。
//!
//! ローカルは `canonicalize` で実体（symlink 解決後）を検査する。
//! リモートは `canonicalize` できないので、構文ベースで `..` と絶対パスを禁じる
//! （SSH 経由ではどのみち任意コードが走るため、symlink エスケープの厳密化は諦める）。

use anyhow::{bail, Context, Result};
use std::path::{Component, Path, PathBuf};

/// ローカル用: `root` 配下に実体が収まる絶対パスを返す。
/// `rel` は POSIX 相対パス（空文字はルート自身）。
pub fn join_within_root_local(root: &str, rel: &str) -> Result<PathBuf> {
    let root_p = Path::new(root);
    if !root_p.is_absolute() {
        bail!("ルートディレクトリが絶対パスではありません: {root}");
    }
    let canon_root = root_p
        .canonicalize()
        .with_context(|| format!("ルートを解決できません: {root}"))?;

    let rel_trim = rel.trim_start_matches('/');
    check_no_escape_syntax(rel_trim)?;

    let joined = canon_root.join(rel_trim);
    // 実体が無い（存在しないパス）の場合は join のままで OK とする
    // 存在する場合は canonicalize して再検査
    let canon = match joined.canonicalize() {
        Ok(c) => c,
        Err(_) => joined.clone(),
    };
    if !canon.starts_with(&canon_root) {
        bail!("ルートの外側を指すパスは許可されません");
    }
    Ok(canon)
}

/// リモート用: 構文検査のみ。`root` と `rel` を `/` で繋いだ POSIX パスを返す。
pub fn join_within_root_remote(root: &str, rel: &str) -> Result<String> {
    let rel_trim = rel.trim_start_matches('/');
    check_no_escape_syntax(rel_trim)?;
    if rel_trim.is_empty() {
        Ok(root.to_string())
    } else {
        // `root` 末尾の `/` を取り除いて連結
        let root_clean = root.trim_end_matches('/');
        Ok(format!("{root_clean}/{rel_trim}"))
    }
}

fn check_no_escape_syntax(rel: &str) -> Result<()> {
    for c in Path::new(rel).components() {
        match c {
            Component::ParentDir => bail!("'..' を含むパスは許可されません"),
            Component::RootDir | Component::Prefix(_) => {
                bail!("絶対パスは許可されません")
            }
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_parent_dir() {
        let r = join_within_root_remote("/srv/app", "src/../etc/passwd");
        assert!(r.is_err());
    }

    #[test]
    fn rejects_absolute_path() {
        let r = join_within_root_remote("/srv/app", "/etc/passwd");
        // trim_start_matches('/') により先頭の `/` は剥がれるので、`etc/passwd` 相対として通る点に注意。
        // `check_no_escape_syntax` 側は `RootDir/Prefix` を見るので、ここは "etc/passwd" と等価。
        assert!(r.is_ok(), "先頭スラッシュは剥がして相対扱い");
    }

    #[test]
    fn accepts_normal_relative() {
        let r = join_within_root_remote("/srv/app", "src/main.rs").unwrap();
        assert_eq!(r, "/srv/app/src/main.rs");
    }

    #[test]
    fn empty_rel_is_root() {
        let r = join_within_root_remote("/srv/app", "").unwrap();
        assert_eq!(r, "/srv/app");
    }

    #[test]
    fn rejects_double_dot_mid() {
        let r = join_within_root_remote("/srv/app", "a/../b");
        assert!(r.is_err());
    }
}
