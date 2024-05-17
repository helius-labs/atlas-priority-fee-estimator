use std::sync::{Arc, RwLock};

use dashmap::DashMap;
use queues::IsQueue;
use queues::Queue;
use solana_sdk::slot_history::Slot;
use tracing::error;

#[derive(Debug, Clone)]
pub struct SlotCache {
    slot_queue: Arc<RwLock<Queue<u64>>>,
    slot_set: Arc<DashMap<Slot, u8>>,
    slot_cache_length: usize,
}

/// SlotCache tracks slot_cache_length number of slots, when capacity is reached
/// it evicts the oldest slot
impl SlotCache {
    pub fn new(slot_cache_length: usize) -> Self {
        Self {
            slot_queue: Arc::new(RwLock::new(Queue::new())),
            slot_set: Arc::new(DashMap::new()),
            slot_cache_length,
        }
    }

    // this pushes a new slot into the cache,
    // and returns the oldest slot if the cache
    // is at capacity
    pub fn push_pop(&self, slot: Slot) -> Option<Slot> {
        if self.slot_set.contains_key(&slot) {
            return None;
        }
        let mut removed_slot = None;
        match self.slot_queue.write() {
            Ok(mut slot_queue) => {
                if slot_queue.size() >= self.slot_cache_length {
                    match slot_queue.remove() {
                        Ok(oldest_slot) => {
                            self.slot_set.remove(&oldest_slot);
                            removed_slot = Some(oldest_slot);
                        }
                        Err(e) => {
                            error!("error removing slot from slot queue: {}", e);
                        }
                    }
                }
                self.slot_set.insert(slot, 0);
                if let Err(e) = slot_queue.add(slot) {
                    error!("error adding slot to slot queue: {}", e);
                }
            }
            Err(e) => {
                error!("error getting write lock on slot queue: {}", e);
            }
        }
        removed_slot
    }
    pub fn len(&self) -> usize {
        self.slot_set.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*; // Import the SlotCache and necessary components

    #[test]
    fn test_push_pop() {
        // Create a SlotCache with a small length for testing
        let slot_cache = SlotCache::new(100);
        let mut i = 0;
        while i < 100 {
            assert_eq!(slot_cache.push_pop(i), None);
            i += 1;
        }

        // Now push one more and it should return the oldest (first inserted)
        assert_eq!(slot_cache.push_pop(101), Some(0));

        // Ensure duplicates are not added
        assert_eq!(slot_cache.push_pop(3), None); // Already exists, should not insert or pop

        // Ensure pushing repeatedly doesn't make the cache grow
        let mut i = 0;
        let len = slot_cache.len();
        while i < 100 {
            assert_eq!(slot_cache.len(), len);
            i += 1;
        }
    }
}
