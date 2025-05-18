use arboard::Clipboard;
use anyhow::{Result, anyhow};
use tokio::sync::mpsc;
use std::process::Command;
use tracing::{info, warn, debug};
use std::time::Duration;
use tokio::time::sleep;

// libxdoのリンクを避けるため、直接ライブラリ依存をコメントアウト
// use libxdo; 

use crate::config::{Config, OutputMode};
use crate::transcriber::TranscriptionResult;
use crate::audio::show_notification;

/// 出力マネージャー
pub struct OutputManager {
    config: Config,
    result_rx: mpsc::Receiver<TranscriptionResult>,
}

impl OutputManager {
    /// 新しいOutputManagerを作成
    pub fn new(config: Config, result_rx: mpsc::Receiver<TranscriptionResult>) -> Self {
        Self {
            config,
            result_rx,
        }
    }

    /// 結果処理を実行
    pub async fn run(&mut self) -> Result<()> {
        info!("OutputManager: 結果処理を開始します");
        
        while let Some(result) = self.result_rx.recv().await {
            debug!("OutputManager: 結果を受信: {}", result.text);
            
            match self.config.output_mode {
                OutputMode::Clipboard => {
                    self.copy_to_clipboard(&result.text)?;
                }
                OutputMode::Type => {
                    self.type_text(&result.text).await?;
                }
                OutputMode::Both => {
                    self.copy_to_clipboard(&result.text)?;
                    
                    // 少し待ってからタイプ
                    sleep(Duration::from_millis(100)).await;
                    self.type_text(&result.text).await?;
                }
            }
        }
        
        info!("OutputManager: 結果処理を終了します");
        Ok(())
    }

    /// クリップボードにテキストをコピー
    fn copy_to_clipboard(&self, text: &str) -> Result<()> {
        let mut clipboard = Clipboard::new()
            .map_err(|e| anyhow!("クリップボード初期化エラー: {}", e))?;
        
        clipboard.set_text(text)
            .map_err(|e| anyhow!("クリップボードコピーエラー: {}", e))?;
        
        info!("クリップボードにコピーしました ({} 文字)", text.len());
        
        // 通知を表示
        let short_text = if text.len() > 30 {
            format!("{}...", &text[..30])
        } else {
            text.to_string()
        };
        let message = format!("クリップボードにコピーしました：{}", short_text);
        let _ = show_notification("音声入力", &message);
        
        Ok(())
    }

    /// テキストをタイピング
    async fn type_text(&self, text: &str) -> Result<()> {
        info!("テキストをタイプします ({} 文字)", text.len());
        
        #[cfg(target_os = "macos")]
        {
            // AppleScriptを使用
            let script = format!(
                "tell application \"System Events\" to keystroke \"{}\"",
                text.replace("\\", "\\\\").replace("\"", "\\\"")
            );
            
            let status = Command::new("osascript")
                .args(["-e", &script])
                .status();
            
            match status {
                Ok(status) if status.success() => {
                    debug!("osascriptでタイプ成功");
                    return Ok(());
                }
                Ok(status) => {
                    warn!("osascriptの実行失敗: {}", status);
                    return Err(anyhow!("osascriptの実行に失敗しました"));
                }
                Err(e) => {
                    warn!("osascriptの実行エラー: {}", e);
                    return Err(anyhow!("osascriptの実行に失敗しました: {}", e));
                }
            }
        }
        
        #[cfg(target_os = "linux")]
        {
            // LinuxでX11またはWaylandを検出
            let is_wayland = std::env::var("WAYLAND_DISPLAY").is_ok();
            let is_x11 = std::env::var("DISPLAY").is_ok();
            
            if is_wayland {
                debug!("Wayland環境を検出しました");
                
                // wtype（Wayland用タイプツール）を試す
                if let Ok(status) = Command::new("which")
                    .arg("wtype")
                    .output()
                {
                    if status.status.success() {
                        let status = Command::new("wtype")
                            .arg(text)
                            .status();
                            
                        match status {
                            Ok(status) if status.success() => {
                                debug!("wtypeでタイプ成功");
                                return Ok(());
                            }
                            Ok(status) => {
                                warn!("wtypeの実行失敗: {}", status);
                            }
                            Err(e) => {
                                warn!("wtypeの実行エラー: {}", e);
                            }
                        }
                    }
                }
            }
            
            if is_x11 {
                debug!("X11環境を検出しました");
                
                // xdotool（X11用タイプツール）を試す
                if let Ok(status) = Command::new("which")
                    .arg("xdotool")
                    .output()
                {
                    if status.status.success() {
                        let status = Command::new("xdotool")
                            .args(["type", "--clearmodifiers", text])
                            .status();
                            
                        match status {
                            Ok(status) if status.success() => {
                                debug!("xdotoolでタイプ成功");
                                return Ok(());
                            }
                            Ok(status) => {
                                warn!("xdotoolの実行失敗: {}", status);
                            }
                            Err(e) => {
                                warn!("xdotoolの実行エラー: {}", e);
                            }
                        }
                    }
                }
            }
            
            // どのツールもない場合はエラー
            return Err(anyhow!("テキスト入力ツールが見つかりません。wtypeまたはxdotoolをインストールしてください。"));
        }
        
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            // 非対応プラットフォーム
            return Err(anyhow!("このプラットフォームはサポートされていません"));
        }
    }
} 