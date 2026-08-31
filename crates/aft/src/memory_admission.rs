use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

const DISABLED_LIMIT: u64 = u64::MAX;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryAdmissionClass {
    Search,
    Semantic,
    Callgraph,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryAdmissionError {
    pub class: MemoryAdmissionClass,
    pub root: Option<Arc<str>>,
    pub requested_bytes: u64,
    pub charged_bytes: u64,
    pub limit_bytes: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryAdmissionSnapshot {
    pub limit_bytes: Option<u64>,
    pub charged_bytes: u64,
    pub peak_charged_bytes: u64,
    pub available_bytes: Option<u64>,
    pub denied_total: u64,
    pub last_denied: Option<MemoryAdmissionError>,
}

#[derive(Debug)]
pub struct MemoryAdmissionLedger {
    limit_bytes: AtomicU64,
    charged_bytes: AtomicU64,
    peak_charged_bytes: AtomicU64,
    denied_total: AtomicU64,
    last_denied: Mutex<Option<MemoryAdmissionError>>,
}

impl MemoryAdmissionLedger {
    pub fn new(limit_bytes: Option<u64>) -> Self {
        Self {
            limit_bytes: AtomicU64::new(encode_limit(limit_bytes)),
            charged_bytes: AtomicU64::new(0),
            peak_charged_bytes: AtomicU64::new(0),
            denied_total: AtomicU64::new(0),
            last_denied: Mutex::new(None),
        }
    }

    pub fn set_limit(&self, limit_bytes: Option<u64>) {
        self.limit_bytes
            .store(encode_limit(limit_bytes), Ordering::Release);
    }

    pub fn reserve(
        self: &Arc<Self>,
        class: MemoryAdmissionClass,
        requested_bytes: u64,
    ) -> Result<MemoryReservation, MemoryAdmissionError> {
        self.reserve_for_root(class, None, requested_bytes)
    }

    pub fn reserve_for_root(
        self: &Arc<Self>,
        class: MemoryAdmissionClass,
        root: Option<Arc<str>>,
        requested_bytes: u64,
    ) -> Result<MemoryReservation, MemoryAdmissionError> {
        let mut charged = self.charged_bytes.load(Ordering::Acquire);
        loop {
            let limit = decode_limit(self.limit_bytes.load(Ordering::Acquire));
            let Some(next) = charged.checked_add(requested_bytes) else {
                return Err(self.record_denial(class, root, requested_bytes, charged, limit));
            };
            if limit.is_some_and(|limit| next > limit) {
                return Err(self.record_denial(class, root, requested_bytes, charged, limit));
            }
            match self.charged_bytes.compare_exchange_weak(
                charged,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    self.peak_charged_bytes.fetch_max(next, Ordering::AcqRel);
                    return Ok(MemoryReservation {
                        ledger: Arc::clone(self),
                        bytes: requested_bytes,
                    });
                }
                Err(observed) => charged = observed,
            }
        }
    }

    pub fn snapshot(&self) -> MemoryAdmissionSnapshot {
        let limit_bytes = decode_limit(self.limit_bytes.load(Ordering::Acquire));
        let charged_bytes = self.charged_bytes.load(Ordering::Acquire);
        MemoryAdmissionSnapshot {
            limit_bytes,
            charged_bytes,
            peak_charged_bytes: self.peak_charged_bytes.load(Ordering::Acquire),
            available_bytes: limit_bytes.map(|limit| limit.saturating_sub(charged_bytes)),
            denied_total: self.denied_total.load(Ordering::Acquire),
            last_denied: self
                .last_denied
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
        }
    }

    fn record_denial(
        &self,
        class: MemoryAdmissionClass,
        root: Option<Arc<str>>,
        requested_bytes: u64,
        charged_bytes: u64,
        limit_bytes: Option<u64>,
    ) -> MemoryAdmissionError {
        let error = MemoryAdmissionError {
            class,
            root,
            requested_bytes,
            charged_bytes,
            limit_bytes,
        };
        self.denied_total.fetch_add(1, Ordering::AcqRel);
        *self
            .last_denied
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(error.clone());
        error
    }

    fn release(&self, bytes: u64) {
        let previous = self.charged_bytes.fetch_sub(bytes, Ordering::AcqRel);
        debug_assert!(previous >= bytes, "memory admission charge underflow");
    }
}

fn encode_limit(limit_bytes: Option<u64>) -> u64 {
    limit_bytes.unwrap_or(DISABLED_LIMIT)
}

fn decode_limit(limit_bytes: u64) -> Option<u64> {
    (limit_bytes != DISABLED_LIMIT).then_some(limit_bytes)
}

#[derive(Debug)]
pub struct MemoryReservation {
    ledger: Arc<MemoryAdmissionLedger>,
    bytes: u64,
}

impl Drop for MemoryReservation {
    fn drop(&mut self) {
        self.ledger.release(self.bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::{MemoryAdmissionClass, MemoryAdmissionLedger};
    use std::sync::{Arc, Barrier};

    #[test]
    fn reservation_releases_owned_bytes() {
        let ledger = Arc::new(MemoryAdmissionLedger::new(Some(64)));
        let reservation = ledger
            .reserve(MemoryAdmissionClass::Search, 48)
            .expect("reservation should fit");

        assert_eq!(ledger.snapshot().charged_bytes, 48);
        drop(reservation);
        assert_eq!(ledger.snapshot().charged_bytes, 0);
        assert_eq!(ledger.snapshot().peak_charged_bytes, 48);
    }

    #[test]
    fn denial_records_request_without_charging() {
        let ledger = Arc::new(MemoryAdmissionLedger::new(Some(32)));
        let _reservation = ledger
            .reserve(MemoryAdmissionClass::Search, 24)
            .expect("first reservation should fit");

        let error = ledger
            .reserve(MemoryAdmissionClass::Semantic, 16)
            .expect_err("second reservation must exceed the ceiling");
        let snapshot = ledger.snapshot();

        assert_eq!(error.requested_bytes, 16);
        assert_eq!(snapshot.charged_bytes, 24);
        assert_eq!(snapshot.denied_total, 1);
        assert_eq!(snapshot.last_denied, Some(error));
    }

    #[test]
    fn unlimited_mode_preserves_existing_behavior() {
        let ledger = Arc::new(MemoryAdmissionLedger::new(None));
        let _reservation = ledger
            .reserve(MemoryAdmissionClass::Callgraph, u64::MAX)
            .expect("disabled ceiling should admit the request");

        let snapshot = ledger.snapshot();
        assert_eq!(snapshot.limit_bytes, None);
        assert_eq!(snapshot.available_bytes, None);
        assert_eq!(snapshot.charged_bytes, u64::MAX);
    }

    #[test]
    fn arithmetic_overflow_is_denied() {
        let ledger = Arc::new(MemoryAdmissionLedger::new(None));
        let _reservation = ledger
            .reserve(MemoryAdmissionClass::Search, u64::MAX)
            .expect("first reservation should fit in the counter");

        assert!(ledger
            .reserve(MemoryAdmissionClass::Search, 1)
            .is_err());
        assert_eq!(ledger.snapshot().charged_bytes, u64::MAX);
    }

    #[test]
    fn concurrent_reservations_never_oversubscribe() {
        const LIMIT: u64 = 64;
        const WORKERS: usize = 16;
        let ledger = Arc::new(MemoryAdmissionLedger::new(Some(LIMIT)));
        let start = Arc::new(Barrier::new(WORKERS));
        let finish = Arc::new(Barrier::new(WORKERS));
        let mut threads = Vec::new();

        for _ in 0..WORKERS {
            let ledger = Arc::clone(&ledger);
            let start = Arc::clone(&start);
            let finish = Arc::clone(&finish);
            threads.push(std::thread::spawn(move || {
                start.wait();
                let reservation = ledger.reserve(MemoryAdmissionClass::Search, 16).ok();
                finish.wait();
                reservation.is_some()
            }));
        }

        let admitted = threads
            .into_iter()
            .map(|thread| thread.join().expect("worker should not panic"))
            .filter(|admitted| *admitted)
            .count();
        let snapshot = ledger.snapshot();

        assert!(admitted <= (LIMIT / 16) as usize);
        assert_eq!(snapshot.charged_bytes, 0);
        assert!(snapshot.peak_charged_bytes <= LIMIT);
    }

    #[test]
    fn reservation_releases_during_unwind() {
        let ledger = Arc::new(MemoryAdmissionLedger::new(Some(32)));
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _reservation = ledger
                .reserve(MemoryAdmissionClass::Search, 32)
                .expect("reservation should fit");
            panic!("exercise unwind");
        }));

        assert!(result.is_err());
        assert_eq!(ledger.snapshot().charged_bytes, 0);
    }
}
