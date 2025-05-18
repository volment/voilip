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
        /// 出力モード: clipboard
        #[arg(short, long)]
        mode: Option<String>,
        
        /// 言語コード (例: ja, en)
        #[arg(short, long)]
        lang: Option<String>,
        
        /// Push-To-Talkキー
        #[arg(long)]
        ptt: Option<String>,
        
        /// トグルキー (例: F9, Ctrl+F10)
        #[arg(long)]
        toggle: Option<String>,
        
        /// 音声エンジン: gpt-4o, whisper-1, whisper-cpp
        #[arg(long)]
        engine: Option<String>,
        
        /// Whisper.cppのパス (whisper-cppエンジン使用時)
        #[arg(long)]
        whisper_cpp_path: Option<PathBuf>,
        
        /// Whisper.cppのモデルパス (whisper-cppエンジン使用時)
        #[arg(long)]
        whisper_cpp_model: Option<PathBuf>,
        
        /// 使用するモデル (例: gpt-4o-transcribe)
        #[arg(long)]
        model: Option<String>,
    },
    
    /// テストモード (音声ファイルから文字起こし)
    Test {
        /// テスト用の音声ファイルパス
        #[arg(long, required = true)]
        test_file: PathBuf,
        
        /// 使用するモデル
        #[arg(long)]
        model: Option<String>,
    },
    
    /// 設定の管理
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
}

#[derive(Subcommand)]
enum ConfigAction {
    /// 現在の設定を表示
    Show,
    
    /// APIキーを設定
    SetApiKey {
        /// OpenAI API キー
        api_key: String,
    },
    
    /// トグルキーを設定 (例: F9, Ctrl+F10)
    SetToggleKey {
        /// キー名
        key: String,
    },
    
    /// Push-To-Talkキーを設定
    SetPttKey {
        /// キー名
        key: String,
    },
    
    /// 言語を設定
    SetLanguage {
        /// 言語コード (例: ja, en)
        lang: String,
    },
    
    /// モデルを設定
    SetModel {
        /// モデル名 (例: gpt-4o-transcribe)
        model: String,
    },
    
    /// 無音除去を設定
    SetRemoveSilence {
        /// 有効/無効
        #[arg(default_value = "true")]
        enable: bool,
    },
    
    /// 再生速度を設定
    SetSpeedFactor {
        /// 速度倍率 (例: 1.0, 1.1, 1.5)
        factor: f32,
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
            toggle,
            engine, 
            whisper_cpp_path, 
            whisper_cpp_model,
            model,
        } => {
            // 設定の読み込み
            let mut config = Config::new(
                mode.as_deref(),
                lang.as_deref(),
                ptt.as_deref(),
                engine.as_deref(),
                whisper_cpp_path.as_ref(),
                whisper_cpp_model.as_ref(),
                toggle.as_deref(),
                model.as_deref(),
            )?;
            
            // トグルモードの場合、録音の最大持続時間を長く設定
            if let config::RecordingMode::Toggle { .. } = config.recording_mode {
                config.max_recording_duration_sec = Some(300); // 5分
            }
            
            info!("音声認識を開始します: 言語={}, エンジン={}, モデル={}", 
                config.language, 
                match config.transcription_engine {
                    config::TranscriptionEngine::GPT4o => "GPT-4o",
                    config::TranscriptionEngine::Whisper1 => "Whisper-1",
                    config::TranscriptionEngine::WhisperCpp { .. } => "Whisper.cpp",
                },
                config.model);
            
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
            info!("テストモード: ファイル={}", test_file.display());
            
            // 設定ファイルから読み込み
            let config = Config::load().unwrap_or_default();
            
            // モデルはコマンドラインで指定されたものを優先
            let model_name = model.unwrap_or(config.model.clone());
            info!("使用モデル: {}", model_name);
            
            // チャネルの設定 (ダミー)
            let (_audio_tx, audio_rx) = mpsc::channel::<Vec<f32>>(1);
            let (result_tx, _) = mpsc::channel::<TranscriptionResult>(1);
            
            // Transcriberの作成
            let transcriber = Transcriber::new(config, audio_rx, result_tx);
            
            // テスト実行
            match transcriber.transcribe_file(&test_file, &model_name).await {
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
        Command::Config { action } => {
            match action {
                ConfigAction::Show => {
                    let config = Config::load()?;
                    println!("{}", config.display());
                    Ok(())
                },
                ConfigAction::SetApiKey { api_key } => {
                    let mut config = Config::load()?;
                    config.set_api_key(&api_key)?;
                    println!("APIキーを設定しました");
                    Ok(())
                },
                ConfigAction::SetToggleKey { key } => {
                    let mut config = Config::load()?;
                    config.set_toggle_key(&key)?;
                    println!("トグルキーを設定しました: {}", key);
                    Ok(())
                },
                ConfigAction::SetPttKey { key } => {
                    let mut config = Config::load()?;
                    config.set_ptt_key(&key)?;
                    println!("PTTキーを設定しました: {}", key);
                    Ok(())
                },
                ConfigAction::SetLanguage { lang } => {
                    let mut config = Config::load()?;
                    config.set_language(&lang)?;
                    println!("言語を設定しました: {}", lang);
                    Ok(())
                },
                ConfigAction::SetModel { model } => {
                    let mut config = Config::load()?;
                    config.set_model(&model)?;
                    println!("モデルを設定しました: {}", model);
                    Ok(())
                },
                ConfigAction::SetRemoveSilence { enable } => {
                    let mut config = Config::load()?;
                    config.set_remove_silence(enable)?;
                    println!("無音除去を{}に設定しました", if enable { "有効" } else { "無効" });
                    Ok(())
                },
                ConfigAction::SetSpeedFactor { factor } => {
                    let mut config = Config::load()?;
                    config.set_speed_factor(factor)?;
                    println!("再生速度を{:.1}倍に設定しました", factor);
                    Ok(())
                },
            }
        },
    }
}
