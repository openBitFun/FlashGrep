//! Admission budget shared by all readers of one segment store. Cached payloads
//! retire with their immutable segment; in-flight reads remain valid across GC.
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Debug)]
pub(crate) struct SourceCacheBudget {
    capacity: usize,
    used: AtomicUsize,
}
impl SourceCacheBudget {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            capacity,
            used: AtomicUsize::new(0),
        }
    }
    pub(crate) fn reserve(&self, bytes: usize) -> bool {
        self.used
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(bytes)
                    .filter(|next| *next <= self.capacity)
            })
            .is_ok()
    }
    pub(crate) fn release(&self, bytes: usize) {
        let before = self.used.fetch_sub(bytes, Ordering::AcqRel);
        debug_assert!(before >= bytes);
    }
    #[cfg(test)]
    pub(crate) fn used(&self) -> usize {
        self.used.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn concurrent_admission_never_exceeds_budget_and_released_space_is_reusable() {
        let budget = SourceCacheBudget::new(1024);
        std::thread::scope(|scope| {
            for _ in 0..16 {
                let budget = &budget;
                scope.spawn(move || while budget.reserve(16) {});
            }
        });
        assert_eq!(budget.used(), 1024);
        assert!(!budget.reserve(1));
        budget.release(512);
        assert!(budget.reserve(512));
        assert!(!budget.reserve(usize::MAX));
    }
}
