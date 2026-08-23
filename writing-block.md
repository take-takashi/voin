# クロスプラットフォーム高精度音声入力ツール設計書

- 想定ファイル名: `voice-input-design.md`
- ステータス: 実装着手前の設計案
- 主言語: Rust
- 対象OS: macOS、Windows、Linux
- 初期用途: 現在フォーカス中の入力欄、Herdrのペイン、Tmuxペイン、標準出力への音声入力

## 1. 概要

本ツールは、ローカルで録音した音声を `whisper.cpp` で文字起こしし、整形したテキストを任意の入力先へ渡すデスクトップ向け音声入力ツールである。

Whisperの認識処理、音声入力、テキスト出力を分離することで、次の要件を満たす。

- 音声データを外部サービスへ送信せず、ローカルで完結できる。
- macOS、Windows、Linuxで共通のコア処理を利用できる。
- 音声録音、文字起こし、後処理、出力先を個別に差し替えられる。
- CLIだけでも利用でき、常駐GUIを追加してもコアを変更しなくてよい。
- Herdr、Tmux、クリップボード、標準出力などを同じ出力抽象化で扱える。

基本フローは次のとおりである。

```text
グローバルホットキー
        │
        ▼
    Recorder ── 音声バッファ
        │
        ▼
  Transcriber ── Whisper文字列
        │
        ▼
 PostProcessor ── 入力用テキスト
        │
        ▼
    TextSink
        ├─ Clipboard
        ├─ Clipboard + 自動paste
        ├─ HerdrSink
        ├─ TmuxSink
        └─ StdoutSink
```

## 2. 目的と非目標

### 2.1 目的

#### 機能要件

- ホットキーの押下中、または開始・停止操作の間だけ録音する。
- 録音終了後に、音声全体をまとめて文字起こしする。
- 日本語、英語、および日本語と英語が混在する入力を扱う。
- Whisperの初期プロンプト、言語、モデル、温度などを設定できる。
- テキストをクリップボードへコピーできる。
- クリップボードへコピーした後、対象アプリへ自動pasteできる。
- Herdrのペインへテキストを渡せる。
- Tmuxの指定ペインへテキストを渡せる。
- CLIから標準出力へテキストを出力できる。
- 認識、出力、権限、デバイスの失敗理由を利用者が確認できる。

#### 非機能要件

- 音声データは、明示的に設定しない限りディスクへ保存しない。
- 文字起こしは、初期対象ではローカルで完結する。
- コア機能をOS依存コードから分離する。
- 長時間常駐しても、録音状態やモデル状態が不整合にならない。
- CLI単体で、GUIなしの自動化とデバッグができる。

### 2.2 非目標

初期リリースでは、次を対象外とする。

- リアルタイム字幕や逐次確定文字列の表示
- 音声の話者分離
- 音声ファイルの編集、再生、管理
- 高度なLLMによる意味変換や要約
- 自動的な英訳、コード生成、プロンプト最適化
- Wayland環境での、全アプリケーションに対する無条件のキー入力注入
- 複数の利用者またはサーバー間でのモデル共有
- 外部APIを標準動作とするクラウド文字起こし

## 3. 設計方針

### 3.1 中心はパイプラインである

Whisperをアプリケーションの中心に置かず、次のデータ変換を中心に設計する。

```text
AudioSource
  → AudioBuffer
  → Transcript
  → ProcessedText
  → TextSink
```

Whisperは `Transcriber` の一実装に限定する。そのため、将来、別のローカルモデル、OS標準音声認識、外部APIを追加しても、録音と出力の実装は変更しない。

### 3.2 抽象化は交換理由に合わせて分ける

| 抽象 | 変更理由 | 代表的な実装 |
|---|---|---|
| `Recorder` | 音声デバイス、OSの音声API、録音制御 | `CpalRecorder` |
| `Transcriber` | モデル、推論ランタイム、ローカル/外部実行 | `WhisperCppTranscriber` |
| `PostProcessor` | 句読点、辞書、表記統一、ユーザー処理 | `PipelinePostProcessor` |
| `TextSink` | 入力先、フォーカス、IPC、端末 | `ClipboardSink`、`HerdrSink`、`TmuxSink`、`StdoutSink` |
| `HotkeySource` | OSのグローバルホットキー、GUIイベント | `GlobalHotkeySource`、`CliCommandSource` |

### 3.3 デフォルトは安全な出力から始める

自動pasteは誤入力の影響が大きいため、初期設定では次の順を推奨する。

1. `stdout`
2. クリップボードへのコピー
3. クリップボードへのコピーと自動paste

自動pasteを有効にする場合は、対象ウィンドウが変わっていないか、録音開始時のフォーカス情報と一致するかを確認できる設計にする。

## 4. システム構成

### 4.1 論理構成

```text
┌──────────────────────────────┐
│ voice-input-desktop           │  GUI、常駐、設定、通知
│ voice-input-cli               │  CLI、診断、単発実行
└──────────────┬───────────────┘
               │ IPCまたは直接呼び出し
               ▼
┌──────────────────────────────┐
│ voice-input-core              │
│  SessionCoordinator            │
│  Recorder                      │
│  Transcriber                   │
│  PostProcessor                 │
│  TextSink                      │
└──────┬───────────────┬───────┘
       │               │
       ▼               ▼
┌──────────────┐  ┌────────────────┐
│ CPAL         │  │ whisper.cpp    │
│ 音声入力      │  │ ローカル推論    │
└──────────────┘  └────────────────┘
       │               │
       └───────┬───────┘
               ▼
       ┌──────────────┐
       │ TextSink群   │
       │ clipboard    │
       │ Herdr        │
       │ Tmux         │
       │ stdout       │
       └──────────────┘
```

### 4.2 主要な処理単位

#### `SessionCoordinator`

1回の録音から出力までを1セッションとして管理する。録音状態、キャンセル、タイムアウト、後処理、出力結果を管理し、各アダプターの詳細を知らない。

#### `Recorder`

マイクから音声を受け取り、Whisperが扱えるモノラルPCMへ変換する。録音中は音声コールバックからロックを長時間保持する処理を呼ばない。

#### `Transcriber`

音声バッファをWhisperへ渡し、文字列と必要なメタデータを返す。モデルのロードは、常駐時には初回起動時または明示的なプリロード時に行う。

#### `PostProcessor`

文字列の前後空白、句読点、改行、辞書による表記統一、禁止文字の扱いを順に処理する。意味の変更を伴うLLM処理は初期実装へ含めない。

#### `TextSink`

処理済みテキストを入力先へ渡す。出力先ごとに、送信結果と失敗理由を返す。

## 5. データモデル

### 5.1 音声データ

Whisperへの入力は、実装上の変換を減らすため次の形式を標準とする。

- サンプル形式: `f32`
- チャンネル数: 1（モノラル）
- サンプルレート: 16,000 Hz
- サンプル値: `[-1.0, 1.0]` に正規化
- メモリ上の所有者: `Vec<f32>` または参照カウントされたバッファ

```rust
pub struct AudioBuffer {
    pub samples: Vec<f32>,
    pub sample_rate_hz: u32,
    pub channels: u16,
    pub started_at: std::time::SystemTime,
    pub duration: std::time::Duration,
}
```

入力デバイスが48 kHz、ステレオなどの場合は、Recorderの境界でリサンプリングとモノラル化を行う。Transcriber側でデバイス固有の変換を行わない。

### 5.2 文字起こし結果

```rust
pub struct Transcript {
    pub text: String,
    pub language: Option<String>,
    pub duration: std::time::Duration,
    pub segments: Vec<TranscriptSegment>,
}

pub struct TranscriptSegment {
    pub start: std::time::Duration,
    pub end: std::time::Duration,
    pub text: String,
}
```

セグメント情報はMVPの表示には必須ではないが、診断、将来の字幕、単語タイムスタンプに備えてモデルに含める。

### 5.3 出力コンテキスト

録音開始時に、可能な範囲でフォーカス対象を保存する。自動pasteやHerdr連携で対象の取り違えを検知するために使用する。

```rust
pub struct OutputContext {
    pub session_id: uuid::Uuid,
    pub started_at: std::time::SystemTime,
    pub focused_app: Option<String>,
    pub focused_window_id: Option<String>,
    pub herdr_pane_id: Option<String>,
    pub tmux_target_pane: Option<String>,
}
```

## 6. 主要インターフェース

### 6.1 `Recorder`

```rust
pub trait Recorder: Send {
    fn list_devices(&self) -> Result<Vec<AudioDevice>, RecorderError>;

    fn start(&mut self, options: RecordingOptions)
        -> Result<(), RecorderError>;

    fn stop(&mut self) -> Result<AudioBuffer, RecorderError>;

    fn cancel(&mut self) -> Result<(), RecorderError>;
}

pub struct RecordingOptions {
    pub device_name: Option<String>,
    pub max_duration: std::time::Duration,
    pub sample_rate_hz: u32,
    pub channels: u16,
}
```

録音コールバックでエラーが発生した場合は、コールバック内でセッション全体を直接終了させず、エラーをイベントキューへ送る。コーディネーターが状態を一元管理する。

### 6.2 `Transcriber`

```rust
pub trait Transcriber: Send + Sync {
    fn transcribe(
        &self,
        audio: &AudioBuffer,
        options: &TranscriptionOptions,
        cancel: &CancellationToken,
    ) -> Result<Transcript, TranscriptionError>;
}

pub struct TranscriptionOptions {
    pub language: LanguageMode,
    pub initial_prompt: Option<String>,
    pub temperature: f32,
    pub translate_to_english: bool,
}

pub enum LanguageMode {
    Auto,
    Japanese,
    English,
}
```

`WhisperCppTranscriber` は、Rustから `whisper.cpp` のC APIを呼ぶアダプターとする。Rustバインディングを採用する場合も、この型の内部に閉じ込める。

モデルコンテキストはセッションごとに作成せず、常駐プロセス内で共有する。ただし、推論APIがスレッドセーフでない場合は、推論キューを1本に限定する。

### 6.3 `PostProcessor`

```rust
pub trait PostProcessor: Send + Sync {
    fn process(
        &self,
        transcript: Transcript,
        context: &ProcessingContext,
    ) -> Result<ProcessedText, PostProcessError>;
}

pub struct ProcessingContext {
    pub dictionary: Vec<DictionaryEntry>,
    pub preserve_newlines: bool,
    pub append_space: bool,
    pub output_format: OutputFormat,
}

pub struct ProcessedText {
    pub text: String,
    pub source: Transcript,
}
```

MVPの処理順は次のとおりである。

1. UTF-8文字列として受け取る。
2. 先頭と末尾の空白を除去する。
3. Whisperが返した不要な繰り返し空白を整理する。
4. 設定された辞書を、完全一致または安全な単語境界で適用する。
5. 出力先の設定に応じて末尾の改行または空白を追加する。
6. 空文字列なら出力せず、`EmptyTranscript` として扱う。

辞書置換は、意図しない部分一致を避けるため、デフォルトでは大文字・小文字の扱いと単語境界を明示する。日本語の単語境界は英語と異なるため、短い置換語には明示的な完全一致モードを用意する。

### 6.4 `TextSink`

```rust
pub trait TextSink: Send + Sync {
    fn send(
        &self,
        text: &ProcessedText,
        context: &OutputContext,
    ) -> Result<SendReceipt, SinkError>;
}

pub struct SendReceipt {
    pub sink_name: String,
    pub bytes_sent: usize,
    pub pasted: bool,
}
```

同じテキストを複数の出力先へ送る場合は、`FanoutSink` を別の合成実装として提供する。各Sinkは相互の実装詳細を知らない。

## 7. 実装コンポーネント

### 7.1 Recorder: CPAL

録音はRustの `cpal` を基本実装とする。CPALが提供する各OSの音声バックエンドを利用し、アプリケーション固有のコードでは次だけを担当する。

- 入力デバイスの列挙と選択
- デバイスのサンプル形式の確認
- サンプルの `f32` への変換
- モノラル化
- 16 kHzへのリサンプリング
- 録音バッファへの非同期追加
- 最大録音時間の監視

録音コールバックからメインスレッドへは、チャネルまたはロックフリーキューでサンプルを渡す。コールバックでファイルI/O、ログ出力、モデル処理を実行しない。

入力デバイスを指定しない場合は、OSの既定入力デバイスを使用する。指定したデバイスが存在しない場合は、勝手に別デバイスへ切り替えず、候補一覧を含むエラーを返す。

### 7.2 Transcriber: whisper.cpp

`whisper.cpp` はローカル推論の実装として採用する。モデルファイルはアプリケーション本体へ埋め込まず、設定されたパスからロードする。

#### 推奨初期モデル

- 第一候補: `large-v3-turbo`
- 低スペック端末向け: `small` または `medium`
- 初回起動時にモデルを自動ダウンロードしない

モデルサイズは精度、メモリ、推論時間に大きく影響するため、設定の診断コマンドでロード可否と推定メモリを確認できるようにする。

#### 初期プロンプト

技術用語の認識を安定させるため、初期プロンプトを設定可能にする。

```text
Codex, Claude Code, Herdr, GitHub, Rust, TypeScript,
React, Hono, Vite, TanStack Query, Terraform,
Terragrunt, mise, Tmux, SSH, API, CLI
```

初期プロンプトは認識結果を強制する辞書ではない。長すぎるプロンプトはコンテキストを消費するため、利用者が編集でき、デフォルト値は短く保つ。

#### 推論実行

- 録音停止後にワーカースレッドへ投入する。
- GUIスレッドまたはホットキーイベント処理をブロックしない。
- キャンセル要求をポーリングし、長い音声の停止要求に応答する。
- モデルロード中と推論中を状態として通知する。
- 推論時間、音声長、実時間係数を診断ログへ記録する。

### 7.3 PostProcessor

MVPでは決定的なテキスト処理だけを実装する。処理結果が同じ入力から常に同じになるため、テストと再現が容易になる。

#### 推奨処理

- 前後の空白除去
- 連続する空白の整理
- 末尾の改行または空白の設定
- 技術用語辞書
- よくある誤認識の明示的な置換
- クリップボード向けの制御文字除去
- `stdout` 向けのJSON出力

LLMや外部APIによる自然文の修正は、認識結果を変更するため、別の任意PostProcessorとして後から追加する。

### 7.4 ClipboardSink

クリップボード操作には、Rustから利用できるクロスプラットフォームのクリップボードライブラリを使用する。実装候補は `arboard` とするが、依存関係の採用時点で対象OSとWaylandの動作を確認する。

動作モードは次の3種類とする。

```text
copy-only
  文字列をクリップボードへコピーする。

copy-and-paste
  文字列をコピーした後、対象アプリへpaste操作を送る。

copy-and-paste-restore
  元のクリップボードを保存し、paste後に復元を試みる。
```

`copy-and-paste-restore` は他アプリがクリップボードを変更する競合があるため、MVPではオプション扱いとする。復元に失敗しても、ペースト済みのテキスト自体は取り消さない。

### 7.5 自動paste

自動pasteはOS依存処理である。キーボード入力ライブラリの利用、またはOSごとのネイティブAPIを `KeyboardInjector` に閉じ込める。

```rust
pub trait KeyboardInjector: Send + Sync {
    fn paste(&self, context: &OutputContext) -> Result<(), PasteError>;
}
```

#### 防止策

- デフォルトは無効にする。
- 録音中に対象ウィンドウが変わった場合の扱いを設定する。
- paste前に短い待機時間を設定できるようにする。
- `--dry-run` でコピーだけを行えるようにする。
- 失敗時はテキストをクリップボードへ残す。
- 自動paste実行の成否をログへ記録する。

### 7.6 HerdrSink

Herdr固有の通信方式をコアに埋め込まず、最初の実装は「Herdrへ渡すアダプター」として分離する。実装方式は次の優先順で選ぶ。

1. Herdrが提供する公式IPCまたはCLI
2. ローカルソケットまたは名前付きパイプ
3. Herdrの現在フォーカス中の入力欄へのClipboard + paste
4. 設定可能な外部コマンドの標準入力

MVPでは、Herdrの実行ファイルやIPC仕様に依存しない `CommandHerdrSink` を実装してもよい。

```toml
[sinks.herdr]
kind = "command"
program = "herdr"
args = ["input", "--pane", "{pane_id}"]
stdin = true
```

プレースホルダーの展開対象は、許可された値だけに限定する。テキスト自体は引数へ埋め込まず、標準入力で渡す。外部コマンドの終了コード、標準エラー、タイムアウトを確認する。

将来、Herdrが明確なIPCを持つ場合は `HerdrIpcSink` を追加し、設定だけで切り替えられるようにする。

### 7.7 TmuxSink

Tmux連携は、初期実装ではTmuxのCLIを呼び出す方式が扱いやすい。Tmuxのセッション内外から利用でき、Rust側にTmuxプロトコルを実装する必要がない。

推奨フローは次のとおりである。

```text
処理済みテキスト
   │
   ├─ 一時バッファへ書き込む
   ├─ tmux load-buffer
   └─ tmux paste-buffer -t <target-pane>
```

実行例は概念的に次のようになる。

```text
tmux load-buffer -b voice-input /path/to/temp-buffer
tmux paste-buffer -b voice-input -t :.
```

実装では、一時ファイルの権限、作成場所、削除タイミングを管理する。テキストをコマンドライン引数へ直接渡さない。Tmuxの存在確認、対象ペインの妥当性確認、コマンドタイムアウトを行う。

別方式として、`tmux set-buffer` へ標準入力を渡す構成を採用してもよい。シェルの解釈を避けるため、`Command` APIへ引数を個別に渡し、シェル経由で実行しない。

### 7.8 StdoutSink

CLIとパイプライン連携のため、標準出力を正式なSinkとして扱う。

```text
voice-input transcribe --input recording.wav --sink stdout
voice-input listen --sink stdout --format json
```

出力形式は少なくとも次を提供する。

- `plain`: 文字列だけを出力する
- `json`: セッションID、文字列、言語、処理時間を出力する
- `jsonl`: 常駐プロセスが1行1イベントで出力する

ログは標準エラーへ出力し、標準出力へ混入させない。

## 8. ホットキーとセッション状態

### 8.1 ホットキー

グローバルホットキーには、クロスプラットフォームの `global-hotkey` 系ライブラリを使用する。OS固有の制約や登録失敗は、`HotkeySource` のアダプターに閉じ込める。

初期操作は次のいずれかとする。

```text
push-to-talk
  ホットキーを押している間だけ録音する。

toggle
  1回目の押下で録音開始、2回目の押下で録音停止する。
```

MVPでは `toggle` をCLIで確実に動かし、GUIでは `push-to-talk` を追加する。キーリピートにより開始・停止が複数回発生しないように、押下イベントをデバウンスする。

### 8.2 状態遷移

```text
Idle
  │ start
  ▼
Recording
  │ stop                 cancel / recorder error
  ▼                      ▼
Transcribing          Failed
  │ success              │ reset
  ▼                      ▼
PostProcessing        Idle
  │ success
  ▼
Sending
  │ success / error
  ▼
Completed ─────────────► Idle
```

同時に2セッションを走らせない。新しい開始要求が来た場合、既存セッションが `Recording` なら拒否し、`Transcribing` なら設定に応じて無視またはキャンセルする。

## 9. CLIとGUIの分離

### 9.1 CLI

CLIは、診断、自動化、CI、SSH、Tmux、GUIなしの利用を担当する。

想定サブコマンドは次のとおりである。

```text
voice-input doctor
voice-input devices list
voice-input record --output /tmp/voice.wav
voice-input transcribe --input /tmp/voice.wav
voice-input listen --sink stdout --format jsonl
voice-input daemon start
voice-input daemon stop
voice-input config print
```

`record` は録音だけ、`transcribe` は既存音声の文字起こしだけ、`listen` はホットキーを含む常駐入力を担当する。責務を分けることで、録音とモデルの問題を個別に再現できる。

### 9.2 GUI

GUIは次を担当する。

- 常駐状態の表示
- 録音中、推論中、出力完了の通知
- 入力デバイスの選択
- モデルとモデルパスの設定
- ホットキーの設定
- 出力先の切り替え
- 権限と診断結果の表示

GUIは `voice-input-core` の状態イベントを購読する。GUIから録音バッファやWhisperコンテキストへ直接アクセスしない。

GUIフレームワークは初期段階では必須としない。デスクトップ配布を始める段階で、TauriなどのWebViewベースGUIを採用するか、RustネイティブGUIを採用するかを決める。どちらを選んでも、GUIがコアに依存する方向は変えない。

### 9.3 CLIとGUIの通信

最初のMVPでは、CLIとGUIが同じコアを直接呼び出してもよい。常駐デーモンを分離する段階では、次の構成へ移行する。

```text
GUI / CLI
   │ JSON-RPCまたはローカルIPC
   ▼
voice-input-daemon
   ├─ HotkeySource
   ├─ SessionCoordinator
   └─ Audio/Whisper/TextSink
```

IPCはローカルユーザーだけが接続できる権限で作成する。認証なしでネットワークへ公開しない。

## 10. crate構成

```text
voice-input/
├─ Cargo.toml
├─ crates/
│  ├─ voice-input-core/
│  │  ├─ src/
│  │  │  ├─ audio.rs
│  │  │  ├─ session.rs
│  │  │  ├─ transcribe.rs
│  │  │  ├─ post_process.rs
│  │  │  ├─ sink.rs
│  │  │  ├─ error.rs
│  │  │  └─ lib.rs
│  │  └─ tests/
│  ├─ voice-input-recorder-cpal/
│  │  └─ src/lib.rs
│  ├─ voice-input-transcriber-whisper-cpp/
│  │  └─ src/lib.rs
│  ├─ voice-input-sinks/
│  │  ├─ src/clipboard.rs
│  │  ├─ src/herdr.rs
│  │  ├─ src/tmux.rs
│  │  ├─ src/stdout.rs
│  │  └─ src/lib.rs
│  ├─ voice-input-hotkey/
│  │  └─ src/lib.rs
│  ├─ voice-input-config/
│  │  └─ src/lib.rs
│  ├─ voice-input-cli/
│  │  └─ src/main.rs
│  └─ voice-input-desktop/
│     └─ src/main.rs
├─ config/
│  └─ voice-input.example.toml
├─ models/
│  └─ .gitkeep
└─ docs/
   └─ architecture.md
```

### 10.1 依存関係の境界

```text
voice-input-core
  ├─ 外部のOS APIへ直接依存しない
  ├─ CPALへ直接依存しない
  ├─ whisper.cppへ直接依存しない
  └─ concrete Sinkへ直接依存しない

adapter crates
  └─ coreのtraitを実装する

CLI / GUI
  └─ adapterを組み合わせてアプリケーションを構成する
```

モデルバインディングがビルドを重くする場合は、`voice-input-transcriber-whisper-cpp` をfeatureで切り替える。CPU版を基本にし、GPUバックエンドは各OSの配布要件が固まってから追加する。

## 11. OSごとの差分

### 11.1 共通部分

次の処理は、できるだけ共通コードで実装する。

- セッション状態
- PCMバッファ
- リサンプリング後の音声形式
- Whisper呼び出し
- PostProcessor
- `stdout` 出力
- 設定の読み込み
- エラー型とログイベント

### 11.2 macOS

- マイク権限を要求し、拒否時はシステム設定への導線を示す。
- グローバルホットキー登録の失敗を利用者に表示する。
- 自動pasteやフォーカス取得に必要なアクセシビリティ権限を案内する。
- クリップボードは通常動作を優先し、復元機能は競合を考慮して任意にする。
- Apple Silicon向けの高速化は、MVPのCPU動作確認後にMetalなどを追加する。

### 11.3 Windows

- マイク権限と、デスクトップアプリに対するプライバシー設定を確認する。
- グローバルホットキーの競合時は、登録済みキーと候補を表示する。
- 自動pasteはWindowsのキーボード入力方式をアダプターとして実装する。
- Tmuxは標準インストールではないため、WSL内のTmuxを利用する場合の接続方式を別途設定する。
- モデルファイルのパス、DLL、ランタイム依存を診断コマンドで検出する。

### 11.4 Linux X11

- X11ではグローバルホットキーとキー入力注入をMVPの対象にできる。
- X11固有の自動pasteは、権限とウィンドウの種類によって挙動が異なるため、失敗時はクリップボードを残す。
- PulseAudio、ALSA、PipeWireなど、CPALが選択したバックエンドを診断に表示する。

### 11.5 Linux Wayland

Waylandでは、アプリケーションが任意の他アプリへキーボード入力を注入することを一般に許可しない。そのため、macOSやX11と同じ自動pasteを保証しない。

MVPでの対応方針は次のとおりである。

- クリップボード出力を標準機能にする。
- `wl-copy` などのWayland対応手段を、利用可能な場合に選択する。
- グローバルホットキーは、デスクトップ環境のショートカットからCLIを起動する方式を優先する。
- KDE、GNOME、その他のコンポジターごとのポータルや拡張機能は、個別アダプターとして扱う。
- 自動paste非対応時は、GUI通知または標準エラーで理由を示す。

Waylandで「1つのバイナリだけで全デスクトップへ自動入力する」ことを初期要件にしない。クリップボード、stdout、Tmux、HerdrのIPCは、自動pasteが利用できない環境でも動作するようにする。

### 11.6 OS差分の実装境界

```text
voice-input-platform
├─ macos.rs
│  ├─ permission check
│  ├─ focused application
│  └─ keyboard injection
├─ windows.rs
│  ├─ permission check
│  ├─ focused window
│  └─ keyboard injection
└─ linux.rs
   ├─ X11 / Wayland判定
   ├─ clipboard backend
   └─ compositor integration
```

OS判定を文字列比較で全体へ散らさず、この境界の中で処理する。

## 12. 設定

設定ファイルはTOMLを想定する。コマンドライン引数は一時的な上書きに使い、永続設定はファイルへ保存する。

```toml
[recording]
device = "default"
sample_rate_hz = 16000
channels = 1
max_duration_seconds = 120
mode = "toggle"

[transcription]
backend = "whisper_cpp"
model_path = "~/Models/ggml-large-v3-turbo.bin"
language = "auto"
temperature = 0.0
translate_to_english = false
initial_prompt = "Codex, Claude Code, Herdr, Rust, TypeScript, React, Hono, Vite, TanStack Query, Terraform, Tmux, SSH, API, CLI"

[post_process]
trim = true
normalize_spaces = true
preserve_newlines = true
append_newline = false
dictionary_path = "~/.config/voice-input/dictionary.toml"

[hotkey]
start_stop = "CommandOrControl+Shift+Space"

[sink]
kind = "clipboard"
paste = false
restore_clipboard = false

[sinks.stdout]
format = "plain"

[sinks.herdr]
kind = "command"
program = "herdr"
args = ["input", "--pane", "{pane_id}"]
timeout_ms = 3000

[sinks.tmux]
target_pane = ":."
buffer_name = "voice-input"
timeout_ms = 3000

[logging]
level = "info"
file = ""
```

### 12.1 設定の優先順位

```text
CLI引数
  > 環境変数
  > ユーザー設定ファイル
  > プロジェクト設定ファイル
  > デフォルト値
```

モデルパスや外部コマンドのパスは、設定読み込み時に展開と存在確認を行う。未使用の設定を黙って無視せず、警告として表示する。

### 12.2 技術用語辞書

```toml
[[entry]]
spoken = "たんすたっくくえり"
replacement = "TanStack Query"
mode = "exact"

[[entry]]
spoken = "インバリデートクエリーズ"
replacement = "invalidateQueries"
mode = "exact"
```

辞書は音声認識後に適用する。Whisperの初期プロンプトと辞書は役割が異なるため、両方を設定できるようにする。

## 13. エラー処理

### 13.1 エラー分類

```rust
pub enum AppError {
    Config(ConfigError),
    Permission(PermissionError),
    Hotkey(HotkeyError),
    Recorder(RecorderError),
    Transcription(TranscriptionError),
    PostProcess(PostProcessError),
    Sink(SinkError),
    Cancelled,
    EmptyTranscript,
}
```

利用者向けメッセージには、次の3項目を含める。

1. 何が失敗したか
2. なぜ失敗した可能性があるか
3. 次に何を試せばよいか

### 13.2 失敗時の保証

| 失敗地点 | 期待する動作 |
|---|---|
| マイク開始 | セッションを終了し、デバイス一覧と権限確認方法を表示する |
| 録音中 | 録音を停止し、途中音声を保存しない。設定時のみ診断用ファイルを残す |
| モデルロード | 起動時または録音前に失敗を通知し、録音を開始しない |
| 推論 | クリップボードを変更せず、エラーを表示する |
| 後処理 | 元の文字起こし結果を診断ログにのみ残し、出力しない |
| 自動paste | テキストをクリップボードへ残し、手動pasteを案内する |
| Herdr/Tmux | コマンドの失敗情報を表示し、別Sinkへのフォールバックは設定時だけ行う |
| stdout | 終了コードを非ゼロにし、ログを標準エラーへ出力する |

フォールバックは勝手に実行しない。たとえばHerdrへの送信に失敗したからといって、意図しないアプリケーションへ自動pasteしない。安全なフォールバックとしてクリップボードへ保存する場合も、設定で明示する。

### 13.3 キャンセル

- 録音中: 録音を破棄する。
- 推論中: `CancellationToken` を通知し、APIが応答可能になった時点で終了する。
- 出力中: 外部コマンドをタイムアウトさせる。途中送信を完全に取り消せない場合は、結果を不確定として通知する。

## 14. ログとプライバシー

音声入力は機微情報を含み得るため、ログを最小化する。

- 音声サンプルを通常ログへ出さない。
- 文字起こし本文を通常ログへ出さない。
- デバッグモードでも、本文と音声を明示設定なしで保存しない。
- セッションID、状態、経過時間、エラーコードを構造化ログへ記録する。
- モデルパス、入力デバイス名、出力先だけを必要に応じて記録する。
- 外部コマンドへ渡すテキストは、標準入力を優先する。
- 一時ファイルを使う場合は、ユーザー専用権限で作成し、処理後に削除する。

診断用に録音を保存する場合は、CLIで明示的に `--save-audio` を指定する設計にする。

## 15. テスト戦略

### 15.1 ユニットテスト

外部デバイスやOSを使わず、次をテストする。

- PCMのモノラル化
- サンプルレート変換の境界値
- 空の音声バッファ
- 最大録音時間の判定
- PostProcessorの空白整理
- 辞書の完全一致と部分一致防止
- 日本語と英語の混在
- 改行保持
- JSON出力のエスケープ
- セッション状態の遷移
- 二重開始、二重停止、キャンセル
- エラーからIdleへ戻る処理

### 15.2 契約テスト

各trait実装が満たすべき契約を共通テストとして用意する。

#### `TextSink` 契約

- 空文字列を受け取った場合の動作が定義されている。
- 成功時に送信バイト数が正しい。
- タイムアウトが呼び出し元へ返る。
- テキストをシェル展開しない。
- 送信失敗時に別のSinkへ勝手に送らない。

#### `Transcriber` 契約

- 16 kHzモノラル入力を受け取れる。
- モデルがない場合に明確なエラーを返す。
- キャンセルが可能な場合に応答する。
- 空結果を成功文字列として黙って返さない。

### 15.3 統合テスト

実機またはCI環境で、次を段階的に確認する。

- 仮想音声デバイスからの録音
- 小さなテストモデルによる推論
- クリップボードへのコピー
- Tmuxの一時セッションへの送信
- Herdrのモックコマンドへの標準入力
- stdoutとstderrの分離
- CLIの終了コード

自動pasteは、実際のユーザー環境に依存しやすい。OSごとに手動受け入れテストを用意し、仮想キーボード入力の結果を確認する。

### 15.4 精度評価

認識精度はモデルの変更、初期プロンプト、辞書の変更で変わるため、固定の音声サンプルセットを作る。

サンプルには次を含める。

- 日本語の一般文
- 日本語と英語の混在
- API名、ライブラリ名、CLI名
- 長い文と短い文
- 無音、雑音、複数人の環境
- 句読点を含む口述

評価指標として、文字誤り率、単語誤り率、技術用語の正解率、録音時間に対する推論時間を記録する。MVPでは、最小モデルを決め打ちせず、端末ごとの品質と速度を比較して推奨値を決める。

## 16. MVPスコープ

### 16.1 MVPに含めるもの

- Rust workspace
- `CpalRecorder`
- `WhisperCppTranscriber` のCPU実装
- 日本語・英語・自動言語判定の設定
- 固定のPostProcessorパイプライン
- `StdoutSink`
- `ClipboardSink` のcopy-only
- CLIの `doctor`、`devices list`、`record`、`transcribe`、`listen`
- toggle方式のホットキー、またはCLIの開始・停止入力
- TOML設定
- 構造化ログ
- コアとアダプターのユニットテスト
- macOS、Windows、Linuxでのビルド確認

### 16.2 MVP後に追加するもの

- Clipboard + 自動paste
- `TmuxSink`
- `HerdrSink`
- GUI常駐アプリ
- push-to-talk
- モデルのダウンロード支援
- Metal、CUDA、Vulkanなどの高速化
- Waylandコンポジター連携
- クリップボード復元
- ユーザー辞書のGUI編集
- IPCデーモン

## 17. ロードマップ

### Phase 0: 技術検証

- CPALで各OSのマイクから16 kHzモノラルを取得する。
- `whisper.cpp` で短い日本語音声を文字起こしする。
- `stdout` へ文字列を出力する。
- クリップボード実装を各OSで確認する。
- モデルロード時間と推論時間を測定する。

完了条件は、3OSのうち少なくとも開発環境で録音から文字起こしまでを再現でき、失敗要因が特定できることである。

### Phase 1: コアとCLI

- `voice-input-core` のtraitと状態機械を実装する。
- CPALアダプターを実装する。
- Whisperアダプターを実装する。
- PostProcessorと設定を実装する。
- stdoutとcopy-only Sinkを実装する。
- エラー型、ログ、ユニットテストを整備する。

完了条件は、音声ファイルを使った再現テストと、マイクを使った単発実行ができることである。

### Phase 2: 常駐入力とOS統合

- グローバルホットキーを実装する。
- macOS、Windows、X11の権限と自動pasteを実装する。
- Waylandではクリップボードを優先し、非対応理由を表示する。
- `doctor` コマンドにデバイス、モデル、権限、バックエンドの診断を追加する。

完了条件は、各OSで録音開始から出力完了までを手動テストできることである。

### Phase 3: HerdrとTmux

- `TmuxSink` を実装する。
- モックコマンドを使った `HerdrSink` を実装する。
- Herdrの正式なIPCまたはCLI仕様が利用可能になったら、専用アダプターを追加する。
- 出力先を設定とCLIで選択できるようにする。

完了条件は、HerdrとTmuxの対象ペインを誤らずに選択でき、送信失敗時にクリップボードへ安全に退避できることである。

### Phase 4: GUIと配布

- 常駐GUIを追加する。
- 設定画面、モデル状態、権限案内、通知を実装する。
- macOS、Windows、Linux向けの配布形式を決める。
- モデルの別配布、署名、アップデート、アンインストールを整備する。
- CPU版と高速化版の互換性を確認する。

完了条件は、利用者がターミナルを開かずに初期設定から音声入力まで行えることである。

## 18. 受け入れ基準

- マイク入力を16 kHzモノラルPCMとして取得できる。
- 録音の開始、停止、キャンセルが状態機械に従って動作する。
- `whisper.cpp` のモデルを一度ロードした後、複数セッションで再利用できる。
- 日本語の短い発話を文字起こしできる。
- 日本語と英語が混在する発話を設定に応じて処理できる。
- PostProcessorの処理結果が決定的である。
- stdoutへログを混入させず、plainまたはJSONを出力できる。
- クリップボードへコピーできる。
- モデル不在、マイク権限拒否、入力デバイス不在を明確に通知できる。
- macOS、Windows、Linuxでビルドできる。
- 音声と本文を、明示設定なしにディスクやネットワークへ保存・送信しない。

## 19. 未決事項

実装開始時に、次の項目を実機で決定する。

- Rustから `whisper.cpp` を呼ぶバインディング方式
- CPU版とGPU版の配布方法
- リサンプリングライブラリ
- global hotkeyライブラリの各OS・Wayland対応範囲
- 自動pasteのキーボード注入方式
- Herdrの正式なIPCまたはCLI仕様
- Windows上のTmuxをWSL経由で扱うか
- GUIフレームワーク
- モデルファイルの配布とライセンス表示
- クリップボード復元をMVPへ含めるか

未決事項は、コアのtraitに反映しない。アダプター実装と配布構成に限定して決定する。

## 20. 実装開始時の最初のタスク

1. Cargo workspaceと`voice-input-core`を作成する。
2. `AudioBuffer`、`Transcript`、`ProcessedText`、`OutputContext`を定義する。
3. Recorder、Transcriber、PostProcessor、TextSinkのtraitを定義する。
4. ダミーRecorder、ダミーTranscriber、StdoutSinkで1本のパイプラインを通す。
5. セッション状態とエラー型のテストを書く。
6. CPALの録音アダプターを追加する。
7. Whisperアダプターを追加し、固定音声サンプルで精度と速度を測定する。
8. Copy-onlyのClipboardSinkを追加する。
9. CLIの`doctor`でマイク、モデル、出力先の診断を実装する。

この順番なら、OS依存のホットキーや自動pasteが未実装でも、録音、文字起こし、後処理、標準出力までを先に検証できる。