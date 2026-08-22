use std::fmt::{Display, Formatter};

macro_rules! define_error {
    ($name:ident) => {
        #[derive(Clone, Debug, PartialEq, Eq)]
        pub struct $name {
            message: String,
        }

        impl $name {
            pub fn new(message: impl Into<String>) -> Self {
                Self {
                    message: message.into(),
                }
            }

            pub fn message(&self) -> &str {
                &self.message
            }
        }

        impl Display for $name {
            fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
                formatter.write_str(&self.message)
            }
        }

        impl std::error::Error for $name {}
    };
}

define_error!(RecorderError);
define_error!(TranscriptionError);
define_error!(PostProcessError);
define_error!(SinkError);

use crate::SessionState;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AppError {
    InvalidState {
        expected: SessionState,
        actual: SessionState,
    },
    Recorder(RecorderError),
    Transcription(TranscriptionError),
    PostProcess(PostProcessError),
    Sink(SinkError),
    Cancelled,
    EmptyTranscript,
}

impl Display for AppError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidState { expected, actual } => {
                write!(
                    formatter,
                    "invalid session state: expected {expected:?}, got {actual:?}"
                )
            }
            Self::Recorder(error) => write!(formatter, "recorder error: {error}"),
            Self::Transcription(error) => write!(formatter, "transcription error: {error}"),
            Self::PostProcess(error) => write!(formatter, "post-process error: {error}"),
            Self::Sink(error) => write!(formatter, "sink error: {error}"),
            Self::Cancelled => formatter.write_str("operation cancelled"),
            Self::EmptyTranscript => formatter.write_str("transcript is empty"),
        }
    }
}

impl std::error::Error for AppError {}

impl From<RecorderError> for AppError {
    fn from(error: RecorderError) -> Self {
        Self::Recorder(error)
    }
}

impl From<TranscriptionError> for AppError {
    fn from(error: TranscriptionError) -> Self {
        Self::Transcription(error)
    }
}

impl From<PostProcessError> for AppError {
    fn from(error: PostProcessError) -> Self {
        Self::PostProcess(error)
    }
}

impl From<SinkError> for AppError {
    fn from(error: SinkError) -> Self {
        Self::Sink(error)
    }
}
