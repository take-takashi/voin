use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

/// 長時間処理のキャンセル要求を共有します。
#[derive(Clone, Debug, Default)]
pub struct CancellationToken {
    cancelled: Arc<AtomicBool>,
}

impl CancellationToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    pub fn reset(&self) {
        self.cancelled.store(false, Ordering::Release);
    }
}
