//! in-process FTS5 lindera トークナイザ。
//!
//! 外部 `lindera-sqlite`（ローダブル拡張）には依存しない。ccc が rusqlite と同じ
//! `libsqlite3-sys` にリンクしたまま、接続ごとに FTS5 カスタムトークナイザ
//! `lindera` を**プロセス内で登録**する。これにより dylib ロード時に起きていた
//! ABI スキューが消え、`load_extension` も不要になる（PoC で実証）。
//!
//! FTS5 カスタムトークナイザは DB に永続しないため、`messages_fts` を読む/書く
//! すべての接続で開いた直後に [`register`] を呼ぶ必要がある。

use std::ffi::{c_char, c_int, c_void};
use std::ptr;
use std::sync::OnceLock;

use lindera::dictionary::{load_embedded_dictionary, DictionaryKind};
use lindera::mode::Mode;
use lindera::segmenter::Segmenter;
use lindera::tokenizer::Tokenizer;
use rusqlite::{ffi, Connection};

// libsqlite3-sys が束縛していないため自前宣言（bundled SQLite に静的リンク済み）。
extern "C" {
    fn sqlite3_bind_pointer(
        stmt: *mut ffi::sqlite3_stmt,
        i: c_int,
        p: *mut c_void,
        t: *const c_char,
        d: Option<unsafe extern "C" fn(*mut c_void)>,
    ) -> c_int;
}

/// FTS5 トークナイザ名（`CREATE VIRTUAL TABLE ... tokenize='lindera'`）。
const TOKENIZER_NAME: &std::ffi::CStr = c"lindera";

/// IPADIC 辞書（実測 18MB）は接続ごとにロードすると重い。`Tokenizer::tokenize` は
/// 不変借用（`&self`）で内部状態を持たないため、プロセス全体で 1 インスタンスを共有する。
/// 初回呼び出しのときだけ辞書をロードし、以降は同じ参照を返す。
fn shared_tokenizer() -> anyhow::Result<&'static Tokenizer> {
    static TOKENIZER: OnceLock<Tokenizer> = OnceLock::new();
    if let Some(t) = TOKENIZER.get() {
        return Ok(t);
    }
    // OnceLock はロード失敗を保持できないため get_or_init は使わず、失敗時は次回再試行する。
    let dict = load_embedded_dictionary(DictionaryKind::IPADIC)
        .map_err(|e| anyhow::anyhow!("lindera IPADIC 辞書のロードに失敗: {e}"))?;
    let segmenter = Segmenter::new(Mode::Normal, dict, None);
    // 競合で他スレッドが先に set 済みなら、こちらの Tokenizer は破棄される（同一辞書なので無害）。
    let _ = TOKENIZER.set(Tokenizer::new(segmenter));
    Ok(TOKENIZER.get().expect("set 直後なので必ず存在する"))
}

/// 接続に lindera トークナイザを登録する。スキーマ適用（FTS5 表作成）より前に呼ぶこと。
pub fn register(conn: &Connection) -> anyhow::Result<()> {
    // 共有 Tokenizer への参照を SQLite に渡すだけで所有権は移さない（'static なので
    // 接続クローズで解放してはならない）。したがって x_destroy も登録しない。
    let tokenizer = shared_tokenizer()? as *const Tokenizer as *mut c_void;

    unsafe {
        let api = fts5_api_ptr(conn);
        if api.is_null() {
            anyhow::bail!("fts5_api を取得できません（bundled SQLite の FTS5 無効？）");
        }
        let mut tk = ffi::fts5_tokenizer {
            xCreate: Some(x_create),
            xDelete: Some(x_delete),
            xTokenize: Some(x_tokenize),
        };
        let create = (*api)
            .xCreateTokenizer
            .ok_or_else(|| anyhow::anyhow!("fts5_api.xCreateTokenizer が null"))?;
        let rc = create(api, TOKENIZER_NAME.as_ptr(), tokenizer, &mut tk, None);
        if rc != ffi::SQLITE_OK {
            anyhow::bail!("xCreateTokenizer 失敗 rc={rc}");
        }
    }
    Ok(())
}

/// `SELECT fts5(?1)` + bind_pointer で fts5_api ポインタを得る標準手法。
unsafe fn fts5_api_ptr(conn: &Connection) -> *mut ffi::fts5_api {
    let db = conn.handle();
    let mut stmt: *mut ffi::sqlite3_stmt = ptr::null_mut();
    let sql = c"SELECT fts5(?1)";
    if ffi::sqlite3_prepare_v2(db, sql.as_ptr(), -1, &mut stmt, ptr::null_mut()) != ffi::SQLITE_OK {
        return ptr::null_mut();
    }
    let mut api: *mut ffi::fts5_api = ptr::null_mut();
    let ty = c"fts5_api_ptr";
    sqlite3_bind_pointer(
        stmt,
        1,
        (&mut api as *mut *mut ffi::fts5_api).cast(),
        ty.as_ptr(),
        None,
    );
    ffi::sqlite3_step(stmt);
    ffi::sqlite3_finalize(stmt);
    api
}

/// xCreate: pUserData(=*mut Tokenizer) をそのままインスタンスとして返す（共有）。
unsafe extern "C" fn x_create(
    p_ctx: *mut c_void,
    _az_arg: *mut *const c_char,
    _n_arg: c_int,
    pp_out: *mut *mut ffi::Fts5Tokenizer,
) -> c_int {
    *pp_out = p_ctx.cast();
    ffi::SQLITE_OK
}

/// xDelete: インスタンスは 'static 共有 Tokenizer なので解放しない（x_destroy も未登録）。
unsafe extern "C" fn x_delete(_p: *mut ffi::Fts5Tokenizer) {}

/// xTokenize: lindera で分割し、各トークンを byte 範囲付きで FTS5 に渡す。
unsafe extern "C" fn x_tokenize(
    p_tokenizer: *mut ffi::Fts5Tokenizer,
    p_ctx: *mut c_void,
    _flags: c_int,
    p_text: *const c_char,
    n_text: c_int,
    x_token: Option<
        unsafe extern "C" fn(*mut c_void, c_int, *const c_char, c_int, c_int, c_int) -> c_int,
    >,
) -> c_int {
    let tokenizer = &*(p_tokenizer as *const Tokenizer);
    let Some(emit) = x_token else {
        return ffi::SQLITE_ERROR;
    };
    let bytes = std::slice::from_raw_parts(p_text as *const u8, n_text.max(0) as usize);
    let Ok(text) = std::str::from_utf8(bytes) else {
        return ffi::SQLITE_OK; // 不正 UTF-8 はトークン無しで成功扱い
    };
    let tokens = match tokenizer.tokenize(text) {
        Ok(t) => t,
        Err(_) => return ffi::SQLITE_ERROR,
    };
    for tok in tokens {
        let surf: &str = tok.surface.as_ref();
        if surf.is_empty() {
            continue;
        }
        let rc = emit(
            p_ctx,
            0,
            surf.as_ptr() as *const c_char,
            surf.len() as c_int,
            tok.byte_start as c_int,
            tok.byte_end as c_int,
        );
        if rc != ffi::SQLITE_OK {
            return rc;
        }
    }
    ffi::SQLITE_OK
}
