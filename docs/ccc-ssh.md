# ccc-ssh: ssh ラッパー CLI

`ccc-ssh` は ccc に同梱される ssh のラッパー CLI で、ccc GUI の外
（普段のターミナル利用時）にも同じ運用機能を提供します。素の `ssh` と
完全に併用できる設計です。

## インストール

ccc アプリの「設定 > ツール」から「CLI インストール」を実行すると、
`~/.local/bin/ccc-ssh` に symlink が張られます。`~/.local/bin` を PATH に
追加してください。

## 設計原則: 素の ssh と完全に併用可能

`ccc-ssh` は独自の接続系を持ちません。素の `ssh` と **同じ config・同じ
ControlMaster・同じソケット** に相乗りし、追加で行うのは冪等な mux コマンドと
台帳の読み書きだけです。

- `ccc-ssh` を使わずに素の `ssh` だけを使った場合は従来挙動に戻るだけ
- 素の `ssh` と混ぜても壊れない
- 唯一の運用ルール: **ccc 台帳に載せた forward の削除は ccc 側（GUI または
  `ccc-ssh fwd rm`）で行う**（生の `ssh -O cancel` で消しても台帳が残り、
  次の master 世代交代でリプレイが復活させる）

## コマンド

| コマンド | 動作 |
|---|---|
| `ccc-ssh <ssh引数...>` | pre-connect フック実行後、`exec ssh <引数...>` で完全透過 |
| `ccc-ssh fwd list <host>` | forward 一覧（GUI の Forwards タブと同じ合成: 台帳+config） |
| `ccc-ssh fwd add <host> <listen>:<host>:<port>` | `-L` 形式で forward 追加 + 台帳記録 |
| `ccc-ssh fwd rm <host> <listen_port>` | ccc 台帳の forward を削除 |
| `ccc-ssh down <host>` | 安全な master 終了（`-O exit`。無応答なら kill フォールバック） |
| `ccc-ssh heal <host>` | master 死活診断 + 台帳リプレイ + gpg uplink の張り直し |
| `ccc-ssh gpg <サブコマンド>` | gpg agent forward（relay 方式）。下記参照 |

## pre-connect フック

`ccc-ssh <host> ...` で接続する際、`exec ssh` の直前に:

1. **master 死活プローブ**: 網断で half-open（`-O check` は成功するが実通信は
   永遠に返らない）になった master を検知したら、自動で畳んで再確立する
   （ユーザー ControlMaster 設定時は `ssh -N -f` で復旧し、config の
   RemoteForward = gpg forward も復活する）。全段タイムアウト付きなので
   フックが固まることはない
2. **世代ゲート**: 前回疎通確認済みの master pid とキャッシュを照合。
   pid 不変ならリモート実行ゼロで即 exec（common case は数 ms）
3. **gpg relay の uplink 確認**: 常駐 uplink が居ることを保証する
   （flock を試すだけで数 ms、リモート実行なし）
4. **forward 台帳のリプレイ**: master 世代交代を検知したら、台帳に登録済みの
   `-L` を全件冪等リプレイ

引数から接続先 alias を推定できない場合（複雑なオプション等）は
フックをスキップして透過実行します。

## 推奨 ssh 設定（ネットワーク断への耐性）

自分の `~/.ssh/config` で ControlMaster を管理しているホストには、以下を
設定しておくと網断時に master が自滅してクリーンに再確立できます
（未設定だと TCP keepalive 頼みで死活検知まで数十分かかる）:

```ssh-config
Host mybox
    ControlMaster auto
    ControlPath ~/.ssh/cm-%C
    ControlPersist 30
    ServerAliveInterval 15
    ServerAliveCountMax 3
    ExitOnForwardFailure yes
```

## gpg agent forward（relay 方式）

v0.14 で、ssh の `RemoteForward` に相乗りする方式をやめました。リモートに常駐する
`ccc-gpg-relay` が `~/.gnupg/S.gpg-agent` を**所有し続け**、ローカルの uplink が
ssh の stdio 1 本で多重化して繋ぎます。ssh 接続の生死と socket ファイルの寿命が
分離されるため、「接続は健全なのに gpg が使えない」状態が構造的に起きません。
設計の詳細は `specs/v0.14-gpg-relay.md`。

| コマンド | 動作 |
|---|---|
| `ccc-ssh gpg enable <host>` | このホストで relay を有効にする |
| `ccc-ssh gpg disable <host>` | 無効にする（uplink が動いていれば停止） |
| `ccc-ssh gpg up <host>` | uplink を起動（既に居れば何もしない） |
| `ccc-ssh gpg down <host>` | uplink を停止 |
| `ccc-ssh gpg status [<host>]` | 状態を表示（省略時は有効な全ホスト） |
| `ccc-ssh gpg list` | 設定済みホストの一覧 |
| `ccc-ssh gpg doctor <host>` | 移行漏れ・前提条件を診断 |

### 設定

`~/.ccc/gpg.json`（`CCC_DEV=1` なら `~/.ccc/dev/gpg.json`）に全ホスト分を持ちます。

```json
{
  "schema": 1,
  "defaults": { "grace_secs": 5 },
  "hosts": {
    "mybox": { "enabled": true },
    "container-host": {
      "enabled": true,
      "remote_socket": "/run/user/1000/gnupg/S.gpg-agent"
    }
  }
}
```

省略した項目は `defaults` → 組み込み既定の順に解決されるので、通常は
`{"enabled": true}` だけで足ります（`ccc-ssh gpg enable` が書きます）。

### 移行時の必須手順

**`~/.ssh/config` から gpg 用の `RemoteForward` 行を削除してください。**
残っていると、素の `ssh` で接続した瞬間に sshd が relay の socket を上書きします。
`ccc-ssh gpg doctor <host>` が検出して警告します。

リモートの `sshd_config` に `AllowStreamLocalForwarding no` を入れておくと、
クライアント側の設定ミスに関係なく socket が守られます。relay 方式では
streamlocal forward を一切使わないため、無効化しても副作用はありません
（`ForwardAgent` は別の設定項目、`-L`/`-R` の TCP forward も無関係）。

リモートの `~/.gnupg/gpg.conf` に `no-autostart` を入れておくと、鍵を持たない
gpg-agent が socket を奪う事故を防げます。

## 使用例

```sh
# 素の ssh と同じ感覚で接続。裏で世代チェック + 必要なら修復
ccc-ssh mybox

# forward 一覧
ccc-ssh fwd list mybox

# ローカル 8080 を リモート webapp:80 にトンネル
ccc-ssh fwd add mybox 8080:webapp:80

# ccc 追加分の forward を削除
ccc-ssh fwd rm mybox 8080

# master を安全に停止（ゾンビ回避）
ccc-ssh down mybox

# gpg agent forward を即時チェック + 修復
ccc-ssh heal mybox
```

## 非スコープ

- `scp` / `rsync` / `git` など `ssh` を直接 exec するツールへのフック適用
  （修復は次の `ccc-ssh` / ccc GUI 操作時に走る）
- `ssh` の全オプションの完全解釈（value を取る主要オプションのみ対応し、
  解釈不能なら透過実行にフォールバック）
- 1 リモートホストにつき gpg socket は 1 本。複数マシンの ccc から同時に繋いだ
  場合は後勝ちになり、先客の gpg 操作は切れる
