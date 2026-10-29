use super::*;
use std::time::{Duration, Instant};

// Retry only acquisition, never a transition or an external side effect.
pub(super) fn retry<T>(mut acquire: impl FnMut() -> Result<T>) -> Result<T> {
    retry_until(Instant::now() + Duration::from_secs(2), &mut acquire)
}

pub(super) fn retry_until<T>(
    deadline: Instant,
    acquire: &mut impl FnMut() -> Result<T>,
) -> Result<T> {
    loop {
        match acquire() {
            Err(error) if error.kind == crate::ErrorKind::StateBusy => {
                if Instant::now() >= deadline {
                    return Err(FlowError::new(crate::ErrorKind::StateBusy,
                        "human handoff lock remained busy for the bounded wait; no transition was committed; retry this same command"));
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            result => return result,
        }
    }
}
