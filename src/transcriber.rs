use std::path::PathBuf;
use std::io::Cursor;
use anyhow::{Result, anyhow};
use tracing::{info, warn, error, debug};
use tokio::sync::mpsc;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use hound::{WavSpec, WavWriter, SampleFormat};
use std::fs;
use std::process::Command;
use tempfile::NamedTempFile;

use crate::config::{Config, TranscriptionEngine};

const API_RETRY_MAX: u8 = 3;
const API_RETRY_DELAY_MS: u64 = 1000;

/// 文字起こし結果
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptionResult {
    pub text: String,
    pub language: String,
    pub duration_sec: f32,
}

/// 音声認識エンジン
pub struct Transcriber {
    config: Config,
    audio_rx: mpsc::Receiver<Vec<f32>>,
    result_tx: mpsc::Sender<TranscriptionResult>,
}

impl Transcriber {
    /// 新しいTranscriberを作成
    pub fn new(
        config: Config,
        audio_rx: mpsc::Receiver<Vec<f32>>,
        result_tx: mpsc::Sender<TranscriptionResult>,
    ) -> Self {
        Self {
            config,
            audio_rx,
            result_tx,
        }
    }

    /// 音声認識処理を実行
    pub async fn run(&mut self) -> Result<()> {
        info!("Transcriber: 音声認識処理を開始します");
        
        while let Some(audio_data) = self.audio_rx.recv().await {
            if audio_data.is_empty() {
                continue;
            }
            
            debug!("Transcriber: 音声データを受信 ({} サンプル)", audio_data.len());
            
            // WAVファイルにエンコード
            let wav_data = self.encode_wav(&audio_data)?;
            
            // 音声認識を実行
            match &self.config.transcription_engine {
                TranscriptionEngine::GPT4o => {
                    let config_clone = self.config.clone();
                    let wav_data_clone = wav_data.clone();
                    let result_tx = self.result_tx.clone();
                    
                    // ブロッキングAPIを別スレッドで実行
                    tokio::task::spawn_blocking(move || {
                        match transcribe_with_openai(&config_clone, "gpt-4o", &wav_data_clone) {
                            Ok(result) => {
                                tokio::spawn(async move {
                                    if let Err(e) = result_tx.send(result).await {
                                        error!("結果送信エラー: {}", e);
                                    }
                                });
                            }
                            Err(e) => error!("GPT-4o 音声認識エラー: {}", e),
                        }
                    });
                }
                TranscriptionEngine::Whisper1 => {
                    let config_clone = self.config.clone();
                    let wav_data_clone = wav_data.clone();
                    let result_tx = self.result_tx.clone();
                    
                    // ブロッキングAPIを別スレッドで実行
                    tokio::task::spawn_blocking(move || {
                        match transcribe_with_openai(&config_clone, "whisper-1", &wav_data_clone) {
                            Ok(result) => {
                                tokio::spawn(async move {
                                    if let Err(e) = result_tx.send(result).await {
                                        error!("結果送信エラー: {}", e);
                                    }
                                });
                            }
                            Err(e) => error!("Whisper-1 音声認識エラー: {}", e),
                        }
                    });
                }
                TranscriptionEngine::WhisperCpp { path, model } => {
                    let config_clone = self.config.clone();
                    let path_clone = path.clone();
                    let model_clone = model.clone();
                    let wav_data_clone = wav_data.clone();
                    let result_tx = self.result_tx.clone();
                    
                    // ブロッキングAPIを別スレッドで実行
                    tokio::task::spawn_blocking(move || {
                        match transcribe_with_whisper_cpp(&config_clone, &path_clone, &model_clone, &wav_data_clone) {
                            Ok(result) => {
                                tokio::spawn(async move {
                                    if let Err(e) = result_tx.send(result).await {
                                        error!("結果送信エラー: {}", e);
                                    }
                                });
                            }
                            Err(e) => error!("Whisper.cpp 音声認識エラー: {}", e),
                        }
                    });
                }
            }
        }
        
        info!("Transcriber: 音声認識処理を終了します");
        Ok(())
    }

    /// 音声データをWAVファイルにエンコード
    fn encode_wav(&self, audio_data: &[f32]) -> Result<Vec<u8>> {
        let spec = WavSpec {
            channels: self.config.channels,
            sample_rate: self.config.sample_rate,
            bits_per_sample: 16,
            sample_format: SampleFormat::Int,
        };
        
        let mut buffer = Vec::new();
        let mut writer = WavWriter::new(Cursor::new(&mut buffer), spec)?;
        
        // f32サンプルをi16に変換して書き込み
        for &sample in audio_data {
            let sample_i16 = (sample * 32767.0) as i16;
            writer.write_sample(sample_i16)?;
        }
        
        writer.finalize()?;
        Ok(buffer)
    }

    /// テストモード: 音声ファイルから文字起こし
    pub async fn transcribe_file(
        &self,
        file_path: &PathBuf,
        model: &str,
    ) -> Result<TranscriptionResult> {
        // ファイルを読み込み
        let wav_data = std::fs::read(file_path)?;
        let config_clone = self.config.clone();
        
        // モデルに応じた文字起こし
        match model {
            "gpt-4o" => {
                tokio::task::spawn_blocking(move || {
                    transcribe_with_openai(&config_clone, "gpt-4o", &wav_data)
                }).await?
            }
            "whisper-1" => {
                tokio::task::spawn_blocking(move || {
                    transcribe_with_openai(&config_clone, "whisper-1", &wav_data)
                }).await?
            }
            "whisper.cpp" | "whisper-cpp" => {
                if let TranscriptionEngine::WhisperCpp { path, model } = &self.config.transcription_engine {
                    let path_clone = path.clone();
                    let model_clone = model.clone();
                    let config_clone = self.config.clone();
                    let wav_data_clone = wav_data.clone();
                    
                    tokio::task::spawn_blocking(move || {
                        transcribe_with_whisper_cpp(&config_clone, &path_clone, &model_clone, &wav_data_clone)
                    }).await?
                } else {
                    Err(anyhow!("Whisper.cppを使用するには、パスとモデルが設定されている必要があります"))
                }
            }
            _ => Err(anyhow!("サポートされていないモデル: {}", model)),
        }
    }
}

/// OpenAI APIで音声認識
fn transcribe_with_openai(config: &Config, model: &str, wav_data: &[u8]) -> Result<TranscriptionResult> {
    let api_key = config.openai_api_key.clone();
    if api_key.is_empty() {
        return Err(anyhow!("OpenAI APIキーが設定されていません"));
    }
    
    // モデルを決定する - 引数で指定されたものがあればそれを使用、なければ設定ファイルのモデルを使用
    let transcription_model = match model {
        "gpt-4o" => "gpt-4o-transcribe", // 後方互換性のため
        _ => &config.model,
    };
    
    let url = "https://api.openai.com/v1/audio/transcriptions";
    
    // 処理された音声データの情報をログに出力
    let wav_duration = audio_duration_sec(wav_data)?;
    debug!("音声データの処理: 長さ {:.2}秒, サイズ {} バイト", wav_duration, wav_data.len());
    debug!("使用するモデル: {}", transcription_model);
    
    let mut retry_count = 0;
    loop {
        // WAVファイルを一時ファイルに書き出す
        let mut temp_file = NamedTempFile::new()?;
        std::io::copy(&mut Cursor::new(wav_data), &mut temp_file)?;
        let temp_path = temp_file.path().to_str().ok_or_else(|| anyhow!("一時ファイルパスの変換エラー"))?;
        
        // 拡張子を明示的に指定
        let temp_path_with_ext = format!("{}.wav", temp_path);
        std::fs::copy(temp_path, &temp_path_with_ext)?;

        // curlコマンドをデバッグ出力
        debug!("実行するcurlコマンド: curl -s -X POST -H \"Authorization: Bearer ***\" -H \"Content-Type: multipart/form-data\" -F \"model={}\" -F \"language={}\" -F \"response_format=json\" -F \"file=@{}\" {}", 
              transcription_model, config.language, temp_path_with_ext, url);
        
        // curlコマンドを使用してリクエスト
        let output = Command::new("curl")
            .arg("-s")
            .arg("-X").arg("POST")
            .arg("-H").arg(format!("Authorization: Bearer {}", api_key))
            .arg("-H").arg("Content-Type: multipart/form-data")
            .arg("-F").arg(format!("model={}", transcription_model))
            .arg("-F").arg(format!("language={}", config.language))
            .arg("-F").arg("response_format=json")
            .arg("-F").arg(format!("file=@{}", temp_path_with_ext))
            .arg(url)
            .output()?;
        
        // 一時ファイルを削除
        let _ = std::fs::remove_file(temp_path_with_ext);
        
        if output.status.success() {
            let response = String::from_utf8(output.stdout)?;
            debug!("API応答: {}", response);
            let json: Value = serde_json::from_str(&response)?;
            
            if let Some(text) = json.get("text").and_then(|t| t.as_str()) {
                let duration = audio_duration_sec(wav_data)?;
                
                info!("文字起こし完了: {} ({:.2}秒)", text, duration);
                
                return Ok(TranscriptionResult {
                    text: text.to_string(),
                    language: config.language.clone(),
                    duration_sec: duration,
                });
            } else {
                return Err(anyhow!("APIレスポンスにテキストがありません: {}", response));
            }
        } else {
            let error_text = String::from_utf8(output.stderr)?;
            let status = output.status.code().unwrap_or(500);
            let stdout_text = String::from_utf8(output.stdout)?;
            
            if retry_count < API_RETRY_MAX && (status == 429 || status >= 500) {
                // レート制限または一時的なサーバーエラーの場合はリトライ
                retry_count += 1;
                warn!("API呼び出しエラー ({}/{}): {} - {}. リトライします...", 
                    retry_count, API_RETRY_MAX, status, error_text);
                
                std::thread::sleep(std::time::Duration::from_millis(
                    API_RETRY_DELAY_MS * 2u64.pow(retry_count as u32 - 1)
                ));
                continue;
            }
            
            return Err(anyhow!("API呼び出しエラー: {} - {} - {}", status, error_text, stdout_text));
        }
    }
}

/// Whisper.cppを使用した音声認識
fn transcribe_with_whisper_cpp(
    _config: &Config,
    whisper_path: &PathBuf,
    model_path: &PathBuf,
    wav_data: &[u8],
) -> Result<TranscriptionResult> {
    // 一時ファイルに保存
    let mut temp_file = NamedTempFile::new()?;
    std::io::copy(&mut Cursor::new(wav_data), &mut temp_file)?;
    let temp_path = temp_file.path();
    
    // Whisper.cppのコマンドを構築
    let output = Command::new(whisper_path)
        .arg("-m").arg(model_path)
        .arg("-f").arg(temp_path)
        .arg("-otxt")
        .output()?;
    
    if output.status.success() {
        // 出力ファイル名を取得
        let output_file = format!("{}.txt", temp_path.to_string_lossy());
        
        // 結果を読み込み
        let text = fs::read_to_string(&output_file)?;
        let duration = audio_duration_sec(wav_data)?;
        
        // 一時ファイルを削除
        let _ = fs::remove_file(&output_file);
        
        info!("Whisper.cppによる文字起こし完了 ({:.2}秒)", duration);
        
        Ok(TranscriptionResult {
            text,
            language: "auto".to_string(), // Whisper.cppは自動的に言語を検出
            duration_sec: duration,
        })
    } else {
        let error = String::from_utf8(output.stderr)?;
        Err(anyhow!("Whisper.cpp実行エラー: {}", error))
    }
}

/// 音声ファイルの長さ（秒）を取得
fn audio_duration_sec(wav_data: &[u8]) -> Result<f32> {
    let reader = hound::WavReader::new(Cursor::new(wav_data))?;
    let spec = reader.spec();
    
    let duration = reader.duration() as f32 / spec.sample_rate as f32;
    Ok(duration)
}

/// WAVファイルから音声データを抽出
fn extract_audio_data_from_wav(wav_data: &[u8]) -> Result<Vec<f32>> {
    let mut reader = hound::WavReader::new(Cursor::new(wav_data))?;
    let spec = reader.spec();
    
    let mut samples = Vec::new();
    
    match (spec.bits_per_sample, spec.sample_format) {
        (16, SampleFormat::Int) => {
            for sample in reader.samples::<i16>() {
                samples.push(sample? as f32 / 32768.0);
            }
        },
        (24, SampleFormat::Int) => {
            for sample in reader.samples::<i32>() {
                samples.push(sample? as f32 / 8388608.0);
            }
        },
        (32, SampleFormat::Int) => {
            for sample in reader.samples::<i32>() {
                samples.push(sample? as f32 / 2147483648.0);
            }
        },
        (32, SampleFormat::Float) => {
            for sample in reader.samples::<f32>() {
                samples.push(sample?);
            }
        },
        (bits, _) => {
            return Err(anyhow!("非対応のビット幅: {}", bits));
        }
    }
    
    Ok(samples)
} 
