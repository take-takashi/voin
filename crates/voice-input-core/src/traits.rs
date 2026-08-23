use crate::{
    AudioBuffer, AudioDevice, CancellationToken, OutputContext, PostProcessError, ProcessedText,
    ProcessingContext, RecorderError, RecordingOptions, SendReceipt, SinkError, Transcript,
    TranscriptionError, TranscriptionOptions,
};

pub trait Recorder: Send {
    fn list_devices(&self) -> Result<Vec<AudioDevice>, RecorderError>;

    fn start(&mut self, options: &RecordingOptions) -> Result<(), RecorderError>;

    fn stop(&mut self) -> Result<AudioBuffer, RecorderError>;

    fn cancel(&mut self) -> Result<(), RecorderError>;
}

pub trait Transcriber: Send + Sync {
    fn transcribe(
        &self,
        audio: &AudioBuffer,
        options: &TranscriptionOptions,
        cancel: &CancellationToken,
    ) -> Result<Transcript, TranscriptionError>;
}

pub trait PostProcessor: Send + Sync {
    fn process(
        &self,
        transcript: Transcript,
        context: &ProcessingContext,
    ) -> Result<ProcessedText, PostProcessError>;
}

pub trait TextSink: Send + Sync {
    fn send(&self, text: &ProcessedText, context: &OutputContext)
        -> Result<SendReceipt, SinkError>;
}
