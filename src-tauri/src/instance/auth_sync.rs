//! Claude Code の資格情報ファイルをリモートへ送るかどうかの判定。
//!
//! # 背景
//!
//! Claude Code の OAuth リフレッシュトークンは**使うたびにローテーション**する。
//! リモートで動いている Claude がトークンを更新すると、ローカル (macOS Keychain)
//! に残っている古いリフレッシュトークンはその時点で無効になる。
//! v0.10.3 まではリモートインスタンス作成のたびに無条件で scp していたため、
//! 「同じホストで既にセッションが動いている状態で新しいインスタンスを作ると
//! リモートの認証が壊れる」という事故が起きていた。
//!
//! # 方針
//!
//! **資格情報を時間的に後退させない**。ローカルとリモートのメタ情報だけを比較し、
//! リモートが同等以上に新しければ触らない。リモートが古い・失効している・存在しない
//! ときだけ上書きする（＝上書きが修復として働くケースに限定する）。
//!
//! アクセストークンの失効 (`expiresAt`) は「無効」を意味しない。リフレッシュ
//! トークンが生きていれば Claude 自身が起動時に更新できるため、送ってよい。
//! 一方 `refreshTokenExpiresAt` が切れているものは送っても復帰不能なので、
//! 生きているリモートを壊すだけになる。この 2 つを厳密に区別する。
//!
//! # 秘密の扱い
//!
//! トークン本体はこのモジュールの外へ出さない。同一性の比較とログ出力には
//! リフレッシュトークンの SHA-256 先頭 8 hex (`refresh_fp`) だけを使う。

use anyhow::{Context, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};

/// ローカル / リモート間の時計ズレを吸収するマージン (ms)。
const CLOCK_SKEW_MS: i64 = 60_000;

/// 資格情報ファイルから抽出した、秘密を含まないメタ情報。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthMeta {
    /// アクセストークンの失効時刻 (unix epoch ms)。
    pub expires_at: i64,
    /// リフレッシュトークンの失効時刻 (unix epoch ms)。
    /// 古い Claude Code が書いたファイルには存在しないことがあるため `Option`。
    pub refresh_token_expires_at: Option<i64>,
    /// リフレッシュトークンの SHA-256 先頭 8 hex。
    /// トークン系列が分岐したかどうかの判定とログ出力に使う。
    pub refresh_fp: String,
}

#[derive(Deserialize)]
struct AuthFile {
    #[serde(rename = "claudeAiOauth")]
    claude_ai_oauth: OauthSection,
}

#[derive(Deserialize)]
struct OauthSection {
    #[serde(rename = "refreshToken")]
    refresh_token: String,
    #[serde(rename = "expiresAt")]
    expires_at: i64,
    #[serde(rename = "refreshTokenExpiresAt")]
    refresh_token_expires_at: Option<i64>,
}

/// 資格情報 JSON の生バイト列からメタ情報を抽出する。
///
/// エラーメッセージにはトークンを含めない。
pub fn parse(bytes: &[u8]) -> Result<AuthMeta> {
    let file: AuthFile = serde_json::from_slice(bytes).context("資格情報 JSON の解析に失敗")?;
    let oauth = file.claude_ai_oauth;
    if oauth.refresh_token.is_empty() {
        anyhow::bail!("refreshToken が空");
    }
    Ok(AuthMeta {
        expires_at: oauth.expires_at,
        refresh_token_expires_at: oauth.refresh_token_expires_at,
        refresh_fp: fingerprint(&oauth.refresh_token),
    })
}

/// トークンの SHA-256 先頭 4 バイトを hex 8 文字で返す。
fn fingerprint(token: &str) -> String {
    let digest = Sha256::digest(token.as_bytes());
    let mut out = String::with_capacity(8);
    const HEX: &[u8] = b"0123456789abcdef";
    for b in &digest[..4] {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

/// リモート側の資格情報ファイルの状態。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteState {
    /// ファイルが存在しない（初回接続など）。
    Missing,
    /// 読み取れて解析できた。
    Present(AuthMeta),
    /// ssh 失敗・JSON 破損など、状態を判定できなかった。
    Unknown,
}

/// 転送するかどうかの判定結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    /// 転送してよいか。
    pub send: bool,
    /// デバッグログ / stderr に出す理由（秘密を含まない）。
    pub reason: String,
    /// ユーザーにローカルでの再ログインを促すべきか。
    pub warn_user: bool,
}

impl Verdict {
    fn send(reason: impl Into<String>) -> Self {
        Self {
            send: true,
            reason: reason.into(),
            warn_user: false,
        }
    }
    fn send_warn(reason: impl Into<String>) -> Self {
        Self {
            send: true,
            reason: reason.into(),
            warn_user: true,
        }
    }
    fn skip(reason: impl Into<String>) -> Self {
        Self {
            send: false,
            reason: reason.into(),
            warn_user: false,
        }
    }
    fn skip_warn(reason: impl Into<String>) -> Self {
        Self {
            send: false,
            reason: reason.into(),
            warn_user: true,
        }
    }
}

/// 転送可否を判定する。
///
/// `live_sibling` は「同じホスト × 同じプロファイルで既に生きている ccc インスタンス
/// がある」ことを示す。リモートの状態が判定できなかった (`Unknown`) ときの
/// フォールバックを安全側に倒すために使う。
///
/// 判定順序（上から評価し、最初に一致したものを採用）:
///
/// | # | 条件 | 結果 |
/// |---|---|---|
/// | 1 | ローカルの refresh token が失効 | 送らない（警告） |
/// | 2 | リモートに存在しない | 送る |
/// | 3 | リモートの状態が不明 かつ 稼働中インスタンスあり | 送らない |
/// | 4 | リモートの状態が不明 かつ 稼働中インスタンスなし | 送る |
/// | 5 | refresh token 指紋が一致 | 送らない（同一系列。リモートが自力更新できる） |
/// | 6 | リモートの refresh token が失効 | 送る（修復） |
/// | 7 | `remote.expires_at >= local.expires_at` | 送らない（後退防止） |
/// | 8 | それ以外（ローカルの方が新しい） | 送る（修復） |
pub fn decide(local: &AuthMeta, remote: &RemoteState, live_sibling: bool, now_ms: i64) -> Verdict {
    let deadline = now_ms + CLOCK_SKEW_MS;

    // 1) ローカルのリフレッシュトークンが失効している場合、送っても復帰できない。
    //    生きているリモートを壊すだけなので絶対に送らない。
    if local
        .refresh_token_expires_at
        .is_some_and(|t| t <= deadline)
    {
        return Verdict::skip_warn(
            "ローカルのリフレッシュトークンが失効しているため転送しない \
             (ローカルで `claude /login` を実行してください)",
        );
    }

    match remote {
        RemoteState::Missing => {
            if local.expires_at <= deadline {
                Verdict::send_warn(
                    "リモートに資格情報が無いため転送する \
                     (ローカルのアクセストークンは失効済み。リモート側で自動更新される想定)",
                )
            } else {
                Verdict::send("リモートに資格情報が無いため転送する")
            }
        }
        RemoteState::Unknown if live_sibling => Verdict::skip(
            "リモートの資格情報を確認できず、同一ホスト・プロファイルで \
             稼働中のインスタンスがあるため転送しない",
        ),
        RemoteState::Unknown => Verdict::send_warn(
            "リモートの資格情報を確認できないため転送する (稼働中インスタンスなし)",
        ),
        RemoteState::Present(r) => {
            // 5) 同じリフレッシュトークン系列ならリモートは自力で更新できる。
            //    走行中プロセスとのファイル競合を避けるため触らない。
            if r.refresh_fp == local.refresh_fp {
                return Verdict::skip(format!(
                    "リモートと同一のリフレッシュトークン (fp={}) のため転送しない",
                    r.refresh_fp
                ));
            }
            // 6) リモートのリフレッシュトークンが失効＝自力復帰不能。上書きで修復する。
            if r.refresh_token_expires_at.is_some_and(|t| t <= deadline) {
                return Verdict::send(format!(
                    "リモートのリフレッシュトークンが失効しているため転送する \
                     (remote fp={} → local fp={})",
                    r.refresh_fp, local.refresh_fp
                ));
            }
            // 7) 後退防止。リモートが同等以上に新しければ触らない（本命の修正）。
            if r.expires_at >= local.expires_at {
                return Verdict::skip(format!(
                    "リモートの資格情報の方が新しい (remote expires_at={} >= local={}, \
                     remote fp={} local fp={}) ため転送しない",
                    r.expires_at, local.expires_at, r.refresh_fp, local.refresh_fp
                ));
            }
            // 8) ローカルの方が新しい＝リモートのリフレッシュトークンはローテーション済みで
            //    死んでいる可能性が高い。上書きは修復として働く。
            Verdict::send(format!(
                "ローカルの資格情報の方が新しい (local expires_at={} > remote={}, \
                 remote fp={} → local fp={}) ため転送する",
                local.expires_at, r.expires_at, r.refresh_fp, local.refresh_fp
            ))
        }
    }
}

/// 現在時刻 (unix epoch ms)。
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_700_000_000_000;
    const HOUR: i64 = 3_600_000;
    const DAY: i64 = 24 * HOUR;

    fn meta(expires_in: i64, refresh_expires_in: Option<i64>, token: &str) -> AuthMeta {
        AuthMeta {
            expires_at: NOW + expires_in,
            refresh_token_expires_at: refresh_expires_in.map(|d| NOW + d),
            refresh_fp: fingerprint(token),
        }
    }

    #[test]
    fn parses_real_shaped_payload() {
        let json = br#"{
            "claudeAiOauth": {
                "accessToken": "dummy-access",
                "refreshToken": "dummy-refresh",
                "expiresAt": 1700000000000,
                "refreshTokenExpiresAt": 1710000000000,
                "scopes": ["user:inference", "user:profile"],
                "subscriptionType": "max",
                "rateLimitTier": "default"
            }
        }"#;
        let m = parse(json).unwrap();
        assert_eq!(m.expires_at, 1_700_000_000_000);
        assert_eq!(m.refresh_token_expires_at, Some(1_710_000_000_000));
        assert_eq!(m.refresh_fp.len(), 8);
        // 指紋にトークン本体が現れないこと
        assert!(!m.refresh_fp.contains("dummy"));
    }

    #[test]
    fn parse_tolerates_missing_refresh_token_expiry() {
        let json = br#"{"claudeAiOauth":{"accessToken":"a","refreshToken":"r","expiresAt":1}}"#;
        let m = parse(json).unwrap();
        assert_eq!(m.refresh_token_expires_at, None);
    }

    #[test]
    fn parse_rejects_garbage_and_empty() {
        assert!(parse(b"").is_err());
        assert!(parse(b"not json").is_err());
        assert!(parse(br#"{"claudeAiOauth":{}}"#).is_err());
        // 途中で切れたファイル (書き込みと読み込みが競合した場合を模擬)
        assert!(parse(br#"{"claudeAiOauth":{"accessToken":"a","refre"#).is_err());
    }

    #[test]
    fn expired_local_refresh_token_is_never_sent() {
        let local = meta(-HOUR, Some(-DAY), "old");
        for remote in [
            RemoteState::Missing,
            RemoteState::Unknown,
            RemoteState::Present(meta(HOUR, Some(30 * DAY), "new")),
        ] {
            let v = decide(&local, &remote, false, NOW);
            assert!(!v.send, "remote={remote:?} でも転送してはいけない");
            assert!(v.warn_user);
        }
    }

    #[test]
    fn missing_remote_receives_auth() {
        let local = meta(8 * HOUR, Some(30 * DAY), "tok");
        let v = decide(&local, &RemoteState::Missing, false, NOW);
        assert!(v.send);
        assert!(!v.warn_user);
    }

    #[test]
    fn missing_remote_receives_expired_access_token_with_warning() {
        // アクセストークンは失効しているがリフレッシュトークンは生きている。
        // リモート側で自動更新できるので送る（ユーザーへの警告つき）。
        let local = meta(-HOUR, Some(30 * DAY), "tok");
        let v = decide(&local, &RemoteState::Missing, false, NOW);
        assert!(v.send);
        assert!(v.warn_user);
    }

    #[test]
    fn newer_remote_is_not_overwritten() {
        // 本命のケース: リモートで既にトークンがローテーションされている
        let local = meta(2 * HOUR, Some(30 * DAY), "local-old");
        let remote = RemoteState::Present(meta(8 * HOUR, Some(30 * DAY), "remote-new"));
        let v = decide(&local, &remote, true, NOW);
        assert!(
            !v.send,
            "リモートの方が新しいのに上書きしている: {}",
            v.reason
        );
    }

    #[test]
    fn equally_fresh_remote_is_not_overwritten() {
        let local = meta(8 * HOUR, Some(30 * DAY), "a");
        let remote = RemoteState::Present(meta(8 * HOUR, Some(30 * DAY), "b"));
        assert!(!decide(&local, &remote, false, NOW).send);
    }

    #[test]
    fn identical_token_is_not_resent() {
        // 同じリフレッシュトークン系列。expires_at がローカルの方が新しくても、
        // リモートは自力で更新できるので触らない。
        let local = meta(8 * HOUR, Some(30 * DAY), "same");
        let remote = RemoteState::Present(meta(HOUR, Some(30 * DAY), "same"));
        let v = decide(&local, &remote, false, NOW);
        assert!(!v.send, "{}", v.reason);
    }

    #[test]
    fn older_remote_is_repaired() {
        let local = meta(8 * HOUR, Some(30 * DAY), "local-new");
        let remote = RemoteState::Present(meta(HOUR, Some(30 * DAY), "remote-old"));
        let v = decide(&local, &remote, false, NOW);
        assert!(v.send, "{}", v.reason);
    }

    #[test]
    fn remote_with_dead_refresh_token_is_repaired_even_if_access_token_looks_fresh() {
        // remote の expires_at は未来だが refresh token が失効している。
        // 放置するとアクセストークン失効時に復帰できないので上書きする。
        let local = meta(HOUR, Some(30 * DAY), "local");
        let remote = RemoteState::Present(meta(8 * HOUR, Some(-DAY), "remote"));
        let v = decide(&local, &remote, true, NOW);
        assert!(v.send, "{}", v.reason);
    }

    #[test]
    fn unknown_remote_respects_live_sibling() {
        let local = meta(8 * HOUR, Some(30 * DAY), "tok");
        // 稼働中インスタンスあり → 安全側に倒して送らない
        assert!(!decide(&local, &RemoteState::Unknown, true, NOW).send);
        // 稼働中インスタンスなし → 壊れたファイルを直せるよう送る
        assert!(decide(&local, &RemoteState::Unknown, false, NOW).send);
    }

    #[test]
    fn clock_skew_margin_blocks_almost_expired_refresh_token() {
        // 30 秒後に失効 → スキューマージン (60 秒) 内なので失効扱い
        let local = meta(HOUR, Some(30_000), "tok");
        assert!(!decide(&local, &RemoteState::Missing, false, NOW).send);
        // 5 分後に失効 → まだ有効
        let local = meta(HOUR, Some(5 * 60_000), "tok");
        assert!(decide(&local, &RemoteState::Missing, false, NOW).send);
    }

    #[test]
    fn fingerprint_is_stable_and_short() {
        assert_eq!(fingerprint("abc"), fingerprint("abc"));
        assert_ne!(fingerprint("abc"), fingerprint("abd"));
        assert_eq!(fingerprint("abc").len(), 8);
    }
}
