//! `messages.raw` / `events.payload` 等の透過 zstd 圧縮。
//!
//! schema v2 で `raw_zstd` / `payload_zstd` BLOB に切り替えるための encode/decode。
//! 旧 TEXT 列との互換のため、`decode_blob_to_string` は zstd マジック
//! (4 バイト 0x28 0xB5 0x2F 0xFD, little endian) を見て生 UTF-8 もそのまま受ける。
//!
//! - 圧縮レベル: 3（zstd 既定。1KB 級 JSON で 1ms 未満）
//! - 1 行ごとに独立 frame
//! - schema v3 から共有辞書（[`crate::dicts`]）にも対応。`*_with_dict` 系を使う

use zstd::dict::{DecoderDictionary, EncoderDictionary};

/// zstd の magic number（little endian の最初の 4 バイト）。
pub(crate) const ZSTD_MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];

/// 既定の圧縮レベル。チューニングしたい場合はここを変える。
pub(crate) const LEVEL: i32 = 3;

/// 文字列を zstd フレームに圧縮する。
pub fn encode_str(s: &str) -> anyhow::Result<Vec<u8>> {
    let out = zstd::bulk::compress(s.as_bytes(), LEVEL)?;
    Ok(out)
}

/// BLOB から UTF-8 文字列を復元する。zstd フレームなら展開、そうでなければ
/// 生 UTF-8 とみなしてそのまま返す（schema v1 で書かれた旧データとの互換）。
pub fn decode_blob_to_string(b: &[u8]) -> anyhow::Result<String> {
    if b.len() >= 4 && b[..4] == ZSTD_MAGIC {
        let bytes = zstd::bulk::decompress(b, max_decompress_size(b.len()))?;
        Ok(String::from_utf8(bytes)?)
    } else {
        Ok(std::str::from_utf8(b)?.to_string())
    }
}

/// 展開後サイズの上限見積もり。fuzz/破損データでメモリ暴発しないための保険。
/// 実用では `messages.raw` の最大が 2.3MB なので 32MB あれば十分。
fn max_decompress_size(compressed_len: usize) -> usize {
    // 単一行で圧縮率 50x を超えることはまずないが、安全側に倍率と固定上限を併用。
    const RATIO: usize = 64;
    const HARD_CAP: usize = 32 * 1024 * 1024;
    (compressed_len.saturating_mul(RATIO)).clamp(64 * 1024, HARD_CAP)
}

/// 共有辞書を使って文字列を zstd フレームに圧縮する。
/// 辞書は `EncoderDictionary::copy(blob, LEVEL)` で事前準備したものを渡す（[`crate::dicts`]）。
pub fn encode_str_with_dict(s: &str, dict: &EncoderDictionary<'static>) -> anyhow::Result<Vec<u8>> {
    let mut c = zstd::bulk::Compressor::with_prepared_dictionary(dict)?;
    let out = c.compress(s.as_bytes())?;
    Ok(out)
}

/// 共有辞書を使って BLOB を文字列に復元する。`decode_blob_to_string` と同様に
/// 旧 TEXT データ（zstd フレームでない）はそのまま返す。
pub fn decode_blob_to_string_with_dict(
    b: &[u8],
    dict: &DecoderDictionary<'static>,
) -> anyhow::Result<String> {
    if b.len() >= 4 && b[..4] == ZSTD_MAGIC {
        let mut d = zstd::bulk::Decompressor::with_prepared_dictionary(dict)?;
        let bytes = d.decompress(b, max_decompress_size(b.len()))?;
        Ok(String::from_utf8(bytes)?)
    } else {
        Ok(std::str::from_utf8(b)?.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_str() {
        let s = "設計のレビューを会議で決めた".repeat(20);
        let z = encode_str(&s).unwrap();
        assert!(z.len() < s.len(), "繰り返し JSON は zstd で必ず縮む");
        assert_eq!(decode_blob_to_string(&z).unwrap(), s);
    }

    #[test]
    fn legacy_plain_utf8_is_returned_as_is() {
        // schema v1 で書かれた素のテキストも復元できる（移行期間の互換）。
        let s = b"plain utf-8 line\n";
        assert_eq!(decode_blob_to_string(s).unwrap(), "plain utf-8 line\n");
    }

    #[test]
    fn empty_string_roundtrips() {
        let z = encode_str("").unwrap();
        // 空でも有効な zstd フレームになる。
        assert_eq!(&z[..4], &ZSTD_MAGIC);
        assert_eq!(decode_blob_to_string(&z).unwrap(), "");
    }

    #[test]
    fn dict_roundtrip() {
        // 学習データと評価データを分けて辞書を作り、roundtrip が成立すること。
        let samples: Vec<String> = (0..20)
            .map(|i| {
                format!(
                    r#"{{"type":"assistant","sessionId":"s{i:02}","message":{{"content":[{{"type":"text","text":"sample {i}"}}]}}}}"#
                )
            })
            .collect();
        let bufs: Vec<&[u8]> = samples.iter().map(|s| s.as_bytes()).collect();
        let dict_blob = zstd::dict::from_samples(&bufs, 4096).unwrap();
        let enc = EncoderDictionary::copy(&dict_blob, LEVEL);
        let dec = DecoderDictionary::copy(&dict_blob);

        let target = r#"{"type":"assistant","sessionId":"sXX","message":{"content":[{"type":"text","text":"target"}]}}"#;
        let z = encode_str_with_dict(target, &enc).unwrap();
        let back = decode_blob_to_string_with_dict(&z, &dec).unwrap();
        assert_eq!(back, target);
    }

    #[test]
    fn json_like_payload_compresses_well() {
        // 実物に近い JSONL を 1 行模倣（キー反復で zstd が効きやすい想定の検証）。
        let line = serde_json::json!({
            "type": "assistant",
            "sessionId": "01J9F0XYZABCDE",
            "message": { "content": [
                {"type": "text", "text": "okay, here's the plan: ".to_string() + &"a".repeat(500)},
                {"type": "tool_use", "name": "Edit", "input": {"file_path": "/a/b/c.rs"}}
            ]}
        })
        .to_string();
        let z = encode_str(&line).unwrap();
        assert!(z.len() * 3 < line.len(), "代表的な JSONL は 1/3 以下に縮む");
        assert_eq!(decode_blob_to_string(&z).unwrap(), line);
    }
}
