use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Sample, SampleFormat, Stream, SizedSample};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use std::collections::VecDeque;
use anyhow::{Result, anyhow};
use tracing::{info, warn, debug, error};
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::mpsc;
use rdev::{listen, Event, EventType, Key};
use std::thread;

use crate::config::{Config, RecordingMode};

const MAX_AMPLITUDE: f32 = 1.0;

/// 音声バッファ構造体
pub struct AudioBuffer {
    /// リングバッファ (音声データ保持用)
    buffer: Arc<Mutex<VecDeque<f32>>>,
    /// 最後に音声が検出された時間
    last_voice_activity: Arc<Mutex<Option<Instant>>>,
    /// 録音中フラグ
    is_recording: Arc<AtomicBool>,
    /// 音声データチャネル
    tx: mpsc::Sender<Vec<f32>>,
}

impl AudioBuffer {
    /// 新しいAudioBufferを作成
    pub fn new(capacity: usize, tx: mpsc::Sender<Vec<f32>>) -> Self {
        Self {
            buffer: Arc::new(Mutex::new(VecDeque::with_capacity(capacity))),
            last_voice_activity: Arc::new(Mutex::new(None)),
            is_recording: Arc::new(AtomicBool::new(false)),
            tx,
        }
    }

    /// 音声データをリングバッファに追加
    pub fn push_samples<T: Sample<Float = f32>>(&self, samples: &[T], config: &Config) -> Result<()> {
        let mut buffer = self.buffer.lock().map_err(|_| anyhow!("バッファロックエラー"))?;
        let mut last_activity = self.last_voice_activity.lock().map_err(|_| anyhow!("アクティビティロックエラー"))?;
        
        let is_recording = self.is_recording.load(Ordering::SeqCst);
        
        // 音声アクティビティ検出パラメータの取得
        let (silence_threshold, silence_duration_ms) = if let RecordingMode::VoiceActivity { silence_threshold, silence_duration_ms } = &config.recording_mode {
            (*silence_threshold, *silence_duration_ms)
        } else {
            (0.01, 1000) // デフォルト値
        };
        
        let mut has_voice = false;
        
        // サンプルをf32に変換してバッファに追加
        for &sample in samples {
            let sample_f32 = sample.to_float_sample();
            
            // バッファがキャパシティに達している場合は古いサンプルを削除
            if buffer.len() >= buffer.capacity() {
                buffer.pop_front();
            }
            buffer.push_back(sample_f32);
            
            // 音声アクティビティの検出
            if sample_f32.abs() > silence_threshold {
                has_voice = true;
            }
        }
        
        // 音声アクティビティの状態更新
        if has_voice {
            *last_activity = Some(Instant::now());
        }
        
        // RecordingModeがVoiceActivityの場合、無音検出で録音を停止
        if let RecordingMode::VoiceActivity { .. } = &config.recording_mode {
            if is_recording {
                // 無音が一定時間続いたら録音を停止
                if let Some(last_time) = *last_activity {
                    let silence_duration = Instant::now().duration_since(last_time);
                    if silence_duration > Duration::from_millis(silence_duration_ms.into()) {
                        debug!("無音を検出: {:?}", silence_duration);
                        self.stop_recording()?;
                    }
                }
            } else if has_voice {
                // 音声を検出したら録音を開始
                self.start_recording()?;
            }
        }
        
        Ok(())
    }

    /// 録音を開始
    pub fn start_recording(&self) -> Result<()> {
        if !self.is_recording.load(Ordering::SeqCst) {
            info!("録音を開始します");
            self.is_recording.store(true, Ordering::SeqCst);
            
            // バッファの内容をコピーして送信
            let buffer = self.buffer.lock().map_err(|_| anyhow!("バッファロックエラー"))?;
            let samples: Vec<f32> = buffer.iter().copied().collect();
            
            if !samples.is_empty() {
                // 非同期チャネルへ送信（非同期ランタイム外からも安全に送信）
                let tx = self.tx.clone();
                // 送信エラーは無視（受信側が閉じている場合）
                let _ = tx.try_send(samples);
            }
        }
        Ok(())
    }

    /// 録音を停止
    pub fn stop_recording(&self) -> Result<()> {
        if self.is_recording.load(Ordering::SeqCst) {
            info!("録音を停止します");
            self.is_recording.store(false, Ordering::SeqCst);
            
            // バッファの内容をコピーして送信
            let buffer = self.buffer.lock().map_err(|_| anyhow!("バッファロックエラー"))?;
            let samples: Vec<f32> = buffer.iter().copied().collect();
            
            if !samples.is_empty() {
                // 非同期チャネルへ送信（非同期ランタイム外からも安全に送信）
                let tx = self.tx.clone();
                // 送信エラーは無視（受信側が閉じている場合）
                let _ = tx.try_send(samples);
            }
        }
        Ok(())
    }

    /// 現在録音中かどうか
    pub fn is_recording(&self) -> bool {
        self.is_recording.load(Ordering::SeqCst)
    }
}

/// 音声キャプチャマネージャー
pub struct AudioCapture {
    config: Config,
    stream: Option<Stream>,
    audio_buffer: Arc<AudioBuffer>,
    key_handler_thread: Option<thread::JoinHandle<()>>,
}

impl AudioCapture {
    /// 新しいAudioCaptureを作成
    pub fn new(config: Config, tx: mpsc::Sender<Vec<f32>>) -> Self {
        // リングバッファの容量を計算（デフォルトで5秒分）
        let buffer_capacity = config.sample_rate as usize * config.channels as usize * 5;
        
        Self {
            config,
            stream: None,
            audio_buffer: Arc::new(AudioBuffer::new(buffer_capacity, tx)),
            key_handler_thread: None,
        }
    }

    /// 音声キャプチャを開始
    pub fn start(&mut self) -> Result<()> {
        let host = cpal::default_host();
        
        // 入力デバイスの取得
        let device = host.default_input_device()
            .ok_or_else(|| anyhow!("入力デバイスが見つかりません"))?;
        
        info!("入力デバイス: {:?}", device.name()?);
        
        // 入力設定の構築
        let config = cpal::StreamConfig {
            channels: self.config.channels,
            sample_rate: cpal::SampleRate(self.config.sample_rate),
            buffer_size: cpal::BufferSize::Default,
        };
        
        let audio_buffer = self.audio_buffer.clone();
        let app_config = self.config.clone();
        
        // エラーコールバック
        let err_fn = move |err| {
            error!("音声ストリームエラー: {}", err);
        };
        
        // サンプル形式ごとにストリームを構築
        let stream = match device.default_input_config()?.sample_format() {
            SampleFormat::F32 => self.build_stream::<f32>(&device, &config, audio_buffer.clone(), app_config.clone(), err_fn)?,
            SampleFormat::I16 => self.build_stream::<i16>(&device, &config, audio_buffer.clone(), app_config.clone(), err_fn)?,
            SampleFormat::U16 => self.build_stream::<u16>(&device, &config, audio_buffer.clone(), app_config.clone(), err_fn)?,
            _ => return Err(anyhow!("非対応のサンプル形式")),
        };
        
        // ストリームを開始
        stream.play()?;
        self.stream = Some(stream);
        
        info!("音声キャプチャを開始しました");
        Ok(())
    }

    /// 音声ストリームを構築
    fn build_stream<T>(
        &self,
        device: &cpal::Device,
        config: &cpal::StreamConfig,
        audio_buffer: Arc<AudioBuffer>,
        app_config: Config,
        err_fn: impl FnMut(cpal::StreamError) + Send + 'static,
    ) -> Result<Stream>
    where
        T: Sample<Float = f32> + Send + 'static + SizedSample,
    {
        let stream = device.build_input_stream(
            config,
            move |data: &[T], _: &cpal::InputCallbackInfo| {
                if let Err(e) = audio_buffer.push_samples(data, &app_config) {
                    error!("サンプル処理エラー: {}", e);
                }
            },
            err_fn,
            None,
        )?;
        
        Ok(stream)
    }

    /// PushToTalkモードの制御を設定
    pub fn setup_ptt_control(&mut self) -> Result<()> {
        if let RecordingMode::PushToTalk { key } = &self.config.recording_mode.clone() {
            info!("Push-To-Talk キー: {}", key);
            
            // キー名を設定
            let ptt_key = parse_key_name(key);
            let audio_buffer = self.audio_buffer.clone();
            
            // キー入力監視スレッドを作成
            let handle = thread::spawn(move || {
                let callback = move |event: Event| {
                    match event.event_type {
                        EventType::KeyPress(key_event) => {
                            if key_event == ptt_key {
                                debug!("PTTキー押下: {:?}", key_event);
                                if let Err(e) = audio_buffer.start_recording() {
                                    error!("録音開始エラー: {}", e);
                                }
                            }
                        }
                        EventType::KeyRelease(key_event) => {
                            if key_event == ptt_key {
                                debug!("PTTキー解放: {:?}", key_event);
                                if let Err(e) = audio_buffer.stop_recording() {
                                    error!("録音停止エラー: {}", e);
                                }
                            }
                        }
                        _ => {}
                    }
                };
                
                if let Err(error) = listen(callback) {
                    error!("キー監視エラー: {:?}", error);
                }
            });
            
            self.key_handler_thread = Some(handle);
        }
        
        Ok(())
    }

    /// トグルモードの制御を設定
    pub fn setup_toggle_control(&mut self) -> Result<()> {
        if let RecordingMode::Toggle { start_key, stop_key } = &self.config.recording_mode.clone() {
            info!("トグル開始キー: {}", start_key);
            if let Some(stop) = stop_key {
                info!("トグル停止キー: {}", stop);
            } else {
                info!("トグル停止キー: 同じ ({}, トグル動作)", start_key);
            }
            
            // キー名を設定
            let toggle_start_key = parse_key_name(start_key);
            let toggle_stop_key = stop_key.as_ref().map(|k| parse_key_name(k)).unwrap_or(toggle_start_key);
            let audio_buffer = self.audio_buffer.clone();
            let is_single_key = stop_key.is_none();
            
            // キー入力監視スレッドを作成
            let handle = thread::spawn(move || {
                let callback = move |event: Event| {
                    match event.event_type {
                        EventType::KeyPress(key_event) => {
                            if key_event == toggle_start_key {
                                debug!("トグル開始キー押下: {:?}", key_event);
                                if is_single_key {
                                    // 単一キーの場合は状態をトグル
                                    if audio_buffer.is_recording() {
                                        if let Err(e) = audio_buffer.stop_recording() {
                                            error!("録音停止エラー: {}", e);
                                        }
                                    } else {
                                        if let Err(e) = audio_buffer.start_recording() {
                                            error!("録音開始エラー: {}", e);
                                        }
                                    }
                                } else {
                                    // 開始キーと停止キーが別の場合
                                    if !audio_buffer.is_recording() {
                                        if let Err(e) = audio_buffer.start_recording() {
                                            error!("録音開始エラー: {}", e);
                                        }
                                    }
                                }
                            } else if key_event == toggle_stop_key && !is_single_key {
                                // 開始キーと停止キーが別の場合
                                debug!("トグル停止キー押下: {:?}", key_event);
                                if audio_buffer.is_recording() {
                                    if let Err(e) = audio_buffer.stop_recording() {
                                        error!("録音停止エラー: {}", e);
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                };
                
                if let Err(error) = listen(callback) {
                    error!("キー監視エラー: {:?}", error);
                }
            });
            
            self.key_handler_thread = Some(handle);
        }
        
        Ok(())
    }
    
    /// 録音開始
    pub fn start_recording(&self) -> Result<()> {
        self.audio_buffer.start_recording()
    }
    
    /// 録音停止
    pub fn stop_recording(&self) -> Result<()> {
        self.audio_buffer.stop_recording()
    }
    
    /// 録音中かどうか
    pub fn is_recording(&self) -> bool {
        self.audio_buffer.is_recording()
    }
    
    /// オーディオストリームを停止
    pub fn stop(&mut self) {
        if let Some(stream) = self.stream.take() {
            drop(stream);
            info!("音声キャプチャを停止しました");
        }
    }
}

impl Drop for AudioCapture {
    fn drop(&mut self) {
        self.stop();
    }
}

/// キー名をrdevのKeyに変換
fn parse_key_name(key_name: &str) -> Key {
    match key_name.to_uppercase().as_str() {
        "F1" => Key::F1,
        "F2" => Key::F2,
        "F3" => Key::F3,
        "F4" => Key::F4,
        "F5" => Key::F5,
        "F6" => Key::F6,
        "F7" => Key::F7,
        "F8" => Key::F8,
        "F9" => Key::F9,
        "F10" => Key::F10,
        "F11" => Key::F11,
        "F12" => Key::F12,
        "SHIFT" | "LSHIFT" => Key::ShiftLeft,
        "RSHIFT" => Key::ShiftRight,
        "CTRL" | "LCTRL" => Key::ControlLeft,
        "RCTRL" => Key::ControlRight,
        "ALT" | "LALT" => Key::Alt,
        "RALT" => Key::Alt,
        "META" | "SUPER" | "LMETA" | "LSUPER" => Key::Unknown(0xE05B), // Windows/Super key
        "RMETA" | "RSUPER" => Key::Unknown(0xE05C), // Right Windows/Super key
        "SPACE" => Key::Space,
        "TAB" => Key::Tab,
        "ESCAPE" | "ESC" => Key::Escape,
        _ => {
            warn!("未対応のキー名: {}、CapsLockにフォールバック", key_name);
            Key::CapsLock
        }
    }
} 