# ccc (Claude Code Conductor)

コーディングエージェントを単一マシン上から監視・管理するデスクトップアプリケーション。

## 前提条件

| ツール | バージョン | 備考 |
|--------|-----------|------|
| [mise](https://mise.jdx.dev/) | - | Node.js / pnpm のバージョン管理 |
| Node.js | 24.x | `mise install` で自動インストール |
| pnpm | 10.x | 同上 |
| Rust | stable | [rustup](https://rustup.rs/) でインストール |
| [just](https://github.com/casey/just) | 1.x | タスクランナー |

Tauri 2.0 のシステム依存については [公式ガイド](https://v2.tauri.app/start/prerequisites/) を参照してください。

## セットアップ

```sh
# ランタイムをインストール（mise 利用時）
mise install

# 依存パッケージをインストール
just setup
```

## 開発

```sh
# Tauri 開発サーバー起動（フロント HMR + Rust ホットリロード）
just dev

# フロントエンドのみ起動（Vite dev server, ポート 1420）
just dev-front
```

## ビルド

```sh
# プロダクションビルド（アプリバンドル生成）
just build

# フロントエンドのみ
just build-front

# Rust バックエンドのみ
just build-rust
```

## チェック・テスト

```sh
# フロント + Rust の両方をチェック
just check

# TypeScript 型チェックのみ
just check-front

# Rust コンパイルチェックのみ
just check-rust

# Rust ユニットテスト
just test
```

## フォーマット・Lint

```sh
# Rust コードフォーマット
just fmt

# Rust Lint (clippy)
just clippy
```

## クリーン

```sh
# ビルド成果物を削除
just clean

# node_modules も含めて全削除
just clean-all
```

## 全レシピ一覧

```sh
just
```

## プロジェクト構成

```
src/                  フロントエンド (React + TypeScript + xterm.js)
src-tauri/            バックエンド (Rust + Tauri 2.0)
docs/                 ドキュメント
```

機能一覧は [`docs/features.md`](docs/features.md) を参照してください。
同梱 CLI については [`docs/ccc-sessions-quickstart.md`](docs/ccc-sessions-quickstart.md)
（アーカイブ操作）と [`docs/ccc-ssh.md`](docs/ccc-ssh.md)（ssh ラッパー）を参照。
