use crate::{
    AppError, CancellationToken, OutputContext, PostProcessor, ProcessingContext, Recorder,
    RecordingOptions, SendReceipt, SessionState, TextSink, Transcriber, TranscriptionOptions,
};

pub struct SessionCoordinator {
    recorder: Box<dyn Recorder>,
    transcriber: Box<dyn Transcriber>,
    post_processor: Box<dyn PostProcessor>,
    sink: Box<dyn TextSink>,
    state: SessionState,
    current_context: Option<OutputContext>,
    next_session_number: u64,
}

impl SessionCoordinator {
    pub fn new(
        recorder: Box<dyn Recorder>,
        transcriber: Box<dyn Transcriber>,
        post_processor: Box<dyn PostProcessor>,
        sink: Box<dyn TextSink>,
    ) -> Self {
        Self {
            recorder,
            transcriber,
            post_processor,
            sink,
            state: SessionState::Idle,
            current_context: None,
            next_session_number: 1,
        }
    }

    pub fn state(&self) -> SessionState {
        self.state
    }

    pub fn current_context(&self) -> Option<&OutputContext> {
        self.current_context.as_ref()
    }

    pub fn start_recording(&mut self, options: &RecordingOptions) -> Result<(), AppError> {
        self.require_state(SessionState::Idle)?;

        let context = OutputContext::new(format!("session-{}", self.next_session_number));
        self.next_session_number += 1;

        if let Err(error) = self.recorder.start(options) {
            self.state = SessionState::Failed;
            return Err(error.into());
        }

        self.current_context = Some(context);
        self.state = SessionState::Recording;
        Ok(())
    }

    pub fn cancel_recording(&mut self) -> Result<(), AppError> {
        self.require_state(SessionState::Recording)?;

        if let Err(error) = self.recorder.cancel() {
            self.state = SessionState::Failed;
            return Err(error.into());
        }

        self.clear_session();
        Ok(())
    }

    pub fn stop_and_send(
        &mut self,
        transcription_options: &TranscriptionOptions,
        processing_context: &ProcessingContext,
        cancel: &CancellationToken,
    ) -> Result<SendReceipt, AppError> {
        self.require_state(SessionState::Recording)?;

        let audio = match self.recorder.stop() {
            Ok(audio) => audio,
            Err(error) => {
                self.state = SessionState::Failed;
                return Err(error.into());
            }
        };

        self.state = SessionState::Transcribing;
        let transcript = match self
            .transcriber
            .transcribe(&audio, transcription_options, cancel)
        {
            Ok(transcript) => transcript,
            Err(error) => {
                self.state = if cancel.is_cancelled() {
                    SessionState::Idle
                } else {
                    SessionState::Failed
                };
                return if cancel.is_cancelled() {
                    Err(AppError::Cancelled)
                } else {
                    Err(error.into())
                };
            }
        };

        if cancel.is_cancelled() {
            self.clear_session();
            return Err(AppError::Cancelled);
        }

        self.state = SessionState::PostProcessing;
        let processed = match self.post_processor.process(transcript, processing_context) {
            Ok(processed) => processed,
            Err(error) => {
                self.state = SessionState::Failed;
                return Err(error.into());
            }
        };

        if processed.text.is_empty() {
            self.state = SessionState::Failed;
            return Err(AppError::EmptyTranscript);
        }

        if cancel.is_cancelled() {
            self.clear_session();
            return Err(AppError::Cancelled);
        }

        self.state = SessionState::Sending;
        let context = self
            .current_context
            .as_ref()
            .expect("recording session must have output context");
        let receipt = match self.sink.send(&processed, context) {
            Ok(receipt) => receipt,
            Err(error) => {
                self.state = SessionState::Failed;
                return Err(error.into());
            }
        };

        self.state = SessionState::Completed;
        self.clear_session();
        Ok(receipt)
    }

    pub fn reset(&mut self) -> Result<(), AppError> {
        if self.state != SessionState::Failed {
            return Err(AppError::InvalidState {
                expected: SessionState::Failed,
                actual: self.state,
            });
        }

        self.clear_session();
        Ok(())
    }

    fn require_state(&self, expected: SessionState) -> Result<(), AppError> {
        if self.state == expected {
            Ok(())
        } else {
            Err(AppError::InvalidState {
                expected,
                actual: self.state,
            })
        }
    }

    fn clear_session(&mut self) {
        self.current_context = None;
        self.state = SessionState::Idle;
    }
}
