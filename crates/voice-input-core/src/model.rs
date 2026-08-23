use std::time::{Duration, SystemTime};

#[derive(Clone, Debug, PartialEq)]
pub struct AudioBuffer {
    pub samples: Vec<f32>,
    pub sample_rate_hz: u32,
    pub channels: u16,
    pub started_at: SystemTime,
    pub duration: Duration,
}

impl AudioBuffer {
    pub fn new(samples: Vec<f32>, sample_rate_hz: u32, channels: u16) -> Self {
        let duration = if sample_rate_hz == 0 || channels == 0 {
            Duration::ZERO
        } else {
            Duration::from_secs_f64(
                samples.len() as f64 / f64::from(sample_rate_hz) / f64::from(channels),
            )
        };

        Self {
            samples,
            sample_rate_hz,
            channels,
            started_at: SystemTime::now(),
            duration,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioDevice {
    pub name: String,
    pub is_default: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Transcript {
    pub text: String,
    pub language: Option<String>,
    pub duration: Duration,
    pub segments: Vec<TranscriptSegment>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TranscriptSegment {
    pub start: Duration,
    pub end: Duration,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputContext {
    pub session_id: String,
    pub started_at: SystemTime,
    pub focused_app: Option<String>,
    pub focused_window_id: Option<String>,
    pub herdr_pane_id: Option<String>,
    pub tmux_target_pane: Option<String>,
}

impl OutputContext {
    pub fn new(session_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            started_at: SystemTime::now(),
            focused_app: None,
            focused_window_id: None,
            herdr_pane_id: None,
            tmux_target_pane: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordingOptions {
    pub device_name: Option<String>,
    pub max_duration: Duration,
    pub sample_rate_hz: u32,
    pub channels: u16,
}

impl Default for RecordingOptions {
    fn default() -> Self {
        Self {
            device_name: None,
            max_duration: Duration::from_secs(120),
            sample_rate_hz: 16_000,
            channels: 1,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct TranscriptionOptions {
    pub language: LanguageMode,
    pub initial_prompt: Option<String>,
    pub temperature: f32,
    pub translate_to_english: bool,
}

impl Default for TranscriptionOptions {
    fn default() -> Self {
        Self {
            language: LanguageMode::Auto,
            initial_prompt: None,
            temperature: 0.0,
            translate_to_english: false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LanguageMode {
    Auto,
    Japanese,
    English,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DictionaryEntry {
    pub spoken: String,
    pub replacement: String,
    pub mode: DictionaryMode,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DictionaryMode {
    Exact,
    WordBoundary,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessingContext {
    pub dictionary: Vec<DictionaryEntry>,
    pub preserve_newlines: bool,
    pub normalize_spaces: bool,
    pub append_newline: bool,
    pub append_space: bool,
    pub output_format: OutputFormat,
}

impl Default for ProcessingContext {
    fn default() -> Self {
        Self {
            dictionary: Vec::new(),
            preserve_newlines: true,
            normalize_spaces: true,
            append_newline: false,
            append_space: false,
            output_format: OutputFormat::Plain,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OutputFormat {
    Plain,
    Json,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ProcessedText {
    pub text: String,
    pub source: Transcript,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SendReceipt {
    pub sink_name: String,
    pub bytes_sent: usize,
    pub pasted: bool,
}
