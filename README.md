# voilip - CLI音声入力ユーティリティ

voilipは、PC上での文書作成やチャット入力を高速化するための音声入力CLIユーティリティです。GUIを介さずにキーボード操作の流れを崩さずに音声入力できる環境を提供し、開発者やライターの生産性を向上させます。

## 機能

- **リアルタイム文字起こし**: マイク入力からOpenAI GPT-4o-Transcribe / Whisper-1 / Whisper.cppを使用して音声認識
- **クリップボード出力**: 認識結果を自動的にクリップボードにコピー
- **柔軟な録音制御**: 無音検知、Push-To-Talk、またはトグルキーによる制御
- **複合キー対応**: META+j、CTRL+aなどの複合キー（修飾キー+英数字）をサポート
- **視覚的フィードバック**: 録音開始・停止時のデスクトップ通知とログ表示
- **プラットフォーム対応**: Linux（X11/Wayland）とmacOSで同一コードベースが動作
- **オフラインFallback**: Whisper.cppによるローカル音声認識の選択も可能
- **音声処理の最適化**: 無音除去、可変速度再生をサポート

## インストール

### 依存関係

- Rustツールチェーン (rustc, cargo)
- 音声関連ライブラリ:
  - Linux: `libasound2-dev` (ALSA), `libx11-dev` (X11)
  - macOS: PortAudio (Homebrewで`portaudio`パッケージ)
- クリップボード関連ツール: 
  - Linux: `xclip` または `xsel` (X11), `wl-clipboard` (Wayland)
  - macOS: 標準機能
- OpenAI API Key (GPT-4oまたはWhisper-1使用時)

### ビルド方法

```bash
# リポジトリをクローン
git clone https://github.com/yourusername/voilip.git
cd voilip

# 依存関係のインストール (Ubuntu/Debian)
sudo apt-get install libasound2-dev libx11-dev xclip

# Wayland環境では以下も必要
sudo apt-get install wl-clipboard

# コンパイル
cargo build --release

# インストール (オプション)
cargo install --path .
```

### Nixを使用している場合

Nixユーザーの場合は、以下の依存関係を含む開発環境を使用できます:

```nix
# shell.nix または development環境に追加
pkgs.alsa-lib
pkgs.xorg.libX11
pkgs.xclip
```

Nix環境で実行:

```bash
nix develop path:/path/to/your/nixconfig
cargo build --release
```

### Whisper.cppサポート付きでビルド

```bash
cargo build --release --features whisper-cpp
```

## 使用方法

### 設定

初回実行時に設定ファイルが自動作成されます。設定は`config`コマンドで管理します：

```bash
# 現在の設定を表示
voilip config show

# APIキーを設定
voilip config set-api-key "your_api_key_here"

# トグルキーを設定（例: F9、CTRL+j、META+sなど）
voilip config set-toggle-key "CTRL+j"

# 言語を設定
voilip config set-language "ja"

# モデルを設定
voilip config set-model "gpt-4o-transcribe"

# 無音除去を有効/無効に設定
voilip config set-remove-silence true

# 再生速度を設定（例: 1.5倍速）
voilip config set-speed-factor 1.5
```

設定ファイルの保存先：
- Linux: `~/.config/voilip/config.json`
- macOS: `~/Library/Application Support/com.volment.voilip/config.json`

### 基本的な使い方

OpenAI GPT-4oで音声認識し、クリップボードにコピー:

```bash
voilip start
```

コマンドライン引数で設定を一時的に上書き:

```bash
# 言語を英語に変更して起動
voilip start --lang en

# トグルキーを指定して起動
voilip start --toggle "CTRL+j"

# Push-To-Talkモードで使用
voilip start --ptt "F10"

# 特定のモデルを指定
voilip start --model "whisper-1"
```

Whisper.cppを使用（オフラインモード）:

```bash
voilip start --engine whisper-cpp --whisper-cpp-path ~/bin/whisper --whisper-cpp-model ~/models/ggml-small.bin
```

### テストモード

WAVファイルから文字起こしをテスト:

```bash
voilip test --test-file sample.wav
```

## トグルキーの設定例

以下のような様々な組み合わせが利用可能です：

- ファンクションキー: `F9`, `F10`, `F12`
- 修飾キー + アルファベット: `CTRL+j`, `META+k`, `SUPER+s`, `ALT+z`, `ALTGR+a`
- 修飾キー + 数字: `SHIFT+1`, `CTRL+9`
- 修飾キー + ファンクションキー: `CTRL+F10`, `ALT+F4`
- 矢印キー: `UP`, `DOWN`, `LEFT`, `RIGHT`
- マルチメディアキー: `PLAYPAUSE`, `VOLUMEUP`, `VOLUMEDOWN`, `MUTE`
- 特殊キー: `HOME`, `END`, `PAGEUP`, `PAGEDOWN`, `INSERT`, `DELETE`

## 音声処理機能

- **無音除去**: 録音中の無音部分を自動的に削除し、意味のある音声だけを連結
- **速度調整**: 音声を1.1～1.5倍速など、好みの速度に調整可能
- **無音自動停止**: トグルモードで一定時間（デフォルト10秒）無音が続くと自動的に録音を停止

## システム要件

- OS: Linux (X11/Wayland) または macOS
- メモリ: 最小256MB（Whisper.cpp使用時はモデルにより最大2GB）
- ディスク: 約10MB（Whisper.cppモデル使用時は追加で100MB〜数GB）
- ネットワーク: OpenAI API使用時はインターネット接続が必要

## ライセンス

MITライセンス 
