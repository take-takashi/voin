use std::sync::{Arc, Mutex};
use std::time::Duration;

use voice_input_core::{
    AppError, AudioBuffer, AudioDevice, CancellationToken, DeterministicPostProcessor,
    OutputContext, PostProcessor, ProcessedText, ProcessingContext, Recorder, RecorderError,
    RecordingOptions, SendReceipt, SessionCoordinator, SessionState, SinkError, TextSink,
    Transcriber, Transcript, TranscriptionError, TranscriptionOptions,
};

struct DummyRecorder {
    started: bool,
    cancelled: bool,
}

impl Recorder for DummyRecorder {
    fn list_devices(&self) -> Result<Vec<AudioDevice>, RecorderError> {
        Ok(vec![AudioDevice {
            name: "dummy".to_owned(),
            is_default: true,
        }])
    }

    fn start(&mut self, _options: &RecordingOptions) -> Result<(), RecorderError> {
        self.started = true;
        Ok(())
    }

    fn stop(&mut self) -> Result<AudioBuffer, RecorderError> {
        if !self.started {
            return Err(RecorderError::new("not recording"));
        }
        self.started = false;
        Ok(AudioBuffer::new(vec![0.0; 16_000], 16_000, 1))
    }

    fn cancel(&mut self) -> Result<(), RecorderError> {
        self.started = false;
        self.cancelled = true;
        Ok(())
    }
}

struct DummyTranscriber;

impl Transcriber for DummyTranscriber {
    fn transcribe(
        &self,
        audio: &AudioBuffer,
        _options: &TranscriptionOptions,
        cancel: &CancellationToken,
    ) -> Result<Transcript, TranscriptionError> {
        if cancel.is_cancelled() {
            return Err(TranscriptionError::new("cancelled"));
        }

        Ok(Transcript {
            text: format!("  hello   from {} Hz  ", audio.sample_rate_hz),
            language: Some("en".to_owned()),
            duration: audio.duration,
            segments: Vec::new(),
        })
    }
}

#[derive(Clone, Default)]
struct RecordingSink {
    sent: Arc<Mutex<Vec<(String, String)>>>,
}

impl TextSink for RecordingSink {
    fn send(
        &self,
        text: &ProcessedText,
        context: &OutputContext,
    ) -> Result<SendReceipt, SinkError> {
        self.sent
            .lock()
            .expect("sink mutex must not be poisoned")
            .push((context.session_id.clone(), text.text.clone()));

        Ok(SendReceipt {
            sink_name: "test".to_owned(),
            bytes_sent: text.text.len(),
            pasted: false,
        })
    }
}

fn coordinator(sink: RecordingSink) -> SessionCoordinator {
    SessionCoordinator::new(
        Box::new(DummyRecorder {
            started: false,
            cancelled: false,
        }),
        Box::new(DummyTranscriber),
        Box::new(DeterministicPostProcessor),
        Box::new(sink),
    )
}

#[test]
fn runs_the_cross_platform_pipeline_without_platform_dependencies() {
    let sink = RecordingSink::default();
    let sent = sink.sent.clone();
    let mut coordinator = coordinator(sink);

    coordinator
        .start_recording(&RecordingOptions::default())
        .expect("recording must start");
    assert_eq!(coordinator.state(), SessionState::Recording);

    let receipt = coordinator
        .stop_and_send(
            &TranscriptionOptions::default(),
            &ProcessingContext::default(),
            &CancellationToken::new(),
        )
        .expect("pipeline must complete");

    assert_eq!(receipt.sink_name, "test");
    assert_eq!(coordinator.state(), SessionState::Idle);
    assert_eq!(
        sent.lock().unwrap().as_slice(),
        [("session-1".to_owned(), "hello from 16000 Hz".to_owned())]
    );
}

#[test]
fn rejects_a_second_start_until_the_current_session_is_finished() {
    let mut coordinator = coordinator(RecordingSink::default());
    coordinator
        .start_recording(&RecordingOptions::default())
        .expect("recording must start");

    let error = coordinator
        .start_recording(&RecordingOptions::default())
        .expect_err("a second session must be rejected");

    assert!(matches!(
        error,
        AppError::InvalidState {
            expected: SessionState::Idle,
            actual: SessionState::Recording
        }
    ));
}

#[test]
fn cancellation_discards_the_recording_and_returns_to_idle() {
    let mut coordinator = coordinator(RecordingSink::default());
    coordinator
        .start_recording(&RecordingOptions::default())
        .expect("recording must start");

    coordinator
        .cancel_recording()
        .expect("cancellation must succeed");

    assert_eq!(coordinator.state(), SessionState::Idle);
    assert!(coordinator.current_context().is_none());

    coordinator
        .start_recording(&RecordingOptions::default())
        .expect("a cancelled recording must be reusable");
    coordinator
        .stop_and_send(
            &TranscriptionOptions::default(),
            &ProcessingContext::default(),
            &CancellationToken::new(),
        )
        .expect("the session must be reusable after recording cancellation");
    assert_eq!(coordinator.state(), SessionState::Idle);
}

#[test]
fn cancelled_transcription_clears_session_before_reuse() {
    let mut coordinator = coordinator(RecordingSink::default());
    let cancel = CancellationToken::new();

    coordinator
        .start_recording(&RecordingOptions::default())
        .expect("recording must start");
    cancel.cancel();

    let error = coordinator
        .stop_and_send(
            &TranscriptionOptions::default(),
            &ProcessingContext::default(),
            &cancel,
        )
        .expect_err("cancelled transcription must return a cancellation error");

    assert_eq!(error, AppError::Cancelled);
    assert_eq!(coordinator.state(), SessionState::Idle);
    assert!(coordinator.current_context().is_none());

    coordinator
        .start_recording(&RecordingOptions::default())
        .expect("a cancelled transcription must be reusable");
    coordinator
        .stop_and_send(
            &TranscriptionOptions::default(),
            &ProcessingContext::default(),
            &CancellationToken::new(),
        )
        .expect("the session must be reusable after transcription cancellation");
    assert_eq!(coordinator.state(), SessionState::Idle);
}

#[test]
fn successful_output_is_not_cancelled_after_sink_sends() {
    struct CancellingSink {
        cancel: CancellationToken,
    }

    impl TextSink for CancellingSink {
        fn send(
            &self,
            _text: &ProcessedText,
            _context: &OutputContext,
        ) -> Result<SendReceipt, SinkError> {
            self.cancel.cancel();
            Ok(SendReceipt {
                sink_name: "test".to_owned(),
                bytes_sent: 1,
                pasted: false,
            })
        }
    }

    let cancel = CancellationToken::new();
    let mut coordinator = SessionCoordinator::new(
        Box::new(DummyRecorder {
            started: false,
            cancelled: false,
        }),
        Box::new(DummyTranscriber),
        Box::new(DeterministicPostProcessor),
        Box::new(CancellingSink {
            cancel: cancel.clone(),
        }),
    );

    coordinator
        .start_recording(&RecordingOptions::default())
        .expect("recording must start");
    let receipt = coordinator
        .stop_and_send(
            &TranscriptionOptions::default(),
            &ProcessingContext::default(),
            &cancel,
        )
        .expect("a successful send must remain successful");

    assert_eq!(receipt.sink_name, "test");
    assert_eq!(coordinator.state(), SessionState::Idle);
    assert!(coordinator.current_context().is_none());
}

#[test]
fn empty_processed_text_is_not_sent() {
    struct EmptyTranscriber;

    impl Transcriber for EmptyTranscriber {
        fn transcribe(
            &self,
            audio: &AudioBuffer,
            _options: &TranscriptionOptions,
            _cancel: &CancellationToken,
        ) -> Result<Transcript, TranscriptionError> {
            Ok(Transcript {
                text: " \n\t ".to_owned(),
                language: None,
                duration: audio.duration,
                segments: Vec::new(),
            })
        }
    }

    let mut coordinator = SessionCoordinator::new(
        Box::new(DummyRecorder {
            started: false,
            cancelled: false,
        }),
        Box::new(EmptyTranscriber),
        Box::new(DeterministicPostProcessor),
        Box::new(RecordingSink::default()),
    );
    coordinator
        .start_recording(&RecordingOptions::default())
        .expect("recording must start");

    let error = coordinator
        .stop_and_send(
            &TranscriptionOptions::default(),
            &ProcessingContext::default(),
            &CancellationToken::new(),
        )
        .expect_err("empty text must fail");

    assert_eq!(error, AppError::EmptyTranscript);
    assert_eq!(coordinator.state(), SessionState::Failed);
    coordinator
        .reset()
        .expect("failed session must be resettable");
    assert_eq!(coordinator.state(), SessionState::Idle);
}

#[test]
fn post_processor_applies_exact_dictionary_entry() {
    let processor = DeterministicPostProcessor;
    let result = processor
        .process(
            Transcript {
                text: "  たんすたっくくえり  ".to_owned(),
                language: Some("ja".to_owned()),
                duration: Duration::from_secs(1),
                segments: Vec::new(),
            },
            &ProcessingContext {
                dictionary: vec![voice_input_core::DictionaryEntry {
                    spoken: "たんすたっくくえり".to_owned(),
                    replacement: "TanStack Query".to_owned(),
                    mode: voice_input_core::DictionaryMode::Exact,
                }],
                ..ProcessingContext::default()
            },
        )
        .expect("post-processing must succeed");

    assert_eq!(result.text, "TanStack Query");
}
