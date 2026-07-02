# ccc (Claude Code Conductor) - 開発タスクランナー

# デフォルト: 利用可能なレシピ一覧を表示
default:
    @just --list

# --- セットアップ ---

# 依存パッケージをインストール
setup:
    pnpm install

# --- 開発 ---

# Tauri 開発サーバーを起動（フロント HMR + Rust ホットリロード）
# 開発版は settings.json と instances/ を ~/.ccc/dev/ 配下に分離する。
# agent_settings/ は ~/.ccc/ 直下を共有するため、CLAUDE_CONFIG_DIR が変わらず
# 配布版で取得した Claude 認証情報をそのまま使える。
dev: prepare-hook
    CCC_DEV=1 pnpm tauri dev

# フロントエンドのみの開発サーバーを起動（Vite）
dev-front:
    pnpm dev

# --- ビルド ---

# プロダクションビルド（フロント + Rust → アプリバンドル生成）
# リモート配信用に Linux 用 hook バイナリも同梱する
build: prepare-sidecar prepare-cli prepare-hook-all
    pnpm tauri build

# フロントエンドのみビルド（tsc + vite build）
build-front:
    pnpm build

# Rust バックエンドのみビルド (workspace 全体: ccc 本体 + ccc-claude-auth)
build-rust:
    cd src-tauri && cargo build --workspace

# sidecar バイナリ (ccc-claude-auth) のみビルド
build-sidecar:
    cd src-tauri && cargo build -p ccc-claude-auth

# sidecar を release ビルドし src-tauri/binaries/ccc-claude-auth-<triple> へ配置
# (tauri.conf.json の bundle.externalBin 用)
prepare-sidecar:
    #!/usr/bin/env bash
    set -euo pipefail
    cd src-tauri
    TRIPLE=$(rustc -vV | sed -n 's/host: //p')
    cargo build -p ccc-claude-auth --release
    mkdir -p binaries
    cp -f "target/release/ccc-claude-auth" "binaries/ccc-claude-auth-${TRIPLE}"
    echo "sidecar: binaries/ccc-claude-auth-${TRIPLE}"

# ccc-sessions CLI を release ビルドし src-tauri/binaries/ccc-sessions-<triple> へ配置
# (tauri.conf.json の bundle.externalBin 用。アプリ内「CLIインストール」で symlink される)
prepare-cli:
    #!/usr/bin/env bash
    set -euo pipefail
    cd src-tauri
    TRIPLE=$(rustc -vV | sed -n 's/host: //p')
    cargo build -p ccc-sessions -p ccc-ssh --release
    mkdir -p binaries
    cp -f "target/release/ccc-sessions" "binaries/ccc-sessions-${TRIPLE}"
    cp -f "target/release/ccc-ssh" "binaries/ccc-ssh-${TRIPLE}"
    echo "cli: binaries/ccc-sessions-${TRIPLE}, binaries/ccc-ssh-${TRIPLE}"

# ccc-claude-code-hook をホスト用にビルドし
# src-tauri/binaries/ccc-claude-code-hook/<platform>/ccc-claude-code-hook へ配置。
prepare-hook:
    #!/usr/bin/env bash
    set -euo pipefail
    cd src-tauri
    case "$(uname -sm)" in
      "Darwin arm64")  PLATFORM=darwin-arm64 ;;
      "Linux x86_64")  PLATFORM=linux-amd64 ;;
      "Linux aarch64") PLATFORM=linux-arm64 ;;
      "Linux arm64")   PLATFORM=linux-arm64 ;;
      *) echo "unsupported host: $(uname -sm)" >&2; exit 1 ;;
    esac
    cargo build -p ccc-claude-code-hook --release
    mkdir -p "binaries/ccc-claude-code-hook/${PLATFORM}"
    cp -f "target/release/ccc-claude-code-hook" \
       "binaries/ccc-claude-code-hook/${PLATFORM}/ccc-claude-code-hook"
    echo "hook bin: binaries/ccc-claude-code-hook/${PLATFORM}/ccc-claude-code-hook"

# 全プラットフォーム (darwin-arm64, linux-amd64, linux-arm64) 用 hook バイナリを配置。
# Linux 向けは musl による静的リンクで、リモート側の glibc バージョンに依存しない。
# 事前に `just setup-cross` を一度実行してツールチェーンを揃えておくこと。
prepare-hook-all: prepare-hook
    #!/usr/bin/env bash
    set -euo pipefail
    cd src-tauri
    for entry in \
        "aarch64-unknown-linux-musl:linux-arm64" \
        "x86_64-unknown-linux-musl:linux-amd64"; do
        triple="${entry%:*}"
        platform="${entry#*:}"
        cargo zigbuild -p ccc-claude-code-hook --release --target "$triple"
        mkdir -p "binaries/ccc-claude-code-hook/${platform}"
        cp -f "target/${triple}/release/ccc-claude-code-hook" \
              "binaries/ccc-claude-code-hook/${platform}/ccc-claude-code-hook"
        echo "hook bin: binaries/ccc-claude-code-hook/${platform}/ccc-claude-code-hook"
    done

# クロスコンパイル用ツールチェーンを揃える（ホスト = macOS arm64 想定、初回のみ実行）。
# 必須: zig, cargo-zigbuild がローカルにインストール済みであること。
#   brew install zig
#   cargo install cargo-zigbuild
setup-cross:
    rustup target add aarch64-unknown-linux-musl x86_64-unknown-linux-musl
    @command -v zig >/dev/null || (echo "zig が未インストール: brew install zig" >&2; exit 1)
    @command -v cargo-zigbuild >/dev/null || (echo "cargo-zigbuild が未インストール: cargo install cargo-zigbuild" >&2; exit 1)
    @echo "OK: クロスコンパイル環境が揃いました"

# --- チェック・テスト ---

# フロント + Rust の両方をチェック
check: check-front check-rust

# TypeScript 型チェック
check-front:
    npx tsc --noEmit

# Rust コンパイルチェック（ビルドより高速）
check-rust:
    cd src-tauri && cargo check --workspace

# Rust ユニットテスト実行
test:
    cd src-tauri && cargo test --workspace

# --- フォーマット・Lint ---

# Rust コードフォーマット
fmt:
    cd src-tauri && cargo fmt --all

# Rust コードフォーマットチェック（CI 用）
fmt-check:
    cd src-tauri && cargo fmt --all -- --check

# Rust Lint
clippy:
    cd src-tauri && cargo clippy --workspace -- -D warnings

# --- クリーン ---

# ビルド成果物を削除
clean:
    rm -rf dist
    cd src-tauri && cargo clean

# node_modules も含めて全削除
clean-all: clean
    rm -rf node_modules
