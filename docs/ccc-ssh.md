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
| `ccc-ssh heal <host>` | master 死活診断 + gpg agent forward 修復と台帳リプレイを即時実行 |

## pre-connect フック

`ccc-ssh <host> ...` で接続する際、`exec ssh` の直前に:

1. **master 死活プローブ**: 網断で half-open（`-O check` は成功するが実通信は
   永遠に返らない）になった master を検知したら、自動で畳んで再確立する
   （ユーザー ControlMaster 設定時は `ssh -N -f` で復旧し、config の
   RemoteForward = gpg forward も復活する）。全段タイムアウト付きなので
   フックが固まることはない
2. **世代ゲート**: 前回疎通確認済みの master pid とキャッシュを照合。
   pid 不変ならリモート実行ゼロで即 exec（common case は数 ms）
3. **gpg agent forward の健全性チェック**: `--no-autostart` 付き
   `getinfo socket_name` の応答分類で健全性を判定。不調時のみ修復
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

gpg agent forward（RemoteForward の unix socket）を使うホストでは、リモートの
`sshd_config` に `StreamLocalBindUnlink yes` を入れると残骸ソケットによる
bind 失敗が根本的に消えます（サーバ側にしか効かない設定）。

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
