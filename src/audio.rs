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
use std::process::Command;

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
    /// 録音開始時間
    recording_start_time: Arc<Mutex<Option<Instant>>>,
    /// トグルモード用蓄積バッファ
    accumulated_samples: Arc<Mutex<Vec<f32>>>,
    /// トグルモードの無音時間しきい値（秒）
    toggle_silence_threshold_sec: u32,
}

impl AudioBuffer {
    /// 新しいAudioBufferを作成
    pub fn new(capacity: usize, tx: mpsc::Sender<Vec<f32>>) -> Self {
        Self {
            buffer: Arc::new(Mutex::new(VecDeque::with_capacity(capacity))),
            last_voice_activity: Arc::new(Mutex::new(None)),
            is_recording: Arc::new(AtomicBool::new(false)),
            tx,
            recording_start_time: Arc::new(Mutex::new(None)),
            accumulated_samples: Arc::new(Mutex::new(Vec::new())),
            toggle_silence_threshold_sec: 10, // トグルモードで10秒無音で自動停止
        }
    }

    /// 音声データをリングバッファに追加
    pub fn push_samples<T: Sample<Float = f32>>(&self, samples: &[T], config: &Config) -> Result<()> {
        let mut buffer = self.buffer.lock().map_err(|_| anyhow!("バッファロックエラー"))?;
        let mut last_activity = self.last_voice_activity.lock().map_err(|_| anyhow!("アクティビティロックエラー"))?;
        let mut recording_start = self.recording_start_time.lock().map_err(|_| anyhow!("録音時間ロックエラー"))?;
        
        let is_recording = self.is_recording.load(Ordering::SeqCst);
        
        // 音声アクティビティ検出パラメータの取得
        let (silence_threshold, silence_duration_ms) = if let RecordingMode::VoiceActivity { silence_threshold, silence_duration_ms } = &config.recording_mode {
            (*silence_threshold, *silence_duration_ms)
        } else {
            (0.01, 1000) // デフォルト値
        };
        
        // トグルモードでは雑音を誤検出しないよう、閾値を高めに設定
        let actual_threshold = match &config.recording_mode {
            RecordingMode::Toggle { .. } => silence_threshold * 2.5, // トグルモードではより高い閾値を使用
            _ => silence_threshold,
        };
        
        let mut has_voice = false;
        let mut significant_voice = false; // 実際に意味のある音声かどうか
        let mut max_amplitude = 0.0f32;
        
        // サンプルをf32に変換してバッファに追加
        for &sample in samples {
            let sample_f32 = sample.to_float_sample();
            
            // バッファがキャパシティに達している場合は古いサンプルを削除
            if buffer.len() >= buffer.capacity() {
                buffer.pop_front();
            }
            buffer.push_back(sample_f32);
            
            // 音声アクティビティの検出と最大振幅の記録
            let amplitude = sample_f32.abs();
            if amplitude > actual_threshold {
                has_voice = true;
                if amplitude > max_amplitude {
                    max_amplitude = amplitude;
                }
            }
            
            // より強い音声（意味のある音声）の検出
            if amplitude > actual_threshold * 2.0 {
                significant_voice = true;
            }
        }
        
        // 録音中かつトグルモードの場合は蓄積バッファにも追加
        if is_recording && matches!(config.recording_mode, RecordingMode::Toggle { .. }) {
            // 有意な音声がある場合のみ追加（雑音は含めない）
            if significant_voice {
                let mut accumulated = self.accumulated_samples.lock().map_err(|_| anyhow!("蓄積バッファロックエラー"))?;
                for &sample in samples {
                    accumulated.push(sample.to_float_sample());
                }
                
                // 意味のある音声がある場合はログを出力
                static mut LAST_LOG_TIME: Option<Instant> = None;
                let now = Instant::now();
                
                unsafe {
                    if LAST_LOG_TIME.is_none() || now.duration_since(LAST_LOG_TIME.unwrap()).as_millis() > 500 {
                        debug!("トグルモード: 音声アクティビティを検出 (最大振幅: {:.5})", max_amplitude);
                        LAST_LOG_TIME = Some(now);
                    }
                }
            } else if max_amplitude > 0.005 {
                // 弱い音声も蓄積（ただしノイズは除外）
                let mut accumulated = self.accumulated_samples.lock().map_err(|_| anyhow!("蓄積バッファロックエラー"))?;
                for &sample in samples {
                    accumulated.push(sample.to_float_sample());
                }
            }
        }
        
        // 音声アクティビティの状態更新 - significant_voiceを使用
        if significant_voice {
            *last_activity = Some(Instant::now());
        }
        
        // 録音時間の確認（トグルモード以外で最大録音時間を超えたら送信）
        if is_recording {
            if let Some(start_time) = *recording_start {
                if let Some(max_duration) = config.max_recording_duration_sec {
                    let current_duration = Instant::now().duration_since(start_time);
                    
                    // トグルモード以外で、かつ最大録音時間を超えた場合
                    if let RecordingMode::VoiceActivity { .. } | RecordingMode::PushToTalk { .. } = &config.recording_mode {
                        if current_duration.as_secs() >= max_duration as u64 {
                            debug!("最大録音時間に達しました（{} 秒）", max_duration);
                            
                            // バッファを送信
                            let samples: Vec<f32> = buffer.iter().copied().collect();
                            if !samples.is_empty() {
                                // 非同期チャネルへ送信
                                let tx = self.tx.clone();
                                let _ = tx.try_send(samples);
                            }
                            
                            // 録音開始時間をリセット
                            *recording_start = Some(Instant::now());
                            
                            // バッファをクリア
                            buffer.clear();
                            
                            // トグルモード以外で録音継続中なら終了
                            if let RecordingMode::VoiceActivity { .. } = &config.recording_mode {
                                // 無音状態なら録音停止
                                if !has_voice {
                                    self.stop_recording()?;
                                }
                            }
                        }
                    }
                }
            }
        }
        
        // RecordingModeがVoiceActivityの場合のみ、無音検出で録音を停止
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
        } else if let RecordingMode::Toggle { .. } = &config.recording_mode {
            // トグルモードでの無音検出と処理
            if is_recording {
                if let Some(last_time) = *last_activity {
                    let silence_duration = Instant::now().duration_since(last_time);
                    
                    // トグルモードでも一定時間以上無音が続いたら自動的に録音を停止
                    if silence_duration > Duration::from_secs(self.toggle_silence_threshold_sec as u64) {
                        // 蓄積バッファの確認
                        let accumulated = self.accumulated_samples.lock().map_err(|_| anyhow!("蓄積バッファロックエラー"))?;
                        
                        // 蓄積バッファにデータがある場合のみ処理
                        if !accumulated.is_empty() {
                            debug!("トグルモード: {}秒間無音が続いたため録音を自動停止します", self.toggle_silence_threshold_sec);
                            
                            // 蓄積バッファをクローンしてロックを解放
                            let samples_to_send = accumulated.clone();
                            
                            // 録音状態のフラグを直接操作せず、stop_recording経由で処理
                            if is_recording {
                                // 録音状態を停止
                                info!("録音を停止します（無音自動停止）");
                                let _ = show_notification("音声入力", "録音を停止しました（無音自動停止）");
                                
                                // フラグを直接更新
                                self.is_recording.store(false, Ordering::SeqCst);
                                *recording_start = None;
                                
                                // 蓄積バッファのデータを送信
                                debug!("トグルモード: 蓄積バッファからサンプル送信 ({} サンプル)", samples_to_send.len());
                                let tx = self.tx.clone();
                                let _ = tx.try_send(samples_to_send);
                                
                                // バッファをクリア（次の録音のため）
                                let mut accumulated = self.accumulated_samples.lock().map_err(|_| anyhow!("蓄積バッファロックエラー"))?;
                                accumulated.clear();
                            }
                        } else {
                            // 蓄積バッファが空の場合は単に録音状態を停止
                            info!("録音を停止します（無音自動停止・データなし）");
                            let _ = show_notification("音声入力", "録音を停止しました（無音自動停止）");
                            self.is_recording.store(false, Ordering::SeqCst);
                            *recording_start = None;
                        }
                    }
                }
                
                if significant_voice {
                    debug!("トグルモード: 有効な音声を検出しました");
                }
            }
        }
        
        Ok(())
    }

    /// 録音を開始
    pub fn start_recording(&self) -> Result<()> {
        if !self.is_recording.load(Ordering::SeqCst) {
            info!("録音を開始します");
            // 通知を表示
            let _ = show_notification("音声入力", "録音を開始しました");
            
            // 録音開始時間を設定
            let mut recording_start = self.recording_start_time.lock().map_err(|_| anyhow!("録音時間ロックエラー"))?;
            *recording_start = Some(Instant::now());
            
            // バッファをクリア（録音開始時に空の状態から始める）
            let mut buffer = self.buffer.lock().map_err(|_| anyhow!("バッファロックエラー"))?;
            buffer.clear();
            
            // 蓄積バッファをクリア
            let mut accumulated = self.accumulated_samples.lock().map_err(|_| anyhow!("蓄積バッファロックエラー"))?;
            accumulated.clear();
            
            self.is_recording.store(true, Ordering::SeqCst);
            
            // 録音開始時に送信しない（音声データを受信してから送信する）
        }
        Ok(())
    }

    /// 録音を停止
    pub fn stop_recording(&self) -> Result<()> {
        if self.is_recording.load(Ordering::SeqCst) {
            info!("録音を停止します");
            // 通知を表示
            let _ = show_notification("音声入力", "録音を停止しました");
            
            // 録音開始時間をリセット
            let mut recording_start = self.recording_start_time.lock().map_err(|_| anyhow!("録音時間ロックエラー"))?;
            *recording_start = None;
            
            self.is_recording.store(false, Ordering::SeqCst);
            
            // トグルモードの場合は蓄積バッファを送信
            let accumulated = self.accumulated_samples.lock().map_err(|_| anyhow!("蓄積バッファロックエラー"))?;
            if !accumulated.is_empty() {
                debug!("トグルモード: 蓄積バッファからサンプル送信 ({} サンプル)", accumulated.len());
                // 非同期チャネルへ送信
                let tx = self.tx.clone();
                let samples = accumulated.clone();
                // 送信エラーは無視（受信側が閉じている場合）
                let _ = tx.try_send(samples);
                
                // バッファをクリア (追加)
                drop(accumulated); // 現在のロックを解放
                let mut accumulated_mut = self.accumulated_samples.lock().map_err(|_| anyhow!("蓄積バッファロックエラー"))?;
                accumulated_mut.clear();
            } else {
                // 蓄積バッファが空の場合は通常バッファから送信
                let buffer = self.buffer.lock().map_err(|_| anyhow!("バッファロックエラー"))?;
                let samples: Vec<f32> = buffer.iter().copied().collect();
                
                if !samples.is_empty() {
                    // 非同期チャネルへ送信（非同期ランタイム外からも安全に送信）
                    let tx = self.tx.clone();
                    // 送信エラーは無視（受信側が閉じている場合）
                    let _ = tx.try_send(samples);
                }
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
        // リングバッファの容量を計算
        let buffer_capacity = match config.recording_mode {
            // トグルモードではより大きなバッファ容量を確保（5分相当）
            RecordingMode::Toggle { .. } => config.sample_rate as usize * config.channels as usize * 300,
            // その他のモードは従来通り5秒分
            _ => config.sample_rate as usize * config.channels as usize * 5,
        };
        
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
        if let RecordingMode::Toggle { key } = &self.config.recording_mode.clone() {
            info!("トグルキー: {}", key);
            
            // キー名を設定
            let toggle_key = parse_key_name(key);
            let audio_buffer = self.audio_buffer.clone();
            
            // キー入力監視スレッドを作成
            let handle = thread::spawn(move || {
                let callback = move |event: Event| {
                    match event.event_type {
                        EventType::KeyPress(key_event) => {
                            if key_event == toggle_key {
                                debug!("トグルキー押下: {:?}", key_event);
                                
                                // 状態をトグル
                                if audio_buffer.is_recording() {
                                    if let Err(e) = audio_buffer.stop_recording() {
                                        error!("録音停止エラー: {}", e);
                                    }
                                } else {
                                    if let Err(e) = audio_buffer.start_recording() {
                                        error!("録音開始エラー: {}", e);
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

/// デスクトップ通知を表示
pub fn show_notification(title: &str, message: &str) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        // LinuxではnotifySendを使用
        if let Ok(status) = Command::new("which")
            .arg("notify-send")
            .output()
        {
            if status.status.success() {
                let _ = Command::new("notify-send")
                    .arg(title)
                    .arg(message)
                    .spawn();
                return Ok(());
            }
        }
    }
    
    #[cfg(target_os = "macos")]
    {
        // macOSではosascriptを使用
        let script = format!(
            "display notification \"{}\" with title \"{}\"",
            message.replace("\"", "\\\""),
            title.replace("\"", "\\\"")
        );
        
        let _ = Command::new("osascript")
            .args(["-e", &script])
            .spawn();
        
        return Ok(());
    }
    
    // 対応プラットフォームがない場合や通知コマンドがない場合は警告だけ出して続行
    warn!("通知機能を利用できません");
    Ok(())
} 
