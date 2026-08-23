#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionState {
    Idle,
    Recording,
    Transcribing,
    PostProcessing,
    Sending,
    Completed,
    Failed,
}
