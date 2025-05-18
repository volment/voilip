use std::time::{Duration, Instant};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use anyhow::Result;
use tokio::signal::unix::{signal, SignalKind};
use tracing::info;

/// アプリケーションの状態管理
pub struct AppState {
    running: Arc<AtomicBool>,
    start_time: Instant,
}

impl AppState {
    /// 新しいAppStateを作成
    pub fn new() -> Self {
        Self {
            running: Arc::new(AtomicBool::new(true)),
            start_time: Instant::now(),
        }
    }

    /// 実行中フラグの取得
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    /// 実行中フラグの設定
    pub fn set_running(&self, running: bool) {
        self.running.store(running, Ordering::SeqCst);
    }

    /// 実行中フラグのクローン
    pub fn running_clone(&self) -> Arc<AtomicBool> {
        self.running.clone()
    }

    /// 経過時間の取得
    pub fn elapsed(&self) -> Duration {
        self.start_time.elapsed()
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

/// SIGINTシグナル（Ctrl+C）のハンドラセットアップ
pub async fn setup_signal_handler(app_state: Arc<AppState>) -> Result<()> {
    // SIGINT (Ctrl+C) のハンドラを設定
    let mut sigint = signal(SignalKind::interrupt())?;
    
    let state = app_state.clone();
    tokio::spawn(async move {
        sigint.recv().await;
        info!("Ctrl+Cを受信しました。終了します...");
        state.set_running(false);
    });
    
    Ok(())
}

/// 環境変数から設定値を取得（デフォルト値付き）
pub fn get_env_or<T: std::str::FromStr>(key: &str, default: T) -> T {
    std::env::var(key)
        .ok()
        .and_then(|val| val.parse::<T>().ok())
        .unwrap_or(default)
}

/// システム情報を取得してログに出力
pub fn log_system_info() {
    info!("voilip バージョン: {}", env!("CARGO_PKG_VERSION"));
    info!("システム: {}", std::env::consts::OS);
    
    #[cfg(target_os = "linux")]
    {
        if std::env::var("WAYLAND_DISPLAY").is_ok() {
            info!("ディスプレイサーバー: Wayland");
        } else if std::env::var("DISPLAY").is_ok() {
            info!("ディスプレイサーバー: X11");
        } else {
            info!("ディスプレイサーバー: 不明");
        }
    }
}

/// テキストのシンプルな整形
pub fn format_text(text: &str) -> String {
    // 空白文字の連続を1つに置換
    let mut result = text.trim().to_string();
    while result.contains("  ") {
        result = result.replace("  ", " ");
    }
    
    result
} 