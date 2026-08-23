use std::ffi::c_void;
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use voice_input_core::{
    AudioBuffer, CancellationToken, LanguageMode, Transcriber, Transcript, TranscriptSegment,
    TranscriptionError, TranscriptionOptions,
};
use whisper_rs::{
    get_lang_str, FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters,
};

/// whisper.cppのモデルコンテキストを共有するTranscriberです。
pub struct WhisperTranscriber {
    context: Mutex<WhisperContext>,
    threads: usize,
}

unsafe extern "C" fn abort_callback(user_data: *mut c_void) -> bool {
    let cancel = unsafe { &*(user_data.cast::<CancellationToken>()) };
    cancel.is_cancelled()
}

impl WhisperTranscriber {
    pub fn from_model_path(path: impl AsRef<Path>) -> Result<Self, TranscriptionError> {
        Self::from_model_path_with_threads(path, default_thread_count())
    }

    pub fn from_model_path_with_threads(
        path: impl AsRef<Path>,
        threads: usize,
    ) -> Result<Self, TranscriptionError> {
        if threads == 0 {
            return Err(TranscriptionError::new(
                "whisper thread count must be positive",
            ));
        }

        let context = WhisperContext::new_with_params(path, WhisperContextParameters::default())
            .map_err(|error| {
                TranscriptionError::new(format!("failed to load whisper model: {error}"))
            })?;

        Ok(Self {
            context: Mutex::new(context),
            threads,
        })
    }

    fn language_option(language: &LanguageMode) -> &'static str {
        match language {
            LanguageMode::Auto => "auto",
            LanguageMode::Japanese => "ja",
            LanguageMode::English => "en",
        }
    }
}

impl Transcriber for WhisperTranscriber {
    fn transcribe(
        &self,
        audio: &AudioBuffer,
        options: &TranscriptionOptions,
        cancel: &CancellationToken,
    ) -> Result<Transcript, TranscriptionError> {
        if audio.sample_rate_hz != 16_000 || audio.channels != 1 {
            return Err(TranscriptionError::new(
                "whisper input must be 16kHz mono PCM",
            ));
        }
        if audio.samples.is_empty() {
            return Err(TranscriptionError::new("whisper input is empty"));
        }
        if cancel.is_cancelled() {
            return Err(TranscriptionError::new("transcription cancelled"));
        }

        let context = self
            .context
            .lock()
            .map_err(|_| TranscriptionError::new("whisper context mutex is poisoned"))?;
        let mut state = context.create_state().map_err(|error| {
            TranscriptionError::new(format!("failed to create whisper state: {error}"))
        })?;

        let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 5 });
        params.set_n_threads(self.threads.min(i32::MAX as usize) as i32);
        params.set_language(Some(Self::language_option(&options.language)));
        params.set_translate(options.translate_to_english);
        params.set_temperature(options.temperature);
        params.set_no_timestamps(true);
        params.set_print_progress(false);
        params.set_print_realtime(false);
        params.set_print_timestamps(false);

        // whisper-rsのsafe callback経路はReleaseビルドで誤ってabort判定になるため、
        // state.fullの呼び出し中だけ有効なC ABI callbackを使います。
        unsafe {
            params.set_abort_callback(Some(abort_callback));
            params.set_abort_callback_user_data(cancel as *const _ as *mut c_void);
        }

        if let Some(initial_prompt) = options.initial_prompt.as_deref() {
            params.set_initial_prompt(initial_prompt);
        }

        state.full(params, &audio.samples).map_err(|error| {
            if cancel.is_cancelled() {
                TranscriptionError::new("transcription cancelled")
            } else {
                TranscriptionError::new(format!("whisper transcription failed: {error}"))
            }
        })?;

        if cancel.is_cancelled() {
            return Err(TranscriptionError::new("transcription cancelled"));
        }

        let segment_count = state.full_n_segments();
        let mut text = String::new();
        let mut segments = Vec::new();

        for index in 0..segment_count {
            let Some(segment) = state.get_segment(index) else {
                continue;
            };
            let segment_text = segment
                .to_str()
                .map_err(|error| TranscriptionError::new(format!("invalid whisper text: {error}")))?
                .to_owned();

            text.push_str(&segment_text);
            segments.push(TranscriptSegment {
                start: timestamp_to_duration(segment.start_timestamp()),
                end: timestamp_to_duration(segment.end_timestamp()),
                text: segment_text,
            });
        }

        let language = get_lang_str(state.full_lang_id_from_state()).map(str::to_owned);
        Ok(Transcript {
            text,
            language,
            duration: audio.duration,
            segments,
        })
    }
}

fn default_thread_count() -> usize {
    std::thread::available_parallelism()
        .map(|count| count.get().min(8))
        .unwrap_or(1)
}

fn timestamp_to_duration(timestamp_centiseconds: i64) -> Duration {
    Duration::from_millis(timestamp_centiseconds.max(0) as u64 * 10)
}

#[cfg(test)]
mod tests {
    use super::{default_thread_count, timestamp_to_duration, WhisperTranscriber};
    use std::time::Duration;
    use voice_input_core::{CancellationToken, LanguageMode};

    #[test]
    fn default_thread_count_is_positive_and_bounded() {
        let threads = default_thread_count();

        assert!((1..=8).contains(&threads));
    }

    #[test]
    fn converts_whisper_centiseconds_to_duration() {
        assert_eq!(timestamp_to_duration(123), Duration::from_millis(1_230));
        assert_eq!(timestamp_to_duration(-1), Duration::ZERO);
    }

    #[test]
    fn maps_supported_language_modes() {
        assert_eq!(
            WhisperTranscriber::language_option(&LanguageMode::Auto),
            "auto"
        );
        assert_eq!(
            WhisperTranscriber::language_option(&LanguageMode::Japanese),
            "ja"
        );
        assert_eq!(
            WhisperTranscriber::language_option(&LanguageMode::English),
            "en"
        );
    }

    #[test]
    fn abort_callback_tracks_the_shared_token_state() {
        let cancel = CancellationToken::new();
        let user_data = &cancel as *const CancellationToken as *mut std::ffi::c_void;

        assert!(!unsafe { super::abort_callback(user_data) });
        cancel.cancel();
        assert!(unsafe { super::abort_callback(user_data) });
    }
}
