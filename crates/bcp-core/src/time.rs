use std::sync::{Arc, Mutex};

pub trait Clock: Send + Sync {
    fn now_unix_ms(&self) -> i64;
}

#[derive(Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_unix_ms(&self) -> i64 {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time should be after unix epoch");
        now.as_millis() as i64
    }
}

#[derive(Debug, Clone)]
pub struct FakeClock {
    now_unix_ms: Arc<Mutex<i64>>,
}

impl FakeClock {
    pub fn new(now_unix_ms: i64) -> Self {
        Self {
            now_unix_ms: Arc::new(Mutex::new(now_unix_ms)),
        }
    }

    pub fn advance_ms(&self, delta_ms: i64) {
        let mut guard = self.now_unix_ms.lock().expect("fake clock mutex poisoned");
        *guard += delta_ms;
    }
}

impl Clock for FakeClock {
    fn now_unix_ms(&self) -> i64 {
        *self.now_unix_ms.lock().expect("fake clock mutex poisoned")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fake_clock_advances_deterministically() {
        // Arrange
        let clock = FakeClock::new(1_000);

        // Act
        clock.advance_ms(250);

        // Assert
        assert_eq!(clock.now_unix_ms(), 1_250);
    }
}
