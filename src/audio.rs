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
    /// 無音除去フラグ
    remove_silence: bool,
    /// 速度倍率
    speed_factor: f32,
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
            remove_silence: true, // デフォルトで無音除去を有効化
            speed_factor: 1.1, // デフォルトで1.1倍速
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
        // すでに録音中なら何もしない
        if self.is_recording.load(Ordering::SeqCst) {
            return Ok(());
        }
        
        // 録音フラグをセット
        self.is_recording.store(true, Ordering::SeqCst);
        
        // 録音開始時間を記録
        let mut recording_start = self.recording_start_time.lock().map_err(|_| anyhow!("録音時間ロックエラー"))?;
        *recording_start = Some(Instant::now());
        
        // 最初の音声アクティビティ時間を設定
        let mut last_activity = self.last_voice_activity.lock().map_err(|_| anyhow!("アクティビティロックエラー"))?;
        *last_activity = Some(Instant::now());
        
        // 蓄積バッファをクリア
        let mut accumulated = self.accumulated_samples.lock().map_err(|_| anyhow!("蓄積バッファロックエラー"))?;
        accumulated.clear();
        
        info!("録音を開始しました");
        
        // 録音開始の通知を表示
        show_notification("voilip", "録音を開始しました 🎤")?;
        
        Ok(())
    }

    /// 録音を停止
    pub fn stop_recording(&self) -> Result<()> {
        // 録音中でなければ何もしない
        if !self.is_recording.load(Ordering::SeqCst) {
            return Ok(());
        }
        
        let mut buffer = self.buffer.lock().map_err(|_| anyhow!("バッファロックエラー"))?;
        
        // 録音フラグを解除
        self.is_recording.store(false, Ordering::SeqCst);
        
        // バッファが空でなければ処理を実行
        if !buffer.is_empty() {
            // バッファの内容をベクターに変換
            let mut samples: Vec<f32> = buffer.iter().copied().collect();
            
            // トグルモードの場合は蓄積バッファを使用
            let mut accumulated = self.accumulated_samples.lock().map_err(|_| anyhow!("蓄積バッファロックエラー"))?;
            if !accumulated.is_empty() {
                debug!("トグルモード: 蓄積バッファを使用 ({} サンプル)", accumulated.len());
                samples = accumulated.clone();
                accumulated.clear();
            }
            
            // 無音除去を適用
            if self.remove_silence && !samples.is_empty() {
                match self.remove_silence_from_samples(&samples) {
                    Ok(filtered) => samples = filtered,
                    Err(e) => error!("無音除去エラー: {}", e),
                }
            }
            
            // 速度変更を適用
            if self.speed_factor != 1.0 && !samples.is_empty() {
                match self.change_speed(&samples, self.speed_factor) {
                    Ok(speed_changed) => samples = speed_changed,
                    Err(e) => error!("速度変更エラー: {}", e),
                }
            }
            
            // バッファをクリア
            buffer.clear();
            
            // 非同期チャネルへ送信
            if !samples.is_empty() {
                let tx = self.tx.clone();
                let sample_duration_sec = samples.len() as f32 / 16000.0; // 16kHzサンプリング
                debug!("録音を送信: {:.2}秒 ({} サンプル)", sample_duration_sec, samples.len());
                
                let _ = tx.try_send(samples);
            }
        }
        
        info!("録音を停止しました");
        
        // 録音停止の通知を表示
        show_notification("voilip", "録音を停止しました ✓")?;
        
        Ok(())
    }

    /// 無音部分を除去して音声部分だけを連結する
    fn remove_silence_from_samples(&self, samples: &[f32]) -> Result<Vec<f32>> {
        let threshold = 0.01; // 無音判定の閾値
        let min_segment_len = 1600; // 最小音声セグメント長（0.1秒相当@16kHz）
        
        let mut result = Vec::new();
        let mut current_segment = Vec::new();
        let mut is_speech = false;
        
        for &sample in samples {
            if sample.abs() > threshold {
                is_speech = true;
                current_segment.push(sample);
            } else if is_speech {
                current_segment.push(sample);
                
                // 無音が続く場合、セグメントを終了
                if current_segment.len() > 800 && current_segment.iter().rev().take(800).all(|s| s.abs() <= threshold) {
                    // 末尾の無音を除去
                    let speech_end = current_segment.len() - current_segment.iter().rev()
                        .position(|s| s.abs() > threshold)
                        .unwrap_or(current_segment.len());
                    
                    if speech_end > min_segment_len {
                        // 有効な音声セグメントを結果に追加
                        result.extend_from_slice(&current_segment[0..speech_end]);
                    }
                    
                    current_segment.clear();
                    is_speech = false;
                }
            }
        }
        
        // 最後のセグメントを処理
        if !current_segment.is_empty() && current_segment.len() > min_segment_len {
            // 末尾の無音を除去
            let speech_end = current_segment.len() - current_segment.iter().rev()
                .position(|s| s.abs() > threshold)
                .unwrap_or(current_segment.len());
            
            if speech_end > min_segment_len {
                result.extend_from_slice(&current_segment[0..speech_end]);
            }
        }
        
        Ok(result)
    }
    
    /// 音声の速度を変更する
    fn change_speed(&self, samples: &[f32], speed_factor: f32) -> Result<Vec<f32>> {
        if speed_factor == 1.0 {
            return Ok(samples.to_vec());
        }
        
        let new_len = (samples.len() as f32 / speed_factor) as usize;
        let mut result = Vec::with_capacity(new_len);
        
        for i in 0..new_len {
            let src_idx = (i as f32 * speed_factor) as usize;
            if src_idx < samples.len() {
                result.push(samples[src_idx]);
            }
        }
        
        Ok(result)
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
        if let RecordingMode::PushToTalk { key } = &self.config.recording_mode {
            info!("Push-To-Talk キー: {}", key);
            
            // キー名を設定
            let ptt_key = parse_key_name(key);
            let audio_buffer = self.audio_buffer.clone();
            
            // 複合キーの処理用
            let is_composite = key.contains("+");
            let modifier_key = if is_composite {
                let parts: Vec<&str> = key.split("+").collect();
                let modifier = parts[0].trim().to_uppercase();
                match modifier.as_str() {
                    "CTRL" => Some(Key::ControlLeft),
                    "ALT" => Some(Key::Alt),
                    "SHIFT" => Some(Key::ShiftLeft),
                    "META" | "SUPER" => Some(Key::Unknown(0xE05B)),
                    _ => None
                }
            } else {
                None
            };
            
            // キー情報をクローンしてスレッドに渡す
            let key_clone = key.clone();
            
            // 修飾キーの状態を追跡
            let modifier_pressed = Arc::new(AtomicBool::new(false));
            
            // キー入力監視スレッドを作成
            let handle = thread::spawn(move || {
                let callback = move |event: Event| {
                    match event.event_type {
                        EventType::KeyPress(key_event) => {
                            // 修飾キーの状態を更新
                            if let Some(mod_key) = modifier_key {
                                if key_event == mod_key {
                                    modifier_pressed.store(true, Ordering::SeqCst);
                                }
                            }
                            
                            // PTTキーの処理
                            if key_event == ptt_key {
                                // 複合キーの場合は修飾キーが押されていることを確認
                                let should_activate = if is_composite {
                                    modifier_pressed.load(Ordering::SeqCst)
                                } else {
                                    true
                                };
                                
                                if should_activate {
                                    debug!("PTTキー押下: {}{:?}", 
                                        if is_composite { format!("{:?}+", modifier_key.unwrap()) } else { "".to_string() }, 
                                        key_event);
                                    if let Err(e) = audio_buffer.start_recording() {
                                        error!("録音開始エラー: {}", e);
                                    } else {
                                        // キー入力フィードバック（録音開始）
                                        let key_name = if is_composite {
                                            let parts: Vec<&str> = key_clone.split("+").collect();
                                            format!("{}+{}", parts[0].trim(), parts[1].trim())
                                        } else {
                                            key_clone.to_string()
                                        };
                                        info!("PTTキー {} で録音を開始しました", key_name);
                                    }
                                }
                            }
                        }
                        EventType::KeyRelease(key_event) => {
                            // 修飾キーのリリース
                            if let Some(mod_key) = modifier_key {
                                if key_event == mod_key {
                                    modifier_pressed.store(false, Ordering::SeqCst);
                                    
                                    // 修飾キーが離されたら録音を停止（複合キーの場合）
                                    if is_composite && audio_buffer.is_recording() {
                                        debug!("修飾キーリリースでPTT停止");
                                        if let Err(e) = audio_buffer.stop_recording() {
                                            error!("録音停止エラー: {}", e);
                                        } else {
                                            // キー入力フィードバック（録音停止）
                                            let parts: Vec<&str> = key_clone.split("+").collect();
                                            let key_name = format!("{}+{}", parts[0].trim(), parts[1].trim());
                                            info!("修飾キーリリースで録音を停止しました ({})", key_name);
                                        }
                                    }
                                }
                            }
                            
                            // メインキーのリリース
                            if key_event == ptt_key {
                                // 複合キーでない場合、またはPTTがリリースされた場合に停止
                                if !is_composite || !modifier_pressed.load(Ordering::SeqCst) {
                                    debug!("PTTキー解放: {:?}", key_event);
                                    if let Err(e) = audio_buffer.stop_recording() {
                                        error!("録音停止エラー: {}", e);
                                    } else {
                                        // キー入力フィードバック（録音停止）
                                        let key_name = if is_composite {
                                            let parts: Vec<&str> = key_clone.split("+").collect();
                                            format!("{}+{}", parts[0].trim(), parts[1].trim())
                                        } else {
                                            key_clone.to_string()
                                        };
                                        info!("PTTキー {} のリリースで録音を停止しました", key_name);
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

    /// トグルモードの制御を設定
    pub fn setup_toggle_control(&mut self) -> Result<()> {
        if let RecordingMode::Toggle { key } = &self.config.recording_mode {
            info!("トグルキー: {}", key);
            
            // キー名を設定
            let toggle_key = parse_key_name(key);
            let audio_buffer = self.audio_buffer.clone();
            
            // 複合キーの処理用
            let is_composite = key.contains("+");
            let modifier_key = if is_composite {
                let parts: Vec<&str> = key.split("+").collect();
                let modifier = parts[0].trim().to_uppercase();
                match modifier.as_str() {
                    "CTRL" => Some(Key::ControlLeft),
                    "ALT" => Some(Key::Alt),
                    "SHIFT" => Some(Key::ShiftLeft),
                    "META" | "SUPER" => Some(Key::Unknown(0xE05B)),
                    _ => None
                }
            } else {
                None
            };
            
            // キー情報をクローンしてスレッドに渡す
            let key_clone = key.clone();
            
            // 修飾キーの状態を追跡
            let modifier_pressed = Arc::new(AtomicBool::new(false));
            
            // キー入力監視スレッドを作成
            let handle = thread::spawn(move || {
                let callback = move |event: Event| {
                    match event.event_type {
                        EventType::KeyPress(key_event) => {
                            // 修飾キーの状態を更新
                            if let Some(mod_key) = modifier_key {
                                if key_event == mod_key {
                                    modifier_pressed.store(true, Ordering::SeqCst);
                                }
                            }
                            
                            // トグルキーの処理
                            if key_event == toggle_key {
                                // 複合キーの場合は修飾キーが押されていることを確認
                                let should_toggle = if is_composite {
                                    modifier_pressed.load(Ordering::SeqCst)
                                } else {
                                    true
                                };
                                
                                if should_toggle {
                                    debug!("トグルキー押下: {}{:?}", 
                                        if is_composite { format!("{:?}+", modifier_key.unwrap()) } else { "".to_string() }, 
                                        key_event);
                                    
                                    // 状態をトグル
                                    if audio_buffer.is_recording() {
                                        if let Err(e) = audio_buffer.stop_recording() {
                                            error!("録音停止エラー: {}", e);
                                        } else {
                                            // キー入力フィードバック（録音停止）
                                            let key_name = if is_composite {
                                                let parts: Vec<&str> = key_clone.split("+").collect();
                                                format!("{}+{}", parts[0].trim(), parts[1].trim())
                                            } else {
                                                key_clone.to_string()
                                            };
                                            info!("トグルキー {} で録音を停止しました", key_name);
                                        }
                                    } else {
                                        if let Err(e) = audio_buffer.start_recording() {
                                            error!("録音開始エラー: {}", e);
                                        } else {
                                            // キー入力フィードバック（録音開始）
                                            let key_name = if is_composite {
                                                let parts: Vec<&str> = key_clone.split("+").collect();
                                                format!("{}+{}", parts[0].trim(), parts[1].trim())
                                            } else {
                                                key_clone.to_string()
                                            };
                                            info!("トグルキー {} で録音を開始しました", key_name);
                                        }
                                    }
                                }
                            }
                        }
                        EventType::KeyRelease(key_event) => {
                            // 修飾キーのリリース
                            if let Some(mod_key) = modifier_key {
                                if key_event == mod_key {
                                    modifier_pressed.store(false, Ordering::SeqCst);
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
    // 複合キーの場合は単一キーとして扱う
    if key_name.contains("+") {
        let parts: Vec<&str> = key_name.split("+").collect();
        let modifier = parts[0].trim().to_uppercase();
        let key = parts[1].trim();
        
        // 主要なキーを解析
        // 小文字であれば英字キーとして処理
        if key.len() == 1 {
            let c = key.chars().next().unwrap();
            if c.is_ascii_alphabetic() {
                // アルファベットキー
                return match c {
                    'a' | 'A' => Key::KeyA,
                    'b' | 'B' => Key::KeyB,
                    'c' | 'C' => Key::KeyC,
                    'd' | 'D' => Key::KeyD,
                    'e' | 'E' => Key::KeyE,
                    'f' | 'F' => Key::KeyF,
                    'g' | 'G' => Key::KeyG,
                    'h' | 'H' => Key::KeyH,
                    'i' | 'I' => Key::KeyI,
                    'j' | 'J' => Key::KeyJ,
                    'k' | 'K' => Key::KeyK,
                    'l' | 'L' => Key::KeyL,
                    'm' | 'M' => Key::KeyM,
                    'n' | 'N' => Key::KeyN,
                    'o' | 'O' => Key::KeyO,
                    'p' | 'P' => Key::KeyP,
                    'q' | 'Q' => Key::KeyQ,
                    'r' | 'R' => Key::KeyR,
                    's' | 'S' => Key::KeyS,
                    't' | 'T' => Key::KeyT,
                    'u' | 'U' => Key::KeyU,
                    'v' | 'V' => Key::KeyV,
                    'w' | 'W' => Key::KeyW,
                    'x' | 'X' => Key::KeyX,
                    'y' | 'Y' => Key::KeyY,
                    'z' | 'Z' => Key::KeyZ,
                    _ => {
                        warn!("未対応の文字キー: {}, F9にフォールバック", c);
                        Key::F9
                    }
                };
            } else if c.is_ascii_digit() {
                // 数字キー
                return match c {
                    '0' => Key::Num0,
                    '1' => Key::Num1,
                    '2' => Key::Num2,
                    '3' => Key::Num3,
                    '4' => Key::Num4,
                    '5' => Key::Num5,
                    '6' => Key::Num6,
                    '7' => Key::Num7,
                    '8' => Key::Num8,
                    '9' => Key::Num9,
                    _ => Key::F9  // ここには来ないはず
                };
            }
        }
        
        // ファンクションキーや特殊キー
        match key.to_uppercase().as_str() {
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
            "SPACE" => Key::Space,
            "TAB" => Key::Tab,
            "ESCAPE" | "ESC" => Key::Escape,
            _ => {
                warn!("未対応のキー名: {}、F9にフォールバック", key);
                Key::F9
            }
        }
    } else {
        // 単一キーの場合
        // アルファベットキーの処理を追加
        if key_name.len() == 1 {
            let c = key_name.chars().next().unwrap();
            if c.is_ascii_alphabetic() {
                return match c {
                    'a' | 'A' => Key::KeyA,
                    'b' | 'B' => Key::KeyB,
                    'c' | 'C' => Key::KeyC,
                    'd' | 'D' => Key::KeyD,
                    'e' | 'E' => Key::KeyE,
                    'f' | 'F' => Key::KeyF,
                    'g' | 'G' => Key::KeyG,
                    'h' | 'H' => Key::KeyH,
                    'i' | 'I' => Key::KeyI,
                    'j' | 'J' => Key::KeyJ,
                    'k' | 'K' => Key::KeyK,
                    'l' | 'L' => Key::KeyL,
                    'm' | 'M' => Key::KeyM,
                    'n' | 'N' => Key::KeyN,
                    'o' | 'O' => Key::KeyO,
                    'p' | 'P' => Key::KeyP,
                    'q' | 'Q' => Key::KeyQ,
                    'r' | 'R' => Key::KeyR,
                    's' | 'S' => Key::KeyS,
                    't' | 'T' => Key::KeyT,
                    'u' | 'U' => Key::KeyU,
                    'v' | 'V' => Key::KeyV,
                    'w' | 'W' => Key::KeyW,
                    'x' | 'X' => Key::KeyX,
                    'y' | 'Y' => Key::KeyY,
                    'z' | 'Z' => Key::KeyZ,
                    _ => Key::CapsLock
                };
            } else if c.is_ascii_digit() {
                return match c {
                    '0' => Key::Num0,
                    '1' => Key::Num1,
                    '2' => Key::Num2,
                    '3' => Key::Num3,
                    '4' => Key::Num4,
                    '5' => Key::Num5,
                    '6' => Key::Num6,
                    '7' => Key::Num7,
                    '8' => Key::Num8,
                    '9' => Key::Num9,
                    _ => Key::CapsLock  // ここには来ないはず
                };
            }
        }

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
            "ALTGR" => Key::AltGr,
            "META" | "SUPER" | "LMETA" | "LSUPER" => Key::Unknown(0xE05B), // Windows/Super key
            "RMETA" | "RSUPER" => Key::Unknown(0xE05C), // Right Windows/Super key
            "SPACE" => Key::Space,
            "TAB" => Key::Tab,
            "ESCAPE" | "ESC" => Key::Escape,
            // マルチメディアキー（rdevでサポートされていないのでUnknownとして処理）
            "PLAY" | "PAUSE" | "PLAYPAUSE" => Key::Unknown(0xE022), // Play/Pause
            "STOP" => Key::Unknown(0xE024), // Media Stop
            "NEXT" | "NEXTTRACK" => Key::Unknown(0xE019), // Next Track
            "PREV" | "PREVTRACK" => Key::Unknown(0xE010), // Previous Track
            "VOLUMEUP" => Key::Unknown(0xE030), // Volume Up
            "VOLUMEDOWN" => Key::Unknown(0xE02E), // Volume Down
            "MUTE" => Key::Unknown(0xE020), // Volume Mute
            // その他の特殊キー
            "PRINT" | "PRINTSCREEN" => Key::PrintScreen,
            "SCROLLLOCK" => Key::ScrollLock,
            "PAUSE" => Key::Pause,
            "INSERT" => Key::Insert,
            "HOME" => Key::Home,
            "PAGEUP" => Key::PageUp,
            "DELETE" => Key::Delete,
            "END" => Key::End,
            "PAGEDOWN" => Key::PageDown,
            "RIGHT" => Key::RightArrow,
            "LEFT" => Key::LeftArrow,
            "DOWN" => Key::DownArrow,
            "UP" => Key::UpArrow,
            _ => {
                warn!("未対応のキー名: {}、CapsLockにフォールバック", key_name);
                Key::CapsLock
            }
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
