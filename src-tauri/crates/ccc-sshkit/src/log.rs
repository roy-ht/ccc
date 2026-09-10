//! ログ出力のコールバック型。
//!
//! sshkit は GUI（インスタンスの `.debug.txt`）と CLI（stderr / ファイル）の
//! 両方から使われ、出力先が呼び出し元で異なる。呼び出し側が閉包を渡す。

/// ログ出力コールバック。
pub type Log<'a> = &'a dyn Fn(&str);
