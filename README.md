# voilip - CLI音声入力ユーティリティ

voilipは、PC上での文書作成やチャット入力を高速化するための音声入力CLIユーティリティです。GUIを介さずにキーボード操作の流れを崩さずに音声入力できる環境を提供し、開発者やライターの生産性を向上させます。

## 機能

- **リアルタイム文字起こし**: マイク入力からOpenAI GPT-4o-Transcribe / Whisper-1 / Whisper.cppを使用して音声認識
- **複数の出力モード**: クリップボードへのコピー、アクティブウィンドウへのタイピング、または両方を選択可能
- **柔軟な録音制御**: 無音検知、Push-To-Talk、またはトグルキーによる制御
- **プラットフォーム対応**: Linux（X11/Wayland）とmacOSで同一コードベースが動作
- **オフラインFallback**: Whisper.cppによるローカル音声認識の選択も可能

## インストール

### 依存関係

- Rustツールチェーン (rustc, cargo)
- 音声関連ライブラリ:
  - Linux: `libasound2-dev` (ALSA), `libx11-dev` (X11)
  - macOS: PortAudio (Homebrewで`portaudio`パッケージ)
- クリップボード関連ツール: 
  - Linux: `xclip` または `xsel` (X11), `wl-clipboard` (Wayland)
  - macOS: 標準機能
- キーストローク送信ツール:
  - Linux: `xdotool` (X11), `wtype` (Wayland)
  - macOS: 標準機能
- OpenAI API Key (GPT-4oまたはWhisper-1使用時)

### ビルド方法

```bash
# リポジトリをクローン
git clone https://github.com/yourusername/voilip.git
cd voilip

# 依存関係のインストール (Ubuntu/Debian)
sudo apt-get install libasound2-dev libx11-dev xclip xdotool

# Wayland環境では以下も必要
sudo apt-get install wl-clipboard wtype

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
pkgs.xdotool
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

まず`.env`ファイルを作成し、OpenAIのAPIキーを設定します:

```
OPENAI_API_KEY=your_api_key_here
```

### 基本的な使い方

OpenAI GPT-4oで音声認識し、クリップボードにコピー:

```bash
voilip start --mode clipboard --lang ja
```

Push-To-Talkモードでの使用:

```bash
voilip start --mode both --lang ja --ptt F10
```

Whisper.cppを使用（オフラインモード）:

```bash
voilip start --engine whisper-cpp --whisper-cpp-path ~/bin/whisper --whisper-cpp-model ~/models/ggml-small.bin
```

### テストモード

WAVファイルから文字起こしをテスト:

```bash
voilip test --test-file sample.wav --model whisper-1
```

## 設定

設定は、コマンドライン引数と`.env`ファイルを組み合わせて行います。

### 環境変数 (`.env`ファイル)

```
# 必須
OPENAI_API_KEY=your_api_key_here

# オプション
TOGGLE_START_KEY=F9
TOGGLE_STOP_KEY=F10
```

## システム要件

- OS: Linux (X11/Wayland) または macOS
- メモリ: 最小256MB（Whisper.cpp使用時はモデルにより最大2GB）
- ディスク: 約10MB（Whisper.cppモデル使用時は追加で100MB〜数GB）
- ネットワーク: OpenAI API使用時はインターネット接続が必要

## ライセンス

MITライセンス 