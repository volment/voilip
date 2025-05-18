use clap::{Parser, Subcommand};
use anyhow::Result;
use tracing::{info, error, Level};
use tracing_subscriber::FmtSubscriber;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;

mod audio;
mod transcriber;
mod output;
mod config;
mod utils;

use crate::config::Config;
use crate::audio::AudioCapture;
use crate::transcriber::{Transcriber, TranscriptionResult};
use crate::output::OutputManager;
use crate::utils::{AppState, setup_signal_handler, log_system_info};

#[derive(Parser)]
#[command(name = "voilip")]
#[command(author = "volment")]
#[command(version)]
#[command(about = "CLI音声入力ユーティリティ - 音声をリアルタイムで文字起こしして出力", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// 音声認識を開始
    Start {
        /// 出力モード: clipboard, type, both
        #[arg(short, long, default_value = "clipboard")]
        mode: String,
        
        /// 言語コード (例: ja, en)
        #[arg(short, long, default_value = "ja")]
        lang: String,
        
        /// Push-To-Talkキー
        #[arg(long)]
        ptt: Option<String>,
        
        /// 音声エンジン: gpt-4o, whisper-1, whisper-cpp
        #[arg(long, default_value = "gpt-4o")]
        engine: String,
        
        /// Whisper.cppのパス (whisper-cppエンジン使用時)
        #[arg(long)]
        whisper_cpp_path: Option<PathBuf>,
        
        /// Whisper.cppのモデルパス (whisper-cppエンジン使用時)
        #[arg(long)]
        whisper_cpp_model: Option<PathBuf>,
    },
    
    /// テストモード (音声ファイルから文字起こし)
    Test {
        /// テスト用の音声ファイルパス
        #[arg(long, required = true)]
        test_file: PathBuf,
        
        /// 使用するモデル
        #[arg(long, default_value = "gpt-4o")]
        model: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    // 環境変数の読み込み
    dotenv::dotenv().ok();
    
    // ロガーの初期化
    let subscriber = FmtSubscriber::builder()
        .with_max_level(Level::DEBUG)
        .finish();
    tracing::subscriber::set_global_default(subscriber)?;
    
    // システム情報のログ出力
    log_system_info();
    
    // CLIの解析
    let cli = Cli::parse();
    
    match cli.command {
        Command::Start { 
            mode, 
            lang, 
            ptt, 
            engine, 
            whisper_cpp_path, 
            whisper_cpp_model 
        } => {
            // 設定の読み込み
            let config = Config::new(
                &mode,
                &lang,
                ptt.as_deref(),
                &engine,
                whisper_cpp_path.as_ref(),
                whisper_cpp_model.as_ref(),
            )?;
            
            info!("音声認識を開始します: モード={}, 言語={}, エンジン={}", mode, lang, engine);
            
            // チャネルの設定
            let (audio_tx, audio_rx) = mpsc::channel::<Vec<f32>>(32);
            let (result_tx, result_rx) = mpsc::channel::<TranscriptionResult>(32);
            
            // アプリケーション状態の初期化
            let app_state = Arc::new(AppState::new());
            
            // シグナルハンドラのセットアップ
            setup_signal_handler(app_state.clone()).await?;
            
            // コンポーネントの初期化
            let mut audio_capture = AudioCapture::new(config.clone(), audio_tx);
            let mut transcriber = Transcriber::new(config.clone(), audio_rx, result_tx);
            let mut output_manager = OutputManager::new(config.clone(), result_rx);
            
            // 音声キャプチャの開始
            audio_capture.start()?;
            
            // 録音制御モードの設定
            match config.recording_mode {
                config::RecordingMode::PushToTalk { .. } => {
                    audio_capture.setup_ptt_control()?;
                }
                config::RecordingMode::Toggle { .. } => {
                    audio_capture.setup_toggle_control()?;
                }
                _ => {}
            }
            
            // 各コンポーネントの実行
            let transcriber_future = tokio::spawn(async move {
                if let Err(e) = transcriber.run().await {
                    error!("音声認識エラー: {}", e);
                }
            });
            
            let output_future = tokio::spawn(async move {
                if let Err(e) = output_manager.run().await {
                    error!("出力処理エラー: {}", e);
                }
            });
            
            // アプリケーションのメインループ
            while app_state.is_running() {
                tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
            }
            
            // 終了処理
            info!("アプリケーションを終了しています...");
            
            // 音声キャプチャの停止
            audio_capture.stop();
            
            // タスクの終了を待機
            let _ = transcriber_future.await;
            let _ = output_future.await;
            
            info!("正常に終了しました");
            Ok(())
        },
        Command::Test { test_file, model } => {
            info!("テストモード: ファイル={}, モデル={}", test_file.display(), model);
            
            // デフォルト設定
            let config = Config::default();
            
            // チャネルの設定 (ダミー)
            let (_audio_tx, audio_rx) = mpsc::channel::<Vec<f32>>(1);
            let (result_tx, _) = mpsc::channel::<TranscriptionResult>(1);
            
            // Transcriberの作成
            let transcriber = Transcriber::new(config, audio_rx, result_tx);
            
            // テスト実行
            match transcriber.transcribe_file(&test_file, &model).await {
                Ok(result) => {
                    info!("文字起こし結果:");
                    info!("言語: {}", result.language);
                    info!("長さ: {:.2}秒", result.duration_sec);
                    info!("テキスト: {}", result.text);
                    Ok(())
                }
                Err(e) => {
                    error!("文字起こしエラー: {}", e);
                    Err(e)
                }
            }
        },
    }
}
