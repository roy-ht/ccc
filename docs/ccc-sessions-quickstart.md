# ccc-sessions クイックスタート

`ccc-sessions` は ccc が集約した Claude Code のセッション履歴・メモリを、検索・閲覧・
取り込み・復元する CLI。ccc 本体と同じローカル SQLite（`~/.ccc/archive/sessions.db`、
`CCC_DEV=1` 時は `~/.ccc/dev/archive/sessions.db`）を読み書きする。

機能概要は [`docs/features.md`](features.md) を参照。

## ビルド

```sh
cd src-tauri
cargo build -p ccc-sessions       # target/debug/ccc-sessions
```

ccc 本体（GUI）起動中はローカル履歴・メモリが継続的に最新化されるため、CLI は基本
読み出しだけで足りる。ccc 未起動時は `sync` で手動取り込みする。

## セッション

```sh
ccc-sessions sync                 # 全ローカルプロファイルの transcript＋メモリを取込
ccc-sessions sync --host <alias> --profile <p>   # リモートを rsync pull して取込（kind=remote）
ccc-sessions list                 # セッション一覧（新しい順）
ccc-sessions list --project ccc --since 7   # プロジェクト/直近7日で絞る
ccc-sessions search 設計           # 日本語全文検索（形態素＋BM25）。--json で機械可読
ccc-sessions show <id8>           # セッション要約表示（id は先頭8文字でOK）
ccc-sessions show <id8> --full    # 本文を省略せず表示。--raw で元 JSONL、--json で構造化
ccc-sessions recent --days 1      # 直近 N 日の活動を日付ごとにまとめる（振り返り用）
ccc-sessions stats                # 件数・期間・帰属/プロジェクト内訳の統計
```

`search` の出力は `<id8>#<seq>` 形式で、そのまま `show` に渡せる。
`recent`・`stats` とも `--json` で機械可読出力（外部連携向け）。

> `kind`（local/remote）と `attribution`（hook/inferred/host）は hook 由来のメタで、
> ccc 本体（GUI）経由で取り込まれたセッションにのみ入る。CLI `sync` だけで取り込んだ
> 履歴は両方 NULL（`stats` では `?` 表示）になる。

## リモート pull（揮発マシン対策）

リモート（EC2・devcontainer 等の短命マシン）の履歴・メモリを破棄前に回収する。
`~/.ccc/agent_settings/claude/<profile>/` を rsync でローカルのステージング
`~/.ccc/archive/pulled/<host>/<profile>/` に pull し、取り込む（`kind=remote`）。

```sh
ccc-sessions sync --host container-dev-host --profile max_plan   # 手動で全面 pull → 取込
```

- ccc 本体（GUI）起動中は自動で pull される: リモートの `Stop`/`SessionEnd` で当該セッションを
  狙い撃ち、活動があれば 60s 間隔で定期 sweep、切断/終了時に `projects/` 全体を sweep。
- 帰属は hook（`attribution=hook`）が正本。hook が無い孤児セッションは host+profile+cwd+活動時間窓
  から推定（`inferred`）、確信が持てなければ `host`（無名）に落とす。
- `.credentials.json` は pull 対象外。ssh は既存設定（ControlMaster）を再利用する。

## メモリ（CLAUDE.md / memory / rules）

メモリは内容ハッシュで重複排除されたバージョン付きで保存される（変化時だけ新版）。

```sh
ccc-sessions memory list                          # 最新版の一覧（U=user / P=project、版数 vN）
ccc-sessions memory list --scope user --json      # scope/project/profile で絞る
ccc-sessions memory show CLAUDE.md --profile max_plan          # 最新版の内容を表示
ccc-sessions memory show <rel_path> --version <hash8>          # 特定版（content-hash 前方一致）
ccc-sessions memory diff <rel_path> --profile max_plan         # 直近2版の行差分
ccc-sessions memory restore <rel_path> --profile max_plan      # 復元（確認のみ。実行は --yes）
ccc-sessions memory restore <rel_path> --profile max_plan --version <hash8> --yes
```

- `rel_path` は CLAUDE_CONFIG_DIR（`~/.ccc/agent_settings/claude/<profile>/`）からの相対パス。
  同名ファイルが複数プロファイルに存在する場合は `--profile` が必須。
- `restore` は過去版を CLAUDE_CONFIG_DIR に書き戻す。**上書き前に現状を自動でスナップショット**
  するため、巻き戻し前の内容も archive に残る（`.bak` ファイルは作らない）。
- 自動マージ（双方向同期）は v0.4 では行わない。手動復元のみ。

## 注意

- DB・メモリ内容はすべて `~/.ccc/` 配下のローカルデータ。`.credentials.json` は取込対象外。
- 接続は必ず `ccc_archive::open` を通る（FTS5 の lindera トークナイザは接続ごとに登録が要るため）。
- ノイズ除去ロジック等を変えてクリーンに反映したいときは DB を作り直す:
  `rm -f ~/.ccc/archive/sessions.db*` → `ccc-sessions sync`。
