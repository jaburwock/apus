//! Small MRU insertion primitive shared by bounded genome caches.
//!
//! Entries are stored newest-first: insertion places the item at the front and evicts the oldest
//! item from the back once capacity is reached. Cache callers own lookup and deduplication policy;
//! this helper only enforces insertion order and the capacity boundary.
//!
//! A zero capacity is a deliberate no-op. The helper does not repair an already over-capacity
//! vector, so changing either boundary would alter the cache policy of its callers.

/// Inserts an item at the front, evicting the oldest item at capacity.
pub(super) fn insert_mru<T>(items: &mut Vec<T>, capacity: usize, item: T) {
    if capacity == 0 {
        return;
    }
    if items.len() >= capacity {
        items.pop();
    }
    items.insert(0, item);
}
