//! UI strings in English and Japanese. Docker terms (Container, Image, Volume, Network,
//! Prune, Inspect, Exec, compose up/down, ...) stay in their CLI form. Error text from the
//! engine or the daemon is shown as it comes, inside a sentence worded here.

use crate::config::LanguagePref;
use crate::dto::{CleanupStep, ContainerAction, Op, PruneKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    En,
    Ja,
}

/// Japanese when chosen, or when following the OS and its locale is Japanese.
pub fn resolve(pref: LanguagePref, system_locale: Option<&str>) -> Lang {
    match pref {
        LanguagePref::En => Lang::En,
        LanguagePref::Ja => Lang::Ja,
        LanguagePref::System => {
            if system_locale.is_some_and(|l| l.to_lowercase().starts_with("ja")) {
                Lang::Ja
            } else {
                Lang::En
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Tr(pub Lang);

/// The unit of a relative time ([`Tr::ago`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeUnit {
    Year,
    Month,
    Day,
    Hour,
    Minute,
}

macro_rules! fixed {
    ($($name:ident: $en:literal, $ja:literal;)*) => {
        impl Tr {
            $(pub fn $name(self) -> &'static str {
                match self.0 {
                    Lang::En => $en,
                    Lang::Ja => $ja,
                }
            })*
        }
    };
}

fixed! {
    nav_containers: "Containers", "Containers";
    nav_images: "Images", "Images";
    nav_volumes: "Volumes", "Volumes";
    nav_networks: "Networks", "Networks";
    nav_settings: "Settings", "設定";
    search: "Search", "検索";
    cancel: "Cancel", "キャンセル";
    close: "Close", "閉じる";
    remove: "Remove", "削除";
    prune: "Prune", "Prune";
    create: "Create", "作成";
    copied: "Copied to clipboard.", "クリップボードにコピーしました。";
    unused: "unused", "未使用";
    actions: "Actions", "操作";

    connecting: "Connecting to Docker Engine…", "Docker Engine に接続しています…";
    conn_lost: "Connection lost — showing last known state", "接続が切れました。最後に取得した状態を表示しています";
    unreachable: "Cannot reach Docker Engine", "Docker Engine に接続できません";
    permission_denied: "Permission denied", "権限がありません";
    permission_detail: "Your user cannot open the Docker socket. Run `sudo usermod -aG docker $USER`, then log out and back in (or use rootless Docker).", "このユーザーは Docker ソケットを開けません。`sudo usermod -aG docker $USER` を実行してから、ログインし直してください（rootless Docker を使う方法もあります）。";
    retry_now: "Retry now", "今すぐ再試行";
    retry: "Retry", "再試行";
    switch_rootless: "Switch to rootless (local)", "rootless (local) に切り替える";

    disk_title: "Docker disk usage", "Docker のディスク使用量";
    disk_short: "Disk usage", "ディスク使用量";
    disk_no_breakdown: "The daemon did not report a breakdown.", "デーモンが内訳を返しませんでした。";
    disk_images: "Images", "Images";
    disk_containers: "Containers", "Containers";
    disk_volumes: "Volumes", "Volumes";
    disk_build_cache: "Build cache", "Build cache";
    prune_build_cache: "Prune build cache", "Build cache を Prune";
    prune_build_cache_title: "Prune build cache?", "Build cache を Prune しますか？";
    no_build_cache: "No build cache to remove.", "削除できる Build cache はありません。";
    cleanup_open: "Clean up…", "一括削除…";
    cleanup_run: "Clean up", "一括削除";
    cleanup_in_progress: "Cleaning up…", "一括削除を実行中…";
    cleanup_title: "Clean up unused resources", "不要なリソースを一括削除";
    cleanup_subtitle: "Removes selected unused items in dependency order so resources held by stopped containers are reclaimed too. Running and paused containers are not touched.", "選択した未使用項目を依存順に削除し、停止中のコンテナが保持していたリソースもあわせて回収します。実行中・一時停止中のコンテナには触れません。";
    cleanup_stopped_containers: "Stopped containers", "停止中のコンテナ";
    cleanup_unused_networks: "Unused custom networks", "未使用のカスタムネットワーク";
    cleanup_dangling_images: "Dangling images (untagged)", "dangling イメージ（タグなし）";
    cleanup_tagged_images: "Include unused tagged images", "未使用のタグ付きイメージも含める";
    cleanup_anonymous_volumes: "Unused anonymous volumes", "未使用の匿名ボリューム";
    cleanup_named_volumes: "Include unused named volumes", "未使用の名前付きボリュームも含める";
    cleanup_legacy_volumes: "Unused volumes (includes named volumes on API < 1.42)", "未使用のボリューム（API 1.42 未満のため名前付きを含む）";
    cleanup_build_cache: "Unused build cache", "未使用の Build cache";
    cleanup_named_volumes_warning: "Warning: Unused named volumes (such as database volumes of stopped stacks) will be permanently deleted and cannot be recovered.", "注意: 未使用の名前付きボリューム（down 済み Stack の DB データなど）も完全に削除され、元に戻せません。";
    cleanup_estimate_hint: "Counts and sizes are estimates; shared image layers and items freed by removing stopped containers affect reclaimed space.", "件数とサイズは目安です（共有レイヤーや停止中コンテナ削除で解放される分により実際の回収量は前後します）。";

    context_title: "Docker context", "Docker context";
    context_follow_cli: "Follow CLI default", "CLI の既定に従う";

    show_stopped: "Show stopped", "停止中も表示";
    prune_stopped: "Prune stopped", "停止中を Prune";
    no_stopped: "No stopped containers to remove.", "削除できる停止中のコンテナはありません。";
    prune_stopped_title: "Prune stopped containers?", "停止中のコンテナを Prune しますか？";
    col_state: "State", "状態";
    col_service_id: "Service / ID", "サービス / ID";
    col_image_ports: "Image & ports", "イメージとポート";
    col_status: "Status", "ステータス";
    no_containers: "No containers — run `docker run hello-world` to create one.", "コンテナはありません。`docker run hello-world` を実行すると作成できます。";
    nothing_matches: "Nothing matches — adjust the search or show stopped containers.", "一致するものはありません。検索語を変えるか、停止中のコンテナも表示してください。";
    act_kill: "Kill", "Kill";
    kill_body: "SIGKILL stops it at once, without a graceful shutdown. Data it has not written yet may be lost.", "SIGKILL は正常終了の処理を待たずにすぐ停止します。書き込み前のデータは失われることがあります。";
    compose_down_body: "All containers and networks of this project will be removed. Named volumes are kept.", "このプロジェクトのコンテナとネットワークをすべて削除します。名前付きボリュームは残ります。";
    compose_missing: "docker compose not found", "docker compose が見つかりません";
    compose_running: "running…", "実行中…";
    compose_done: "done", "完了";

    tab_logs: "Logs", "ログ";
    tab_inspect: "Inspect", "Inspect";
    tab_stats: "Stats", "Stats";
    tab_exec: "Exec", "Exec";
    start_to_exec: "Start the container to open a shell.", "シェルを開くにはコンテナを起動してください。";
    waiting_stats: "Waiting for stats…", "統計を待っています…";
    stats_ended: "Stats stream ended (container stopped).", "統計のストリームが終了しました（コンテナが停止しました）。";
    stat_cpu: "CPU", "CPU";
    stat_memory: "Memory", "メモリ";
    stat_network: "Network I/O", "ネットワーク I/O";
    stat_pids: "PIDs", "PID 数";
    exec_exited: "The shell ended. Press Enter or Reconnect for a new one.", "シェルが終了しました。Enter か「再接続」で新しいシェルを開きます。";
    exec_reconnect: "Reconnect", "再接続";
    exec_new_shell: "New shell", "新しいシェル";
    exec_command: "Command", "コマンド";
    exec_command_hint: "Command (bash, else sh)", "コマンド（既定は bash、なければ sh）";
    exec_user: "User", "ユーザー";
    exec_working_dir: "Working directory", "作業ディレクトリ";
    expand_pane: "Expand", "広げる";
    restore_pane: "Restore", "元に戻す";
    starting_shell: "Starting shell…", "シェルを起動しています…";
    exec_failed: "exec failed", "exec に失敗しました";

    search_logs: "Search the logs (Return: next)", "ログを検索（Return で次へ）";
    search_logs_name: "Search the logs", "ログを検索";
    search_containers: "Search containers", "コンテナを検索";
    copy: "Copy", "コピー";
    clear: "Clear", "クリア";

    copy_redacted: "Copy redacted", "マスクしてコピー";
    copy_raw: "Copy raw JSON", "生の JSON をコピー";
    show_secrets: "Show secrets", "秘密値を表示";
    loading: "Loading…", "読み込み中…";

    pull_placeholder: "Image to pull, e.g. nginx:1.27", "Pull するイメージ（例: nginx:1.27）";
    pull_name: "Image to pull", "Pull するイメージ";
    pull: "Pull", "Pull";
    prune_dangling: "Prune dangling", "dangling を Prune";
    no_dangling: "No dangling images to remove.", "削除できる dangling イメージはありません。";
    prune_dangling_title: "Prune dangling images?", "dangling イメージを Prune しますか？";
    col_repo_tag: "Repository : Tag", "リポジトリ : タグ";
    col_short_id: "Short ID", "短縮 ID";
    col_size: "Size", "サイズ";
    col_created: "Created", "作成";
    col_usage: "Usage", "使用状況";
    remove_image_title: "Remove image?", "イメージを削除しますか？";
    no_images: "No images — pull one with the field above.", "イメージはありません。上の入力欄から Pull できます。";
    run: "Run", "Run";
    run_name: "Name (optional)", "名前（省略可）";
    run_command: "Command (optional; the image's own if empty)", "コマンド（省略可。空ならイメージの既定）";
    run_ports: "Ports, e.g. 8080:80, 127.0.0.1:5432:5432, 53/udp", "ポート（例: 8080:80, 127.0.0.1:5432:5432, 53/udp）";
    run_volumes: "Named volumes, e.g. data:/var/lib/data, cache:/cache:ro", "名前付きボリューム（例: data:/var/lib/data, cache:/cache:ro）";
    run_network: "Network (optional; bridge if empty)", "ネットワーク（省略可。空なら bridge）";
    run_restart: "Restart", "再起動";
    run_env: "Environment: KEY=value, one per line", "環境変数: KEY=value を 1 行に 1 つ";
    run_name_label: "Name", "名前";
    compose_output: "Compose output", "Compose の出力";
    inspect_output: "Inspect output", "Inspect の出力";
    run_command_label: "Command", "コマンド";
    run_ports_label: "Ports", "ポート";
    run_volumes_label: "Named volumes", "名前付きボリューム";
    run_network_label: "Network", "ネットワーク";
    run_env_label: "Environment", "環境変数";
    duplicate: "Duplicate…", "複製…";
    update_container: "Update…", "更新…";
    act_update: "Update", "更新";
    update_body: "Pulls the newest image for its tag. If the tag now names another image, the container is replaced by one made from it with the same settings, volumes and networks (it stops for a moment). If a step fails, the original is put back.", "タグの最新のイメージを Pull します。タグの指すイメージが変わっていれば、同じ設定・ボリューム・ネットワークで新しいイメージから作り直したコンテナに置き換えます（少しの間停止します）。途中で失敗したときは元のコンテナに戻します。";
    container_networks: "Networks…", "ネットワーク…";
    check_updates: "Check for updates", "更新を確認";
    update_available: "Newer image available", "新しいイメージあり";
    update_pulled: "Newer image pulled", "新しいイメージを取得済み";
    update_compose_hint: "Containers of a stack are updated with the stack's Pull and Up.", "Stack のコンテナは Stack の Pull と Up で更新します。";
    run_hint: "Host paths cannot be mounted from the app; a named volume that does not exist yet is created.", "ホストのパスはアプリからマウントできません。存在しない名前付きボリュームは作成されます。";

    new_volume: "New volume name", "新しいボリューム名";
    prune_unused: "Prune unused", "未使用を Prune";
    no_unused_volumes: "No unused volumes to remove.", "削除できる未使用のボリュームはありません。";
    prune_volumes_title: "Prune unused volumes?", "未使用のボリュームを Prune しますか？";
    col_volume: "Volume", "ボリューム";
    col_stack: "Stack", "Stack";
    col_driver: "Driver", "ドライバ";
    col_mountpoint: "Mountpoint", "マウントポイント";
    remove_volume_body: "Its data will be permanently deleted.", "中のデータは完全に削除されます。";
    select_all: "Select all", "すべて選択";
    deselect_all: "Deselect all", "選択を解除";
    no_volumes: "No volumes — create one with the field above.", "ボリュームはありません。上の入力欄から作成できます。";
    anonymous: "anonymous", "匿名";

    new_network: "New network name", "新しいネットワーク名";
    no_unused_networks: "No unused networks to remove.", "削除できる未使用のネットワークはありません。";
    prune_networks_title: "Prune unused networks?", "未使用のネットワークを Prune しますか？";
    col_network: "Network", "ネットワーク";
    col_scope: "Scope", "スコープ";
    col_flags: "Flags", "属性";
    builtin: "built-in", "組み込み";
    internal: "internal", "internal";
    remove_network_body: "This network will be deleted.", "このネットワークを削除します。";
    remove_networks_body: "These networks will be deleted.", "これらのネットワークを削除します。";
    no_networks: "No networks", "ネットワークはありません";

    appearance: "Appearance", "表示";
    theme: "Theme", "テーマ";
    theme_system: "System", "システム";
    theme_light: "Light", "ライト";
    theme_dark: "Dark", "ダーク";
    language: "Language", "言語";
    logs: "Logs", "ログ";
    log_lines: "Lines kept per open log", "開いたログごとに保持する行数";
    engine: "Engine", "Engine";
    target: "Target", "接続先";
    network_map: "Map", "構成図";
    map_ports: "Published ports", "公開ポート";
    map_services: "Services", "サービス";
    map_no_ports: "None", "なし";
    context_source: "Resolved from", "接続先の決定元";
    choose_context: "Choose a context…", "context を選ぶ…";
    not_connected: "Not connected", "未接続";
    compose: "Compose", "Compose";
    checking: "checking…", "確認中…";
    compose_available: "docker compose available", "docker compose を利用できます";
    compose_not_found: "docker compose not found — compose actions are disabled", "docker compose が見つかりません。Compose の操作は無効です";
    settings_file: "Settings file", "設定ファイル";
    settings_file_path: "$XDG_CONFIG_HOME/docker-stack/config.toml (default ~/.config/docker-stack/config.toml)", "$XDG_CONFIG_HOME/docker-stack/config.toml（既定は ~/.config/docker-stack/config.toml）";
    background: "Background", "バックグラウンド";
    increase_contrast: "Increase contrast", "コントラストを上げる";
    increase_contrast_hint: "Darker borders on text fields, buttons and checkboxes.", "入力欄・ボタン・チェックボックスの枠を濃くします。";
    tray_label: "Show a tray icon and keep running when the window is closed", "トレイ（メニューバー）にアイコンを表示し、ウィンドウを閉じても動作を続ける";
    tray_hint: "Choose Quit from the tray menu to exit completely.", "完全に終了するには、トレイのメニューから「終了」を選んでください。";
    save: "Save", "保存";
    tray_show: "Show Docker Stack", "Docker Stack を表示";
    tray_refresh: "Refresh", "再読み込み";
    tray_quit: "Quit", "終了";
    just_now: "just now", "たった今";
}

impl Tr {
    fn pick(self, en: String, ja: String) -> String {
        match self.0 {
            Lang::En => en,
            Lang::Ja => ja,
        }
    }

    pub fn engine_title(self, v: &str) -> String {
        format!("Docker Engine {v}")
    }
    pub fn retrying_in(self, msg: &str, n: u32) -> String {
        self.pick(
            format!("{msg} Retrying in {n}s."),
            format!("{msg} {n} 秒後に再試行します。"),
        )
    }
    pub fn starting_daemon(self, label: &str) -> String {
        self.pick(
            format!("Starting {label}…"),
            format!("{label} を起動しています…"),
        )
    }
    pub fn starting_detail(self, elapsed: u64, limit: u32) -> String {
        self.pick(
            format!("{elapsed}s elapsed · waits up to {limit}s for the socket"),
            format!("{elapsed} 秒経過 · ソケットの応答を最大 {limit} 秒待ちます"),
        )
    }
    pub fn start_daemon(self, label: &str) -> String {
        self.pick(format!("Start {label}"), format!("{label} を起動"))
    }
    pub fn prune_build_cache_body(self, size: &str) -> String {
        self.pick(
            format!("Unused build cache (up to {size}) will be permanently deleted. Images and containers are not affected."),
            format!("使われていない Build cache（最大 {size}）を完全に削除します。イメージとコンテナには影響しません。"),
        )
    }
    pub fn prune_stopped_body(self, n: usize) -> String {
        self.pick(
            format!("{n} stopped containers will be permanently deleted."),
            format!("停止中のコンテナ {n} 個を完全に削除します。"),
        )
    }
    pub fn kill_title(self, name: &str) -> String {
        self.pick(
            format!("Kill {name}?"),
            format!("{name} を Kill しますか？"),
        )
    }
    pub fn remove_container_title(self, name: &str) -> String {
        self.pick(
            format!("Remove container {name}?"),
            format!("コンテナ {name} を削除しますか？"),
        )
    }
    pub fn remove_running_body(self, name: &str) -> String {
        self.pick(
            format!(
                "{name} is running. It will be stopped and removed with its anonymous volumes."
            ),
            format!("{name} は実行中です。停止したうえで、匿名ボリュームとともに削除します。"),
        )
    }
    pub fn remove_stopped_body(self, name: &str) -> String {
        self.pick(
            format!("{name} will be removed with its anonymous volumes."),
            format!("{name} を匿名ボリュームとともに削除します。"),
        )
    }
    pub fn compose_down_title(self, p: &str) -> String {
        self.pick(
            format!("Bring down {p}?"),
            format!("{p} を down しますか？"),
        )
    }
    pub fn network_map_title(self, project: &str) -> String {
        self.pick(
            format!("{project} · network map"),
            format!("{project} のネットワーク構成"),
        )
    }
    pub fn running_count(self, r: usize, t: usize) -> String {
        self.pick(format!("{r}/{t} running"), format!("{r}/{t} 実行中"))
    }
    pub fn line_count(self, n: usize) -> String {
        self.pick(format!("{n} lines"), format!("{n} 行"))
    }
    pub fn match_count(self, current: usize, total: usize) -> String {
        self.pick(
            format!("{current} / {total} matches"),
            format!("{current} / {total} 件"),
        )
    }
    pub fn pulling(self, r: &str) -> String {
        self.pick(format!("Pulling {r}"), format!("{r} を Pull しています"))
    }
    pub fn total_size(self, size: &str) -> String {
        self.pick(format!("{size} total"), format!("合計 {size}"))
    }
    pub fn prune_dangling_body(self, n: usize, size: &str) -> String {
        self.pick(
            format!("{n} dangling images ({size}) will be permanently deleted."),
            format!("dangling イメージ {n} 個（{size}）を完全に削除します。"),
        )
    }
    pub fn in_use_by(self, n: usize) -> String {
        self.pick(format!("in use · {n}"), format!("使用中 · {n}"))
    }
    pub fn remove_image_in_use(self, label: &str) -> String {
        self.pick(
            format!("{label} is used by a container. Forcing removal untags it; existing containers keep their layers."),
            format!("{label} はコンテナが使用しています。強制削除するとタグが外れます。既存のコンテナのレイヤーは残ります。"),
        )
    }
    pub fn remove_image_body(self, label: &str) -> String {
        self.pick(
            format!("{label} will be deleted."),
            format!("{label} を削除します。"),
        )
    }
    pub fn prune_volumes_body(self, n: usize, size: &str, anonymous_only: bool) -> String {
        if anonymous_only {
            self.pick(
                format!("{n} unused anonymous volumes ({size}) will be permanently deleted. Named volumes are kept."),
                format!("未使用の匿名ボリューム {n} 個（{size}）を完全に削除します。名前付きボリュームは残ります。"),
            )
        } else {
            self.pick(
                format!("{n} unused volumes ({size}) will be permanently deleted."),
                format!("未使用のボリューム {n} 個（{size}）を完全に削除します。"),
            )
        }
    }
    pub fn remove_volume_title(self, name: &str) -> String {
        self.pick(
            format!("Remove volume {name}?"),
            format!("ボリューム {name} を削除しますか？"),
        )
    }
    pub fn run_title(self, image: &str) -> String {
        self.pick(format!("Run {image}"), format!("{image} を Run"))
    }
    pub fn duplicate_title(self, source: &str) -> String {
        self.pick(format!("Duplicate {source}"), format!("{source} を複製"))
    }
    /// The settings a duplicate leaves out, listed after the label.
    pub fn duplicate_skipped(self, list: &str) -> String {
        self.pick(
            format!("Not copied (the Run form cannot set these): {list}"),
            format!("複製しなかった設定（Run フォームでは指定できません）: {list}"),
        )
    }
    /// After a check for newer images: how many containers can be updated, and how
    /// many could not be checked.
    pub fn update_check_result(self, updatable: usize, unknown: usize) -> String {
        let found = match (self.0, updatable) {
            (Lang::En, 0) => "Every container runs the newest image of its tag".to_owned(),
            (Lang::En, 1) => "1 container has a newer image".to_owned(),
            (Lang::En, n) => format!("{n} containers have a newer image"),
            (Lang::Ja, 0) => "すべてのコンテナがタグの最新のイメージで動いています".to_owned(),
            (Lang::Ja, n) => format!("{n} 個のコンテナに新しいイメージがあります"),
        };
        match (self.0, unknown) {
            (_, 0) => found,
            (Lang::En, n) => format!("{found} ({n} could not be checked)"),
            (Lang::Ja, n) => format!("{found}（{n} 個は確認できませんでした）"),
        }
    }
    pub fn networks_title(self, name: &str) -> String {
        self.pick(
            format!("Networks of {name}"),
            format!("{name} のネットワーク"),
        )
    }
    /// The Networks dialog's text: what the container is on now.
    pub fn networks_body(self, attached: &str) -> String {
        self.pick(
            format!("Attached to {attached}. A change applies at once, without a restart."),
            format!("接続中: {attached}。変更は再起動なしですぐに反映されます。"),
        )
    }
    pub fn no_network_choices(self, name: &str) -> String {
        self.pick(
            format!("{name} uses the host's network, none, or another container's: its networks cannot be changed"),
            format!("{name} はホストのネットワーク、none、または別のコンテナのネットワークを使っているため、ネットワークを変更できません"),
        )
    }
    pub fn connect_to(self, network: &str) -> String {
        self.pick(format!("Connect to {network}"), format!("{network} に接続"))
    }
    pub fn disconnect_from(self, network: &str) -> String {
        self.pick(
            format!("Disconnect from {network}"),
            format!("{network} から切断"),
        )
    }
    pub fn update_all(self, n: usize) -> String {
        self.pick(format!("Update all ({n})…"), format!("すべて更新（{n}）…"))
    }
    pub fn update_all_title(self, n: usize) -> String {
        self.pick(
            format!("Update {n} containers?"),
            format!("{n} 個のコンテナを更新しますか？"),
        )
    }
    /// What Update all replaces, and how (as [`Tr::update_body`] says for one).
    pub fn update_all_body(self, names: &str) -> String {
        self.pick(
            format!("{names}. Each is replaced by a container made from the newest image of its tag, with the same settings, volumes and networks; one that fails is put back as it was."),
            format!("{names}。それぞれ、同じ設定・ボリューム・ネットワークでタグの最新のイメージから作り直したコンテナに置き換えます。失敗したものは元に戻します。"),
        )
    }
    pub fn update_title(self, name: &str) -> String {
        self.pick(
            format!("Update {name}?"),
            format!("{name} を更新しますか？"),
        )
    }
    pub fn update_up_to_date(self, name: &str) -> String {
        self.pick(
            format!("{name} already runs the newest image"),
            format!("{name} はすでに最新のイメージで動いています"),
        )
    }
    pub fn updated(self, name: &str) -> String {
        self.pick(
            format!("{name} was updated to the newest image"),
            format!("{name} を最新のイメージに更新しました"),
        )
    }
    pub fn updated_kept_old(self, name: &str, old: &str) -> String {
        self.pick(
            format!(
                "{name} was updated; the old container could not be removed and is kept as {old}"
            ),
            format!(
                "{name} を更新しました。古いコンテナは削除できなかったため {old} として残っています"
            ),
        )
    }
    pub fn container_exited(self, name: &str, code: i64) -> String {
        self.pick(
            format!("{name} stopped unexpectedly (exit code {code})"),
            format!("{name} が予期せず停止しました（終了コード {code}）"),
        )
    }
    pub fn container_oom(self, name: &str) -> String {
        self.pick(
            format!("{name} was killed for running out of memory"),
            format!("{name} はメモリ不足で強制終了されました"),
        )
    }
    pub fn container_unhealthy(self, name: &str) -> String {
        self.pick(
            format!("{name} is unhealthy: its health check is failing"),
            format!("{name} のヘルスチェックが失敗しています（unhealthy）"),
        )
    }
    pub fn update_not_restored(
        self,
        name: &str,
        err: &str,
        kept_as: &str,
        stopped: bool,
    ) -> String {
        match (self.0, stopped) {
            (Lang::En, false) => format!(
                "Updating {name} failed: {err}. The original container could not be put back as it was and is kept as {kept_as}"
            ),
            (Lang::En, true) => format!(
                "Updating {name} failed: {err}. The original container could not be put back as it was and is kept as {kept_as}, stopped"
            ),
            (Lang::Ja, false) => format!(
                "{name} の更新に失敗しました: {err}。元のコンテナを元どおりに戻せず、{kept_as} として残っています"
            ),
            (Lang::Ja, true) => format!(
                "{name} の更新に失敗しました: {err}。元のコンテナを元どおりに戻せず、{kept_as} として停止したまま残っています"
            ),
        }
    }
    /// What an operation is called in a notice ("Remove volume data").
    pub fn op_name(self, op: &Op) -> String {
        match (self.0, op) {
            (_, Op::Container(a)) => self.container_op(*a).into(),
            (Lang::En, Op::Update) => "Update".into(),
            (Lang::Ja, Op::Update) => "更新".into(),
            (Lang::En, Op::ConnectNetwork(n)) => format!("Connect {n}"),
            (Lang::Ja, Op::ConnectNetwork(n)) => format!("{n} への接続"),
            (Lang::En, Op::DisconnectNetwork(n)) => format!("Disconnect {n}"),
            (Lang::Ja, Op::DisconnectNetwork(n)) => format!("{n} からの切断"),
            (_, Op::Run(image)) => format!("Run {image}"),
            (Lang::En, Op::RemoveImage) => "Remove image".into(),
            (Lang::Ja, Op::RemoveImage) => "イメージの削除".into(),
            (Lang::En, Op::RemoveVolume(n)) => format!("Remove volume {n}"),
            (Lang::Ja, Op::RemoveVolume(n)) => format!("ボリューム {n} の削除"),
            (Lang::En, Op::RemoveNetwork) => "Remove network".into(),
            (Lang::Ja, Op::RemoveNetwork) => "ネットワークの削除".into(),
            (Lang::En, Op::CreateVolume(n)) => format!("Create volume {n}"),
            (Lang::Ja, Op::CreateVolume(n)) => format!("ボリューム {n} の作成"),
            (Lang::En, Op::CreateNetwork(n)) => format!("Create network {n}"),
            (Lang::Ja, Op::CreateNetwork(n)) => format!("ネットワーク {n} の作成"),
            (Lang::En, Op::Prune(k)) => format!("Prune {}", self.prune_noun(*k)),
            (Lang::Ja, Op::Prune(k)) => format!("{}の Prune", self.prune_noun(*k)),
            (Lang::En, Op::Cleanup) => "Clean up".into(),
            (Lang::Ja, Op::Cleanup) => "クリーンアップ".into(),
        }
    }
    fn container_op(self, action: ContainerAction) -> &'static str {
        match (self.0, action) {
            (Lang::En, ContainerAction::Remove { .. }) => "Remove container",
            (Lang::Ja, ContainerAction::Remove { .. }) => "コンテナの削除",
            (_, ContainerAction::Start) => "Start",
            (_, ContainerAction::Stop) => "Stop",
            (_, ContainerAction::Restart) => "Restart",
            (_, ContainerAction::Kill) => "Kill",
            (_, ContainerAction::Pause) => "Pause",
            (_, ContainerAction::Unpause) => "Unpause",
        }
    }
    pub fn prune_noun(self, kind: PruneKind) -> &'static str {
        match (self.0, kind) {
            (Lang::En, PruneKind::Containers) => "stopped containers",
            (Lang::Ja, PruneKind::Containers) => "停止中のコンテナ",
            (Lang::En, PruneKind::Images) => "dangling images",
            (Lang::Ja, PruneKind::Images) => "dangling イメージ",
            (Lang::En, PruneKind::Volumes) => "unused volumes",
            (Lang::Ja, PruneKind::Volumes) => "未使用のボリューム",
            (Lang::En, PruneKind::Networks) => "unused networks",
            (Lang::Ja, PruneKind::Networks) => "未使用のネットワーク",
            (Lang::En, PruneKind::BuildCache) => "unused build cache",
            (Lang::Ja, PruneKind::BuildCache) => "未使用の Build cache",
        }
    }
    pub fn op_failed(self, op: &Op, err: &str) -> String {
        let name = self.op_name(op);
        self.pick(
            format!("{name} failed: {err}"),
            format!("{name}に失敗しました: {err}"),
        )
    }
    pub fn run_started(self, label: &str, id: &str) -> String {
        self.pick(
            format!("Started {label} ({id})"),
            format!("{label} を起動しました（{id}）"),
        )
    }
    pub fn daemon_started(self, label: &str) -> String {
        self.pick(
            format!("Started {label}"),
            format!("{label} を起動しました"),
        )
    }
    pub fn pulled(self, reference: &str) -> String {
        self.pick(
            format!("Pulled {reference}"),
            format!("{reference} を Pull しました"),
        )
    }
    pub fn pull_failed(self, reference: &str, err: &str) -> String {
        self.pick(
            format!("Pull {reference} failed: {err}"),
            format!("{reference} の Pull に失敗しました: {err}"),
        )
    }
    pub fn pruned(self, kind: PruneKind, deleted: usize, reclaimed: &str) -> String {
        let noun = self.prune_noun(kind);
        self.pick(
            format!("Removed {deleted} {noun}; reclaimed {reclaimed}"),
            format!("{noun}を {deleted} 件削除しました（{reclaimed} 回収）"),
        )
    }
    pub fn compose_finished(self, command: &str, project: &str) -> String {
        self.pick(
            format!("compose {command} {project}: done"),
            format!("compose {command} {project}: 完了しました"),
        )
    }
    pub fn start_selected(self, n: usize) -> String {
        self.pick(
            format!("Start selected ({n})"),
            format!("選択を Start（{n}）"),
        )
    }
    pub fn stop_selected(self, n: usize) -> String {
        self.pick(
            format!("Stop selected ({n})"),
            format!("選択を Stop（{n}）"),
        )
    }
    pub fn inspect_title(self, label: &str) -> String {
        format!("Inspect {label}")
    }
    pub fn remove_selected(self, n: usize) -> String {
        self.pick(
            format!("Remove selected ({n})"),
            format!("選択を削除（{n}）"),
        )
    }
    pub fn remove_volumes_title(self, n: usize) -> String {
        self.pick(
            format!("Remove {n} volumes?"),
            format!("{n} 個のボリュームを削除しますか？"),
        )
    }
    pub fn remove_volumes_in_use(self, n: usize) -> String {
        self.pick(
            format!("{n} of them are mounted by containers; Docker refuses to remove those until the containers are removed."),
            format!("うち {n} 個はコンテナがマウントしています。それらはコンテナを削除するまで Docker が削除を拒否します。"),
        )
    }
    pub fn remove_containers_title(self, n: usize) -> String {
        self.pick(
            format!("Remove {n} containers?"),
            format!("{n} 個のコンテナを削除しますか？"),
        )
    }
    pub fn remove_containers_body(self, running: usize) -> String {
        if running > 0 {
            self.pick(
                format!("{running} of them are running and will be stopped. All are removed with their anonymous volumes."),
                format!("うち {running} 個は実行中のため停止します。すべて匿名ボリュームとともに削除します。"),
            )
        } else {
            self.pick(
                "They will be removed with their anonymous volumes.".to_owned(),
                "匿名ボリュームとともに削除します。".to_owned(),
            )
        }
    }
    pub fn remove_images_title(self, n: usize) -> String {
        self.pick(
            format!("Remove {n} images?"),
            format!("{n} 個のイメージを削除しますか？"),
        )
    }
    pub fn remove_images_body(self, n: usize) -> String {
        self.pick(
            format!("{n} images will be deleted."),
            format!("イメージ {n} 個を削除します。"),
        )
    }
    pub fn remove_images_in_use(self, n: usize) -> String {
        self.pick(
            format!("{n} of them are used by containers. Forcing removal untags them; existing containers keep their layers."),
            format!("うち {n} 個はコンテナが使用しています。強制削除するとタグが外れます。既存のコンテナのレイヤーは残ります。"),
        )
    }
    pub fn remove_networks_title(self, n: usize) -> String {
        self.pick(
            format!("Remove {n} networks?"),
            format!("{n} 個のネットワークを削除しますか？"),
        )
    }
    pub fn remove_networks_in_use(self, n: usize) -> String {
        self.pick(
            format!("{n} of them have containers attached; Docker refuses to remove those until they are disconnected."),
            format!("うち {n} 個はコンテナが接続しています。切断するまで Docker は削除を拒否します。"),
        )
    }
    pub fn remove_volume_in_use(self, n: usize) -> String {
        self.pick(
            format!("It is mounted by {n} containers; Docker refuses to remove it until they are removed."),
            format!("{n} 個のコンテナがマウントしています。それらを削除するまで Docker は削除を拒否します。"),
        )
    }
    pub fn prune_networks_body(self, n: usize) -> String {
        self.pick(
            format!("{n} custom networks without containers will be deleted."),
            format!("コンテナのないカスタムネットワーク {n} 個を削除します。"),
        )
    }
    pub fn remove_network_title(self, name: &str) -> String {
        self.pick(
            format!("Remove network {name}?"),
            format!("ネットワーク {name} を削除しますか？"),
        )
    }
    pub fn remove_network_in_use(self, n: usize) -> String {
        self.pick(
            format!("{n} containers are attached. Disconnect them first."),
            format!("{n} 個のコンテナが接続しています。先に切断してください。"),
        )
    }
    pub fn log_lines_range(self, min: u32, max: u32) -> String {
        self.pick(
            format!("{min}–{max}; applies to newly opened logs."),
            format!("{min}–{max}。新しく開いたログから適用されます。"),
        )
    }
    /// The CPU meter's unit.
    pub fn cores(self, one: bool) -> &'static str {
        match (self.0, one) {
            (Lang::En, true) => "core",
            (Lang::En, false) => "cores",
            (Lang::Ja, _) => "コア",
        }
    }
    pub fn tray_running(self, n: usize) -> String {
        self.pick(format!("{n} running"), format!("{n} 実行中"))
    }

    pub fn cleanup_count(self, n: usize) -> String {
        match (self.0, n) {
            (Lang::En, 1) => "1 item".to_owned(),
            (Lang::En, n) => format!("{n} items"),
            (Lang::Ja, n) => format!("{n} 個"),
        }
    }

    pub fn cleanup_count_size(self, n: usize, size: &str) -> String {
        format!("{} · {size}", self.cleanup_count(n))
    }

    pub fn cleanup_up_to(self, size: &str) -> String {
        self.pick(format!("up to {size}"), format!("最大 {size}"))
    }

    pub fn cleanup_step_name(self, step: CleanupStep) -> &'static str {
        match (self.0, step) {
            (Lang::En, CleanupStep::Containers) => "stopped containers",
            (Lang::Ja, CleanupStep::Containers) => "停止中のコンテナ",
            (Lang::En, CleanupStep::Networks) => "unused networks",
            (Lang::Ja, CleanupStep::Networks) => "未使用のネットワーク",
            (Lang::En, CleanupStep::Images) => "unused images",
            (Lang::Ja, CleanupStep::Images) => "未使用のイメージ",
            (Lang::En, CleanupStep::Volumes) => "unused volumes",
            (Lang::Ja, CleanupStep::Volumes) => "未使用のボリューム",
            (Lang::En, CleanupStep::BuildCache) => "build cache",
            (Lang::Ja, CleanupStep::BuildCache) => "Build cache",
        }
    }

    pub fn cleanup_done(self, deleted: usize, reclaimed: &str) -> String {
        if deleted == 0 && reclaimed == "0 B" {
            return self.pick(
                "Nothing unused to remove".to_owned(),
                "削除できる不要な項目はありませんでした".to_owned(),
            );
        }
        let count = self.cleanup_count(deleted);
        self.pick(
            format!("Cleaned up {count}; reclaimed {reclaimed}"),
            format!("{count}の不要項目を削除しました（{reclaimed} 回収）"),
        )
    }

    pub fn cleanup_partial(self, deleted: usize, reclaimed: &str, step: &str, err: &str) -> String {
        let count = self.cleanup_count(deleted);
        self.pick(
            format!("Cleaned up {count} ({reclaimed}) before {step} failed: {err}"),
            format!("{count}削除（{reclaimed} 回収）後、{step} で中断しました: {err}"),
        )
    }

    /// "3 hours ago" style, like `Intl.RelativeTimeFormat` with `numeric: "always"`.
    pub fn ago(self, n: u64, unit: TimeUnit) -> String {
        match self.0 {
            Lang::En => {
                let unit = match unit {
                    TimeUnit::Year => "year",
                    TimeUnit::Month => "month",
                    TimeUnit::Day => "day",
                    TimeUnit::Hour => "hour",
                    TimeUnit::Minute => "minute",
                };
                let plural = if n == 1 { "" } else { "s" };
                format!("{n} {unit}{plural} ago")
            }
            Lang::Ja => {
                let unit = match unit {
                    TimeUnit::Year => "年",
                    TimeUnit::Month => "か月",
                    TimeUnit::Day => "日",
                    TimeUnit::Hour => "時間",
                    TimeUnit::Minute => "分",
                };
                format!("{n} {unit}前")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_language_follows_the_locale() {
        assert_eq!(resolve(LanguagePref::System, Some("ja-JP")), Lang::Ja);
        assert_eq!(resolve(LanguagePref::System, Some("en-US")), Lang::En);
        assert_eq!(resolve(LanguagePref::System, None), Lang::En);
        assert_eq!(resolve(LanguagePref::Ja, Some("en-US")), Lang::Ja);
    }

    #[test]
    fn crash_and_duplicate_messages_name_the_container_in_both_languages() {
        for t in [Tr(Lang::En), Tr(Lang::Ja)] {
            assert!(t.container_exited("web", 137).contains("web"));
            assert!(t.container_exited("web", 137).contains("137"));
            assert!(t.container_oom("web").contains("web"));
            assert!(t.container_unhealthy("web").contains("web"));
            assert!(t.update_title("web").contains("web"));
            assert!(t.update_all(3).contains('3'));
            assert!(t.networks_title("web").contains("web"));
            assert!(t.networks_body("front, back").contains("front, back"));
            assert!(t.no_network_choices("web").contains("web"));
            assert!(t.connect_to("back").contains("back"));
            assert!(t.disconnect_from("front").contains("front"));
            assert!(t.update_all_title(3).contains('3'));
            assert!(t.update_all_body("web, db").starts_with("web, db"));
            assert!(t.update_check_result(2, 1).contains('2'));
            assert!(t.update_check_result(2, 1).contains('1'));
            assert!(!t.update_check_result(2, 0).contains('0'));
            assert!(t.update_up_to_date("web").contains("web"));
            assert!(t.updated("web").contains("web"));
            let kept = t.updated_kept_old("web", "web-replaced-0123");
            assert!(kept.contains("web") && kept.contains("web-replaced-0123"));
            assert!(t.duplicate_title("web").contains("web"));
            assert!(
                t.duplicate_skipped("bind /a → /b")
                    .ends_with("bind /a → /b")
            );
        }
        assert_ne!(Tr(Lang::En).duplicate(), Tr(Lang::Ja).duplicate());
        assert_ne!(Tr(Lang::En).update_body(), Tr(Lang::Ja).update_body());
        assert_eq!(
            Tr(Lang::En).update_check_result(1, 0),
            "1 container has a newer image"
        );
    }

    #[test]
    fn relative_time_reads_naturally() {
        assert_eq!(Tr(Lang::En).ago(1, TimeUnit::Hour), "1 hour ago");
        assert_eq!(Tr(Lang::En).ago(3, TimeUnit::Day), "3 days ago");
        assert_eq!(Tr(Lang::Ja).ago(3, TimeUnit::Day), "3 日前");
        assert_eq!(Tr(Lang::Ja).ago(2, TimeUnit::Month), "2 か月前");
        assert_eq!(Tr(Lang::En).ago(1, TimeUnit::Minute), "1 minute ago");
    }

    #[test]
    fn cleanup_strings_cover_both_languages() {
        for t in [Tr(Lang::En), Tr(Lang::Ja)] {
            assert!(t.cleanup_count_size(3, "12 MB").contains("12 MB"));
            assert!(t.cleanup_up_to("6 GB").contains("6 GB"));
            assert!(t.cleanup_done(4, "120 MB").contains("120 MB"));
            let partial =
                t.cleanup_partial(2, "50 MB", t.cleanup_step_name(CleanupStep::Images), "busy");
            assert!(partial.contains("50 MB") && partial.contains("busy"));
        }
        assert_ne!(Tr(Lang::En).cleanup_open(), Tr(Lang::Ja).cleanup_open());
        assert_ne!(
            Tr(Lang::En).cleanup_done(0, "0 B"),
            Tr(Lang::Ja).cleanup_done(0, "0 B")
        );
    }
}
