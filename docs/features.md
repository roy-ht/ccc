# 機能一覧

ccc (Claude Code Conductor) は、Claude Code などのコーディングエージェントを
デスクトップアプリから一元的に監視・操作するためのツールです。ローカル / SSH 経由の
リモート両方を同じ UI で扱い、履歴・メモリ・状態を横断的に管理できます。

以下は主要機能の概観です。

---

## 1. マルチインスタンス管理

「インスタンス」= 1 つの接続とコーディングエージェントのセット。左サイドバーに
複数インスタンスが並び、切り替えながら並行作業できます。

- **ローカル**: `portable-pty` でシェルを起動し、その上で `claude` などを実行
- **リモート**: `~/.ssh/config` の Host エントリから選び、SSH 接続の上に
  `tmux` セッションを立てて永続化する
- **状態バッジ**: エージェントの状態（idle / busy / permission 待ち / plan 承認待ち /
  切断）をサイドバーに常時表示
- **手動再接続**: SSH 切断後もリモート側の `tmux` セッションは残り、
  「再接続」ボタンで既存セッションに reattach

## 2. Claude Code フックによる状態検知

ターミナル出力のパースではなく、Claude Code 公式の hook 機構を利用して
構造化された状態情報を取得します。

- インスタンス起動時に `~/.claude/settings.json` へ hook 定義を冪等 merge
  （既存の他用途 hook は温存）
- リモートは `ssh -R` で reverse forward した localhost に POST
- 状態変化・permission ダイアログ・plan 承認待ち・最終メッセージなどを
  イベント駆動で反映
- 独自トークンでポート認証、非 ccc 環境では hook はランタイム no-op

## 3. セッションアーカイブと日本語全文検索

各インスタンスの Claude Code transcript (JSONL) を単一の SQLite に集約し、
横断的に検索・閲覧できます。

- **ストレージ**: `~/.ccc/archive/sessions.db`（SQLite + WAL）
- **日本語 FTS5**: `lindera`（IPADIC 埋込）による形態素解析。
  外部拡張なしで in-process 登録
- **増分取り込み**: transcript を「真実の源」とし、ファイル末尾の未読バイトだけを
  読む冪等インジェスト（`uuid` 重複排除）
- **リモート回収**: 揮発マシン (EC2 / devcontainer 等) の履歴を破棄前に
  rsync pull で回収。hook 活動をゲートに定期的に狙い撃ち sync
- **帰属推定**: hook 由来の正確な帰属を最優先、無ければ
  host+profile+cwd+活動窓からスコアリングで推定
- **UI**: サイドバー選択中インスタンスの Sessions タブから検索・閲覧。
  Markdown レンダリング・System 行表示切替・長ターンの中間省略対応

### 圧縮とサイズ最適化

- transcript 生行 (`messages.raw`) と hook payload を **zstd 圧縮**して BLOB 保存
- 反復の多い JSON 構造は **共有 zstd 辞書** でさらに圧縮
- 暗号化 thinking ブロック（本文なし）は取り込み時に除去して
  無駄な圧縮対象を減らす

実測: 40 日分 521MB のアーカイブが辞書付き 253MB に半減 (-51%)。

## 4. メモリのバージョン管理

`CLAUDE.md` / `MEMORY.md` / `rules/` / `projects/*/memory/` を内容ハッシュで
重複排除しながらバージョン付きスナップショット保存します。

- **契機**: 起動時フルスキャン / `SessionEnd` / リモート pull 後
- **CLI 復元**: `ccc-sessions memory list` / `show` / `diff` / `restore`
  で過去版に戻せる（上書き前に現状を自動スナップショット）
- **リモート**: 同じフォーマットで pull → 保存され、揮発マシンでの
  メモリ蓄積を失わない

## 5. Explorer (ファイルブラウザ)

いま作業中のディレクトリを ccc 内でツリー表示・プレビュー・全文検索できます。

- ローカル / リモート (SSH 経由) を同じ UI で扱う抽象化
- ツリー左ペインは遅延ロード、展開状態は `localStorage` に永続化
- プレビュー: Text (syntax highlight) / Markdown (source / preview トグル) /
  画像 / PDF に対応
- 全文検索は **ripgrep 呼び出し**、リモートは ControlMaster を再利用した
  `ssh -T <alias> 'rg ...'`
- OS のファイルマネージャからのドラッグ & ドロップで、ローカル → ローカル
  (`cp -aR`) / ローカル → リモート (`rsync -az`) のコピー
- パストラバーサル防止 (`path_guard`)

## 6. Port Forwarding 管理

ControlMaster が張っている `-L` の forward を UI から一覧・追加・削除できます。

- SSH には forward 列挙手段が無いので、**ccc 台帳 + `ssh -G` config + hook 予約**
  を合成
- master 再起動で消えた forward は世代交代を検知し、
  台帳を全件 `-O forward` で自動リプレイ
- ゾンビ master 保持中のポート未解放も UI に可視化

## 7. GPG agent forward の自動修復

リモートに gpg-agent socket を forward している環境で、master 異常終了による
socket 残骸を検知して自動修復します。

- `gpg-connect-agent --no-autostart 'getinfo socket_name'` の応答分類で
  「Forbidden = 正常」「D パス = 誤起動 agent」を判別
- 不調時は残骸掃除 + `-O forward` で mux 経由の再要求 → 再チェック
- master 世代 pid をゲートに、通常は数 ms のチェックだけでスキップ

## 8. コマンド終了とセッション保全

`tmux` の `remain-on-exit` + `pane-died` フックで、エージェントコマンドが
異常終了してもセッションと最終エラー出力を保全します。

- 従来: エージェント死亡 → tmux セッションごと消滅 → エラー消失
- 現行: pane が dead で残り、Agent タブから最終出力を読める
- ccc に即時通知が届き、サイドバーは `Terminated (exit N)` を表示
- クローズ操作時に best-effort で dead セッションを掃除

## 9. ウォッチドッグと画面ベースの状態補正

hook が完全に届かないケース（Esc 中断 / hook 欠落 / ハング）に対する
2 層の補正機構があります。

- **ウォッチドッグ**: busy 中の単独 Esc 検知 → 2.5 秒後に transcript 末尾を
  確認し、中断マーカーがあれば idle に確定
- **シャドウスクリーン**: PTY バイト列を headless の vt100 に流し、
  スピナー / permission ダイアログ / 実選択肢を検出。hook 沈黙 5 秒以上で
  安定シグナル + 同一シグナル 2 回連続時のみ補正
- 送信側 hook に `sent_at_us` を付与し、並行 POST の適用順序逆転をガード

## 10. `ccc-ssh`: ssh ラッパー CLI

ccc GUI の外（ターミナルからの素の ssh 利用）でも同じ運用機能を使えるように、
`ccc-ssh` を同梱しています。詳細は `docs/ccc-ssh.md` を参照。

- 素の ssh と完全併用可能 (同じ config・同じ ControlMaster を共有)
- `fwd list/add/rm` で forward 管理、`heal` で gpg 修復、
  `down` でゾンビを作らない master 停止

## 11. `ccc-sessions`: アーカイブ操作 CLI

集約済みのセッション / メモリを CLI から検索・閲覧・取り込みできます。
詳細は `docs/ccc-sessions-quickstart.md` を参照。

- `list` / `search` / `show` / `recent` / `stats` の閲覧系
- `sync` で手動取り込み（ccc GUI 未起動時に使う）
- `memory list/show/diff/restore` でメモリの版管理
- ANSI 整形出力（人間向け）と `--json` 出力（機械可読）

---

## 用語

| 用語 | 意味 |
|---|---|
| インスタンス | ccc の基本単位。1 接続 + 1 コーディングエージェント |
| プロファイル | Claude Code の `CLAUDE_CONFIG_DIR` 単位。認証・設定・履歴が独立する |
| SSOT | ローカルの `~/.ccc/agent_settings/claude/<profile>/` を各インスタンスにコピー配布する構成 |
| hook | Claude Code 公式のイベント通知機構。ccc は wrapper 経由で HTTP POST に変換 |
| ControlMaster | OpenSSH の接続多重化機構。ccc は既存の master に相乗りする |

## 対応プラットフォーム

- macOS (Tier 1 サポート)
- Linux (Tier 2 サポート)
- Windows (Tier 3 サポート、未検証)

## ライセンス

Apache License 2.0
