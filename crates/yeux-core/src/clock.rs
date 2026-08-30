use chrono::{DateTime, Utc};
use std::sync::{Arc, Mutex};

/// All core time reads go through this port so traces and expiry checks are deterministic.
pub trait Clock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

#[derive(Clone, Debug)]
pub struct FixedClock {
    instant: Arc<Mutex<DateTime<Utc>>>,
}

impl FixedClock {
    pub fn new(instant: DateTime<Utc>) -> Self {
        Self {
            instant: Arc::new(Mutex::new(instant)),
        }
    }

    pub fn set(&self, instant: DateTime<Utc>) {
        *self.instant.lock().expect("fixed clock mutex poisoned") = instant;
    }
}

impl Clock for FixedClock {
    fn now(&self) -> DateTime<Utc> {
        *self.instant.lock().expect("fixed clock mutex poisoned")
    }
}
