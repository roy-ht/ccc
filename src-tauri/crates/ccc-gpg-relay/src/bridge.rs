//! `--uplink`: stdio と制御 socket を繋ぐ薄いブリッジ。
//!
//! `ssh <host> ccc-gpg-relay --uplink` として起動され、ssh の stdio と
//! relay デーモンの制御 socket の間でバイト列をそのまま流すだけの役割を持つ。
//! フレームの解釈はデーモン側が行うため、ここにプロトコルの知識は無い。
//!
//! **stdout は明示的に flush する**。`io::stdout()` は LineWriter でバッファ
//! されるため、改行を含まないバイナリフレームが滞留して応答が返らなくなる。

use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::Path;

/// 1 回のコピーで扱うバイト数。
const COPY_CHUNK: usize = 32 * 1024;

/// ブリッジを実行する。どちらかの方向が閉じたら両方を畳んで戻る。
pub fn run(ctl_socket: &Path) -> io::Result<()> {
    let stream = UnixStream::connect(ctl_socket).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!(
                "relay デーモンの制御 socket に接続できません（{}）: {e}",
                ctl_socket.display()
            ),
        )
    })?;

    // socket → stdout
    let reader = stream.try_clone()?;
    let closer = stream.try_clone()?;
    let pump = std::thread::Builder::new()
        .name("ccc-gpg-bridge-out".into())
        .spawn(move || {
            let mut reader = reader;
            let mut stdout = io::stdout();
            let mut buf = vec![0u8; COPY_CHUNK];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if stdout.write_all(&buf[..n]).is_err() || stdout.flush().is_err() {
                            break;
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
            let _ = closer.shutdown(Shutdown::Both);
            // **ここで終わらないと ssh が生き残り、uplink 側は切断に気づけない。**
            // stdin（ssh から来る側）は相手が黙っていても EOF にならないため、
            // メインスレッドは永久にブロックする。relay 側が切れた時点で
            // ブリッジの役目は終わりなので、プロセスごと畳む
            std::process::exit(0);
        })?;

    // stdin → socket
    let mut writer = stream.try_clone()?;
    let mut stdin = io::stdin();
    let mut buf = vec![0u8; COPY_CHUNK];
    loop {
        match stdin.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if writer.write_all(&buf[..n]).is_err() {
                    break;
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }

    let _ = stream.shutdown(Shutdown::Both);
    let _ = pump.join();
    Ok(())
}
