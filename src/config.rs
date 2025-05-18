use std::path::PathBuf;
use std::env;
use std::str::FromStr;
use serde::{Deserialize, Serialize};
use tracing::{info, warn, debug};
use anyhow::{Result, anyhow};
use std::fs;
use std::io::Write;
use directories::ProjectDirs;

/// 出力モード
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum OutputMode {
    /// クリップボードにコピー
    Clipboard,
}

impl FromStr for OutputMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "clipboard" => Ok(OutputMode::Clipboard),
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
            "gpt-4o" | "gpt-4o-transcribe" => Ok(TranscriptionEngine::GPT4o),
            "whisper-1" => Ok(TranscriptionEngine::Whisper1),
            "whisper.cpp" | "whisper-cpp" => {
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
    pub remove_silence: bool,
    pub speed_factor: f32,
    pub model: String,
}

impl Default for Config {
    fn default() -> Self {
        // APIキーは環境変数からも読み取れるようにしておく（後方互換性）
        let api_key = env::var("OPENAI_API_KEY").unwrap_or_default();
        
        Self {
            openai_api_key: api_key,
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
            remove_silence: true,
            speed_factor: 1.1,
            model: "gpt-4o-transcribe".to_string(),
        }
    }
}

impl Config {
    /// 設定ファイルのパスを取得
    pub fn get_config_path() -> Result<PathBuf> {
        if let Some(proj_dirs) = ProjectDirs::from("com", "volment", "voilip") {
            let config_dir = proj_dirs.config_dir();
            fs::create_dir_all(config_dir)?;
            Ok(config_dir.join("config.json"))
        } else {
            Err(anyhow!("設定ディレクトリを特定できません"))
        }
    }
    
    /// 設定ファイルから読み込み
    pub fn load() -> Result<Self> {
        let config_path = Self::get_config_path()?;
        
        if config_path.exists() {
            let config_str = fs::read_to_string(&config_path)?;
            let config: Config = serde_json::from_str(&config_str)?;
            info!("設定ファイルを読み込みました: {:?}", config_path);
            Ok(config)
        } else {
            // 設定ファイルがない場合はデフォルト設定を使用
            let config = Config::default();
            info!("設定ファイルが見つからないため、デフォルト設定を使用します");
            Ok(config)
        }
    }
    
    /// 設定ファイルに保存
    pub fn save(&self) -> Result<()> {
        let config_path = Self::get_config_path()?;
        let config_str = serde_json::to_string_pretty(self)?;
        
        let mut file = fs::File::create(&config_path)?;
        file.write_all(config_str.as_bytes())?;
        
        info!("設定を保存しました: {:?}", config_path);
        Ok(())
    }
    
    /// 設定を表示
    pub fn display(&self) -> String {
        let mut output = String::new();
        output.push_str("【現在の設定】\n");
        output.push_str(&format!("APIキー: {}\n", if self.openai_api_key.is_empty() { "未設定" } else { "設定済み" }));
        output.push_str(&format!("出力モード: {:?}\n", self.output_mode));
        output.push_str(&format!("言語: {}\n", self.language));
        
        match &self.recording_mode {
            RecordingMode::VoiceActivity { silence_threshold, silence_duration_ms } => {
                output.push_str(&format!("録音モード: 音声検出 (閾値: {}, 無音時間: {}ms)\n", 
                    silence_threshold, silence_duration_ms));
            }
            RecordingMode::PushToTalk { key } => {
                output.push_str(&format!("録音モード: Push-To-Talk (キー: {})\n", key));
            }
            RecordingMode::Toggle { key } => {
                output.push_str(&format!("録音モード: トグル (キー: {})\n", key));
            }
        }
        
        match &self.transcription_engine {
            TranscriptionEngine::GPT4o => {
                output.push_str(&format!("エンジン: GPT-4o\n"));
            }
            TranscriptionEngine::Whisper1 => {
                output.push_str(&format!("エンジン: Whisper-1\n"));
            }
            TranscriptionEngine::WhisperCpp { path, model } => {
                output.push_str(&format!("エンジン: Whisper.cpp\n"));
                output.push_str(&format!("  パス: {}\n", path.display()));
                output.push_str(&format!("  モデル: {}\n", model.display()));
            }
        }
        
        output.push_str(&format!("モデル: {}\n", self.model));
        output.push_str(&format!("サンプルレート: {}\n", self.sample_rate));
        output.push_str(&format!("チャンネル数: {}\n", self.channels));
        output.push_str(&format!("最大録音時間: {:?}秒\n", self.max_recording_duration_sec));
        output.push_str(&format!("無音除去: {}\n", if self.remove_silence { "有効" } else { "無効" }));
        output.push_str(&format!("再生速度: {:.1}倍速\n", self.speed_factor));
        
        output
    }
    
    /// CLIパラメータと設定ファイルから設定を作成
    pub fn new(
        mode: Option<&str>,
        lang: Option<&str>,
        ptt: Option<&str>,
        engine: Option<&str>,
        whisper_cpp_path: Option<&PathBuf>,
        whisper_cpp_model: Option<&PathBuf>,
        toggle_key: Option<&str>,
        model: Option<&str>,
    ) -> Result<Self> {
        // まず設定ファイルから読み込み
        let mut config = Config::load().unwrap_or_default();
        
        // CLIパラメータで上書き
        if let Some(mode_str) = mode {
            config.output_mode = OutputMode::from_str(mode_str)
                .map_err(|e| anyhow!("{}", e))?;
        }
        
        if let Some(lang_str) = lang {
            config.language = lang_str.to_string();
        }
        
        if let Some(key) = ptt {
            config.recording_mode = RecordingMode::PushToTalk {
                key: key.to_string(),
            };
        } else if let Some(key) = toggle_key {
            config.recording_mode = RecordingMode::Toggle {
                key: key.to_string(),
            };
        }
        
        if let Some(engine_str) = engine {
            match engine_str.to_lowercase().as_str() {
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
                _ => return Err(anyhow!("不明な音声認識エンジン: {}", engine_str)),
            }
        }
        
        if let Some(model_str) = model {
            config.model = model_str.to_string();
        }
        
        // トグルモードの場合、録音の最大持続時間を長く設定
        if let RecordingMode::Toggle { .. } = config.recording_mode {
            config.max_recording_duration_sec = Some(300); // 5分
        }
        
        // OpenAI APIキーの確認
        if config.openai_api_key.is_empty() {
            warn!("OPENAI_API_KEYが設定されていません。設定ファイルで設定してください。");
            if matches!(config.transcription_engine, TranscriptionEngine::GPT4o | TranscriptionEngine::Whisper1) {
                return Err(anyhow!("OpenAI APIを使用するには、APIキーが必要です"));
            }
        }
        
        Ok(config)
    }
    
    /// APIキーを設定
    pub fn set_api_key(&mut self, api_key: &str) -> Result<()> {
        self.openai_api_key = api_key.to_string();
        self.save()?;
        info!("APIキーを設定しました");
        Ok(())
    }
    
    /// 言語を設定
    pub fn set_language(&mut self, lang: &str) -> Result<()> {
        self.language = lang.to_string();
        self.save()?;
        info!("言語を設定しました: {}", lang);
        Ok(())
    }
    
    /// トグルキーを設定
    pub fn set_toggle_key(&mut self, key: &str) -> Result<()> {
        self.recording_mode = RecordingMode::Toggle {
            key: key.to_string(),
        };
        self.save()?;
        info!("トグルキーを設定しました: {}", key);
        Ok(())
    }
    
    /// PTTキーを設定
    pub fn set_ptt_key(&mut self, key: &str) -> Result<()> {
        self.recording_mode = RecordingMode::PushToTalk {
            key: key.to_string(),
        };
        self.save()?;
        info!("PTTキーを設定しました: {}", key);
        Ok(())
    }
    
    /// モデルを設定
    pub fn set_model(&mut self, model: &str) -> Result<()> {
        self.model = model.to_string();
        self.save()?;
        info!("モデルを設定しました: {}", model);
        Ok(())
    }
    
    /// 音声検出モードを設定
    pub fn set_voice_activity(&mut self, threshold: f32, duration_ms: u32) -> Result<()> {
        self.recording_mode = RecordingMode::VoiceActivity {
            silence_threshold: threshold,
            silence_duration_ms: duration_ms,
        };
        self.save()?;
        info!("音声検出モードを設定しました (閾値: {}, 無音時間: {}ms)", threshold, duration_ms);
        Ok(())
    }
    
    /// 無音除去を設定
    pub fn set_remove_silence(&mut self, enable: bool) -> Result<()> {
        self.remove_silence = enable;
        self.save()?;
        info!("無音除去を{}に設定しました", if enable { "有効" } else { "無効" });
        Ok(())
    }
    
    /// 再生速度を設定
    pub fn set_speed_factor(&mut self, factor: f32) -> Result<()> {
        self.speed_factor = factor;
        self.save()?;
        info!("再生速度を{:.1}倍に設定しました", factor);
        Ok(())
    }
} 