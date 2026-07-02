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
| `ccc-ssh down <host>` | 安全な master 終了（`-O exit`。ゾンビを作る `-O stop` の代替） |
| `ccc-ssh heal <host>` | gpg agent forward チェック + 修復と台帳リプレイを即時実行 |

## pre-connect フック

`ccc-ssh <host> ...` で接続する際、`exec ssh` の直前に:

1. **世代ゲート**: 前回疎通確認済みの master pid とキャッシュを照合。
   pid 不変ならリモート実行ゼロで即 exec（common case は数 ms）
2. **gpg agent forward の健全性チェック**: `--no-autostart` 付き
   `getinfo socket_name` の応答分類で健全性を判定。不調時のみ修復
3. **forward 台帳のリプレイ**: master 世代交代を検知したら、台帳に登録済みの
   `-L` を全件冪等リプレイ

引数から接続先 alias を推定できない場合（複雑なオプション等）は
フックをスキップして透過実行します。

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
