use std::path::PathBuf;
use std::env;
use std::str::FromStr;
use serde::{Deserialize, Serialize};
use tracing::warn;
use anyhow::{Result, anyhow};

/// 出力モード
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum OutputMode {
    /// クリップボードにコピー
    Clipboard,
    /// アクティブウィンドウに入力
    Type,
    /// 両方
    Both,
}

impl FromStr for OutputMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "clipboard" => Ok(OutputMode::Clipboard),
            "type" => Ok(OutputMode::Type),
            "both" => Ok(OutputMode::Both),
            _ => Err(format!("不明な出力モード: {}", s)),
        }
    }
}

/// 音声認識エンジン
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TranscriptionEngine {
    /// OpenAI GPT-4o
    GPT4o,
    /// OpenAI Whisper API
    Whisper1,
    /// ローカルのWhisper.cpp
    WhisperCpp {
        path: PathBuf,
        model: PathBuf,
    },
}

impl FromStr for TranscriptionEngine {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "gpt-4o" => Ok(TranscriptionEngine::GPT4o),
            "whisper-1" => Ok(TranscriptionEngine::Whisper1),
            "whisper.cpp" | "whisper-cpp" => {
                // 注: このパターンでFromStrを使用する場合は、パスを別途設定する必要があります
                // 実際の使用時はfrom_strではなく直接コンストラクタを使用します
                Err("Whisper.cppには追加のパラメータが必要です".to_string())
            }
            _ => Err(format!("不明な音声認識エンジン: {}", s)),
        }
    }
}

/// 録音制御モード
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum RecordingMode {
    /// 無音検知
    VoiceActivity {
        silence_threshold: f32,
        silence_duration_ms: u32,
    },
    /// Push-To-Talk
    PushToTalk {
        key: String,
    },
    /// トグル開始/停止
    Toggle {
        key: String,
    },
}

/// アプリケーション設定
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub openai_api_key: String,
    pub output_mode: OutputMode,
    pub language: String,
    pub transcription_engine: TranscriptionEngine,
    pub recording_mode: RecordingMode,
    pub sample_rate: u32,
    pub channels: u16,
    pub max_recording_duration_sec: Option<u32>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            openai_api_key: env::var("OPENAI_API_KEY").unwrap_or_default(),
            output_mode: OutputMode::Clipboard,
            language: "ja".to_string(),
            transcription_engine: TranscriptionEngine::GPT4o,
            recording_mode: RecordingMode::VoiceActivity {
                silence_threshold: 0.01,
                silence_duration_ms: 1000,
            },
            sample_rate: 16000,
            channels: 1,
            max_recording_duration_sec: Some(60),
        }
    }
}

impl Config {
    /// 環境変数と引数から設定を作成
    pub fn new(
        mode: &str,
        lang: &str,
        ptt: Option<&str>,
        engine: &str,
        whisper_cpp_path: Option<&PathBuf>,
        whisper_cpp_model: Option<&PathBuf>,
    ) -> Result<Self> {
        let mut config = Config::default();
        
        // 出力モードの設定
        config.output_mode = OutputMode::from_str(mode)
            .map_err(|e| anyhow!("{}", e))?;
        
        // 言語コードの設定
        config.language = lang.to_string();
        
        // 録音モードの設定
        if let Some(key) = ptt {
            config.recording_mode = RecordingMode::PushToTalk {
                key: key.to_string(),
            };
        } else if let Ok(toggle_key) = env::var("TOGGLE_KEY") {
            // TOGGLE_KEYを使用
            config.recording_mode = RecordingMode::Toggle {
                key: toggle_key,
            };
        } else if let Ok(start_key) = env::var("TOGGLE_START_KEY") {
            // 後方互換性のために古い環境変数もサポート
            warn!("TOGGLE_START_KEY/TOGGLE_STOP_KEYは非推奨です。代わりにTOGGLE_KEYを使用してください。");
            config.recording_mode = RecordingMode::Toggle {
                key: start_key,
            };
        }
        
        // 音声認識エンジンの設定
        match engine.to_lowercase().as_str() {
            "gpt-4o" => {
                config.transcription_engine = TranscriptionEngine::GPT4o;
            }
            "whisper-1" => {
                config.transcription_engine = TranscriptionEngine::Whisper1;
            }
            "whisper.cpp" | "whisper-cpp" => {
                let path = whisper_cpp_path.ok_or_else(|| anyhow!("Whisper.cppのパスが指定されていません"))?;
                let model = whisper_cpp_model.ok_or_else(|| anyhow!("Whisper.cppのモデルパスが指定されていません"))?;
                
                config.transcription_engine = TranscriptionEngine::WhisperCpp {
                    path: path.clone(),
                    model: model.clone(),
                };
            }
            _ => return Err(anyhow!("不明な音声認識エンジン: {}", engine)),
        }
        
        // OpenAI APIキーの確認
        if config.openai_api_key.is_empty() {
            warn!("OPENAI_API_KEYが設定されていません。環境変数または.envファイルで設定してください。");
            if matches!(config.transcription_engine, TranscriptionEngine::GPT4o | TranscriptionEngine::Whisper1) {
                return Err(anyhow!("OpenAI APIを使用するには、OPENAI_API_KEYが必要です"));
            }
        }
        
        Ok(config)
    }
} 