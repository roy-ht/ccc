mod agent_socket;
mod archive_commands;
mod archive_service;
mod cli_install;
mod commands;
mod env_setup;
mod explorer;
mod forwards;
mod hook_receiver;
mod hook_setup;
mod instance;
mod paths;
mod settings;
mod single_instance;
mod ssh_config;

use hook_receiver::HookReceiver;
use instance::InstanceManager;
use tauri::Manager;
// hook_setup は wrapper / settings_json merge / binary install で個別利用するため
// モジュールパスで呼び出す（use 個別 import より変更耐性が高い）

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // GUI 起動時 (Finder/Dock) の最小 PATH では tmux などを解決できず、
    // local_tmux_session_exists が偽陰性を返してインスタンスが Terminated
    // 扱いになる事故が起きる。Builder 起動前に PATH を整える。
    env_setup::inherit_login_shell_path();

    // dev/release の二重起動を禁止する（共有の hook ラッパー
    // `~/.ccc/bin/ccc-hook.sh` を起動時に互いに上書きし合い、先に起動して
    // いた側のセッション集約が壊れるため）。ロックは run() の生存中保持する。
    let _instance_lock = match single_instance::try_acquire() {
        Ok(single_instance::LockOutcome::Acquired(lock)) => Some(lock),
        Ok(single_instance::LockOutcome::Held(holder)) => single_instance::abort_duplicate(&holder),
        Err(e) => {
            // ロックファイル自体を用意できない異常環境でも起動は止めない。
            eprintln!("[ccc] 単一インスタンスロックの取得に失敗（続行）: {e}");
            None
        }
    };

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .manage(InstanceManager::new())
        .setup(|app| {
            if let Err(e) = paths::ensure_layout() {
                eprintln!("[ccc] ~/.ccc/ レイアウトの初期化に失敗: {e}");
            }

            // 同期ステータスライン用の共有クロック（最終同期 unix ms）。
            let sync_clock: archive_service::LastSyncAt =
                std::sync::Arc::new(std::sync::atomic::AtomicI64::new(0));

            // セッション/メモリ集約サービスを起動し manager に注入する。
            // 失敗しても本体起動は止めず、集約のみ無効化する。
            let infos_for_archive = app.state::<InstanceManager>().infos_handle();
            match paths::archive_db_path().and_then(|p| {
                archive_service::ArchiveService::start(
                    p,
                    Some(app.handle().clone()),
                    sync_clock.clone(),
                    infos_for_archive,
                )
            }) {
                Ok(svc) => {
                    // 起動時に全ローカルプロファイルのメモリを 1 回保全する
                    // （ccc 未起動中に変わったメモリの取りこぼし回収）。
                    // transcript も同様にフルスキャンし、hook が届かなかった期間の
                    // セッションをバックフィルする（増分カーソルで 2 回目以降は安価）。
                    if let Ok(claude_root) = paths::agent_settings_dir().map(|d| d.join("claude")) {
                        svc.scan_memory(claude_root.clone());
                        svc.scan_transcripts(claude_root);
                    }
                    app.state::<InstanceManager>().set_archive(svc);
                    eprintln!("[ccc] archive サービス起動");
                }
                Err(e) => eprintln!("[ccc] archive サービス起動に失敗（集約は無効）: {e}"),
            }

            // UI（Sessions / Memories 画面）向けの読み取り専用接続をキャッシュして
            // managed state に載せる。writer とは別接続で、WAL により同時並行に読める。
            let archive_db = paths::archive_db_path()
                .map(|p| archive_commands::ArchiveDb::open(&p))
                .unwrap_or_else(|_| archive_commands::ArchiveDb::none());
            app.manage(archive_db);

            // 同期ステータス問い合わせ用に共有クロックを managed state に載せる。
            app.manage(archive_commands::ArchiveSyncClock(sync_clock));

            // hook バイナリをローカル `~/.ccc/bin/` に配信する。
            match hook_setup::binary::install_local() {
                Ok(true) => eprintln!("[ccc] hook バイナリを更新しました"),
                Ok(false) => {}
                Err(e) => eprintln!("[ccc] hook バイナリ配信に失敗: {e}"),
            }
            // 既存の全 Claude プロファイル（`~/.ccc/agent_settings/claude/*/`）の
            // settings.json に hook 定義を冪等マージする。restore 経由で
            // local_claude_config_dir を通らないインスタンスもこれでカバーされる。
            // 新規プロファイルは create_local 時の local_claude_config_dir 内で merge される。
            match hook_setup::merge_settings_for_all_profiles() {
                Ok(updated) if !updated.is_empty() => {
                    eprintln!(
                        "[ccc] hook 定義を追加したプロファイル: {}",
                        updated.join(", ")
                    )
                }
                Ok(_) => {}
                Err(e) => eprintln!("[ccc] 全プロファイル hook merge に失敗: {e}"),
            }

            #[cfg(debug_assertions)]
            {
                use tauri::menu::{MenuItem, Submenu};
                let open_devtools =
                    MenuItem::with_id(app, "open-devtools", "DevToolsを開く", true, None::<&str>)?;
                let debug_menu = Submenu::with_items(app, "Debug", true, &[&open_devtools])?;
                if let Some(menu) = app.menu() {
                    menu.append(&debug_menu)?;
                }
            }

            app.state::<InstanceManager>()
                .set_app_handle(app.handle().clone());

            // AgentBusy 固着補正ウォッチドッグ（Esc 検知は write 経路、こちらは定期保険）
            app.state::<InstanceManager>().spawn_watchdog();

            // シャドウスクリーン評価タスク（v0.12 画面状態検出）
            app.state::<InstanceManager>().spawn_screen_evaluator();

            // ネットワーク断耐性チェック（v0.13）+ port forward 台帳の世代チェック。
            // 接続中リモートホストを 60 秒ごとに巡回し、
            // - master の死活プローブ（half-open は -O check では検知できないため
            //   mux 経由リモート実行で判定）→ 連続 2 サイクル不調なら畳んで再確立
            // - gpg agent forward のチェック＋修復（世代ゲート付き）
            // - 台帳の世代交代リプレイ
            // を行う。master 不在ホストへの自動接続は行わない。
            let app_handle_for_fwd = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                loop {
                    interval.tick().await;
                    let manager = app_handle_for_fwd.state::<InstanceManager>();
                    for host in manager.active_remote_hosts() {
                        manager.network_resilience_tick(&host).await;
                    }
                }
            });

            // HookReceiver は同期で起動して endpoint/token/port を確定させ、
            // ラッパースクリプト `~/.ccc/bin/ccc-hook.sh` を最新内容で書き直す。
            // ここを spawn にすると restore_instances や create_*_instance が
            // endpoint 未確定で走ってしまい hook が届かないレースになる。
            let app_handle = app.handle().clone();
            match tauri::async_runtime::block_on(HookReceiver::start()) {
                Ok(handle) => {
                    let manager = app_handle.state::<InstanceManager>();
                    manager.set_hook_endpoint(
                        handle.endpoint.clone(),
                        handle.token.clone(),
                        handle.port,
                    );

                    // ローカル wrapper script を最新の endpoint/token で更新。
                    // 失敗しても起動自体は止めず、警告ログのみ残す。
                    let creds = hook_setup::wrapper::HookCredentials {
                        endpoint: &handle.endpoint,
                        token: &handle.token,
                    };
                    match hook_setup::wrapper::write_local(&creds) {
                        Ok(path) => eprintln!("[ccc] hook wrapper を更新: {}", path.display()),
                        Err(e) => eprintln!("[ccc] hook wrapper の書き出しに失敗: {e}"),
                    }

                    eprintln!("[ccc] HookReceiver listening: {}", handle.endpoint);

                    // 受信ループはバックグラウンドで継続する
                    let mut rx = handle.rx;
                    let app_handle_for_loop = app_handle.clone();
                    tauri::async_runtime::spawn(async move {
                        while let Some(received) = rx.recv().await {
                            app_handle_for_loop
                                .state::<InstanceManager>()
                                .apply_hook(received);
                        }
                    });
                }
                Err(e) => {
                    eprintln!("[ccc] HookReceiver の起動に失敗: {e}");
                }
            }

            Ok(())
        })
        .on_menu_event(|app, event| {
            #[cfg(debug_assertions)]
            if event.id() == "open-devtools" {
                if let Some(window) = app.get_webview_window("main") {
                    window.open_devtools();
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            commands::create_local_instance,
            commands::list_ssh_hosts,
            commands::create_remote_instance,
            commands::write_to_instance,
            commands::resize_instance,
            commands::close_instance,
            commands::list_instances,
            commands::subscribe_instance_output,
            commands::reconnect_instance,
            commands::recreate_instance,
            commands::ensure_shell_started,
            commands::write_to_shell,
            commands::resize_shell,
            commands::subscribe_shell_output,
            commands::close_shell,
            commands::show_main_window,
            commands::restore_instances,
            commands::list_system_fonts,
            commands::read_font_face,
            commands::list_claude_profiles,
            commands::load_settings,
            commands::save_settings,
            commands::list_auth_sources,
            commands::copy_auth_from_instance,
            commands::clear_instance_auth,
            archive_commands::archive_list_sessions,
            archive_commands::archive_search_sessions,
            archive_commands::archive_session_messages,
            archive_commands::archive_list_memory,
            archive_commands::archive_memory_content,
            archive_commands::archive_sync_status,
            cli_install::cli_tool_status,
            cli_install::install_cli_tool,
            explorer::explorer_list_directory,
            explorer::explorer_stat,
            explorer::explorer_get_preview,
            explorer::explorer_search,
            explorer::explorer_copy_into,
            explorer::explorer_download,
            forwards::forwards_list,
            forwards::forwards_add,
            forwards::forwards_remove,
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app, event| {
            if let tauri::RunEvent::ExitRequested { .. } = event {
                // 正常終了時にインスタンス状態を保存し、ccc が立てた ssh ControlMaster を
                // 明示的に閉じる（`ControlPersist=30` 任せにすると次回起動時に旧 master の
                // forward を握ったまま再利用される事故に繋がる）。
                let manager = app.state::<InstanceManager>();
                manager.save_state();
                manager.shutdown_ssh_masters();
            }
        });
}
