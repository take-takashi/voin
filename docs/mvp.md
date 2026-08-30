# voin MVP方針

## 結論

voinは、ローカルで音声を文字起こしし、任意の入力先へ渡す音声入力ツールです。

MVPでは、次の処理をクロスプラットフォームの共通パイプラインとして実装します。

```text
録音 → 文字起こし → テキスト処理 → stdout / クリップボード
```

自動paste、Tmux、Herdr、GUIはMVPの対象外です。

## MVPの目的

次の処理が、ローカル環境で再現できることを確認します。

1. マイクから音声を録音する
2. Whisper.cppで日本語または英語を文字起こしする
3. 文字列を決定的に整形する
4. stdoutまたはクリップボードへ出力する
5. 失敗理由を利用者へ通知する

音声と文字起こし本文は、明示的な設定がない限りディスクへ保存しません。
外部APIへも送信しません。

## MVPに含める機能

- Rust workspace
- `voice-input-core`
- `CpalRecorder`
- `WhisperCppTranscriber` のCPU実装
- 日本語、英語、自動判定の設定
- 前後の空白除去と空白整理
- 改行保持の設定
- 技術用語辞書の完全一致置換
- `StdoutSink`
- `ClipboardSink` のcopy-onlyモード
- TOML設定
- `doctor` コマンド
- `devices list` コマンド
- 音声ファイルを使う `transcribe` コマンド
- マイクを使う単発の録音・文字起こし
- `agent`コマンドによるtoggle方式の常駐プロセス
- 構造化ログ
- コアとアダプターのユニットテスト
- macOS、Windows、Linuxでのビルド確認

ホットキーは、コアの状態機械を検証した後に追加します。
最初の操作方式は、実装しやすいtoggle方式を採用します。

## MVPに含めない機能

- 自動paste
- クリップボードの復元
- Tmuxへの送信
- Herdrへの送信
- 常駐GUI
- push-to-talk
- モデルの自動ダウンロード
- GPUバックエンド
- リアルタイム文字起こし
- LLMによる文章変換
- Waylandでの任意アプリへのキー入力注入

## アーキテクチャ方針

共通コアは、OS、録音ライブラリ、Whisper.cpp、GUI、外部コマンドへ直接依存しません。

```text
CLI / GUI / 外部トリガー
    ↓ 共通コマンド
アプリケーション層（将来のvoin-agent）
    ↓ セッション操作
SessionCoordinator（コア層）
    ↓ trait
Recorder → Transcriber → PostProcessor → TextSink
    ↓          ↓              ↓              ↓
CPAL     whisper.cpp       共通処理       stdout / clipboard
```

### コア層

コア層は、OSに依存しない音声入力の処理を担当します。

- 音声入力セッションの状態管理
- 録音、文字起こし、後処理、出力の実行
- キャンセルとエラーの伝播
- `Recorder`、`Transcriber`、`PostProcessor`、`TextSink`のtrait

コア層は、操作手段、IPC、GUI、OS固有のAPIを知りません。

### アプリケーション層

アプリケーション層は、コア層を組み立てて実行します。

常駐プロセスは、次の責務を持ちます。

- コア層の依存オブジェクトを生成する
- `start`、`stop`、`toggle`、`cancel`、`status`を受け付ける
- 常駐中のセッション状態を管理する
- IPC経由で操作インターフェースへ結果を返す

常駐プロセスは、操作手段に依存しません。

### インターフェース層

インターフェース層は、利用者の操作を共通コマンドへ変換します。

初期の候補は、次のとおりです。

- CLI
- 外部ランチャーや自動化ツール
- OS別のグローバルホットキー
- 将来のGUIやメニューバーアプリ

外部トリガーは、グローバルホットキーの入口として利用できます。

初期実装では、常駐プロセスと操作インターフェースの通信にlocalhost TCPを使います。
通信方式は、将来OS別アダプターへ差し替えられる境界に置きます。
通信プロトコルは、OSに依存しない共通コマンドとして設計します。

### 共通コアの責務

`voice-input-core`は、次の責務だけを持ちます。

- 音声、文字起こし結果、処理済みテキストのデータモデル
- 録音から出力までのセッション状態
- キャンセルとエラーの伝播
- `Recorder`、`Transcriber`、`PostProcessor`、`TextSink`のtrait
- OSに依存しないテスト可能な処理

共通コアは、次の実装詳細を知りません。

- CPALのデバイスやOSバックエンド
- Whisper.cppのC APIやバインディング
- クリップボードAPI
- グローバルホットキーAPI
- GUIイベントループ
- HerdrやTmuxのコマンド形式

### アダプターの責務

アダプターは、外部ライブラリやOS固有APIをtraitの境界に閉じ込めます。

| 抽象 | MVPの実装 | 将来の実装 |
|---|---|---|
| `Recorder` | `CpalRecorder` | OS固有録音、仮想デバイス |
| `Transcriber` | `WhisperCppTranscriber` | 別のローカルモデル、外部API |
| `PostProcessor` | 決定的な標準処理 | LLM処理、ユーザー拡張 |
| `TextSink` | `StdoutSink`、`ClipboardSink` | `TmuxSink`、`HerdrSink`、paste |
| `CommandSource` | CLI操作 | 外部ランチャー、自動化ツール、OSごとのグローバルホットキー、GUI |

OSごとに提供できる機能が異なる場合は、共通traitで無理に同じ動作を保証しません。
利用可能な機能を診断結果として示し、SinkやCommandSourceのアダプターで扱います。

`CommandSource`は、利用者の操作を共通コマンドへ変換します。
外部ランチャー固有の実装やOS固有のホットキーAPIは、コア層へ追加しません。

常駐プロセスとIPCは、`voin-cli agent`として実装します。
`start`、`stop`、`toggle`、`cancel`、`status`、`reset`を共通コマンドとして受け付けます。

## データ境界

アダプター間では、次の型だけを受け渡します。

```rust
pub struct AudioBuffer {
    pub samples: Vec<f32>,
    pub sample_rate_hz: u32,
    pub channels: u16,
}

pub struct Transcript {
    pub text: String,
    pub language: Option<String>,
}

pub struct ProcessedText {
    pub text: String,
    pub source: Transcript,
}
```

録音アダプターは、入力デバイスの形式を16kHz、モノラル、`f32`へ変換します。
文字起こしアダプターは、デバイス固有の変換を行いません。

## 最初の実装順

1. Cargo workspaceを作成する
2. 共通データモデルとtraitを定義する
3. ダミー実装でパイプラインを通す
4. セッション状態とエラーのテストを書く
5. CPALで録音する
6. Whisper.cppで固定音声を文字起こしする
7. stdoutとクリップボードへ出力する
8. `doctor` と設定読み込みを追加する
9. macOS、Windows、Linuxでビルドを確認する
10. toggle方式のホットキーを追加する

外部ライブラリの検証で設計変更が発生した場合は、共通コアではなくアダプター側を修正します。

## 完了条件

- 16kHzモノラルPCMを取得できる
- 録音、停止、キャンセルの状態遷移をテストできる
- Whisper.cppのモデルを複数セッションで再利用できる
- 日本語の短い音声を文字起こしできる
- 日本語と英語が混在する音声を処理できる
- 同じ入力に対してPostProcessorが同じ結果を返す
- stdoutへログを混入させずに文字列またはJSONを出力できる
- クリップボードへ文字列をコピーできる
- マイク権限、デバイス、モデルのエラーを説明できる
- macOS、Windows、Linuxでビルドできる
- 音声と本文を、明示設定なしに保存または送信しない

## 実装中に決める事項

次の事項は、共通コアの設計を変えずにアダプター側で決定します。

- whisper.cppのRustバインディング方式
- リサンプリングライブラリ
- クリップボードライブラリ
- グローバルホットキーライブラリ
- 各OSの権限確認方法
- モデルファイルの配布方法
- GUIフレームワーク
