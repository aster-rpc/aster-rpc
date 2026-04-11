//! Lock-free SPSC ring buffer with batch drain.
//!
//! Single producer, single consumer. No locks, no CAS on the hot path.
//! Coordination is via cache-padded atomic head/tail with Acquire/Release
//! ordering. Capacity is always a power of two for fast index wrapping.

use std::cell::UnsafeCell;
use std::mem::MaybeUninit;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use crossbeam_utils::CachePadded;

struct Shared<T> {
    slots: Box<[UnsafeCell<MaybeUninit<T>>]>,
    capacity: usize,
    mask: usize,
    head: CachePadded<AtomicU64>,
    tail: CachePadded<AtomicU64>,
}

// Safety: SPSC contract — one thread owns Producer (writes head + slots),
// one thread owns Consumer (writes tail + reads slots). Acquire/Release
// fencing ensures cross-thread visibility.
unsafe impl<T: Send> Send for Shared<T> {}
unsafe impl<T: Send> Sync for Shared<T> {}

/// Writing half of the ring buffer. Not `Clone` — single producer only.
pub struct Producer<T> {
    shared: Arc<Shared<T>>,
    head: u64,
    cached_tail: u64,
}

/// Reading half of the ring buffer. Not `Clone` — single consumer only.
pub struct Consumer<T> {
    shared: Arc<Shared<T>>,
    tail: u64,
    cached_head: u64,
}

// Producer and Consumer are Send but not Sync (single-thread access each).
unsafe impl<T: Send> Send for Producer<T> {}
unsafe impl<T: Send> Send for Consumer<T> {}

/// Create a bounded SPSC ring buffer. Capacity is rounded up to the next
/// power of two. Returns `(producer, consumer)`.
///
/// # Panics
///
/// Panics if `capacity` is 0.
pub fn spsc<T>(capacity: usize) -> (Producer<T>, Consumer<T>) {
    assert!(capacity > 0, "ring buffer capacity must be > 0");
    let capacity = capacity.next_power_of_two();

    let mut slots = Vec::with_capacity(capacity);
    for _ in 0..capacity {
        slots.push(UnsafeCell::new(MaybeUninit::uninit()));
    }

    let shared = Arc::new(Shared {
        slots: slots.into_boxed_slice(),
        capacity,
        mask: capacity - 1,
        head: CachePadded::new(AtomicU64::new(0)),
        tail: CachePadded::new(AtomicU64::new(0)),
    });

    (
        Producer {
            shared: shared.clone(),
            head: 0,
            cached_tail: 0,
        },
        Consumer {
            shared,
            tail: 0,
            cached_head: 0,
        },
    )
}

impl<T> Producer<T> {
    /// Push a value into the ring. Returns `Err(value)` if full (backpressure).
    pub fn try_push(&mut self, value: T) -> Result<(), T> {
        if self.head - self.cached_tail >= self.shared.capacity as u64 {
            self.cached_tail = self.shared.tail.load(Ordering::Acquire);
            if self.head - self.cached_tail >= self.shared.capacity as u64 {
                return Err(value);
            }
        }

        let idx = (self.head as usize) & self.shared.mask;
        unsafe {
            (*self.shared.slots[idx].get()).write(value);
        }

        self.head += 1;
        self.shared.head.store(self.head, Ordering::Release);
        Ok(())
    }

    pub fn capacity(&self) -> usize {
        self.shared.capacity
    }

    pub fn len(&self) -> usize {
        let tail = self.shared.tail.load(Ordering::Acquire);
        (self.head - tail) as usize
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn is_full(&mut self) -> bool {
        if self.head - self.cached_tail >= self.shared.capacity as u64 {
            self.cached_tail = self.shared.tail.load(Ordering::Acquire);
            self.head - self.cached_tail >= self.shared.capacity as u64
        } else {
            false
        }
    }
}

impl<T> Consumer<T> {
    /// Pop a single value. Returns `None` if empty.
    pub fn try_pop(&mut self) -> Option<T> {
        if self.tail == self.cached_head {
            self.cached_head = self.shared.head.load(Ordering::Acquire);
            if self.tail == self.cached_head {
                return None;
            }
        }

        let idx = (self.tail as usize) & self.shared.mask;
        let value = unsafe { (*self.shared.slots[idx].get()).assume_init_read() };

        self.tail += 1;
        self.shared.tail.store(self.tail, Ordering::Release);
        Some(value)
    }

    /// Drain up to `max` items, invoking `f` for each. Returns the count
    /// drained. Loads the head once (single atomic read) to determine how
    /// many items are available, then processes them sequentially.
    ///
    /// The tail is advanced after each item so the producer can reclaim
    /// slots progressively and the ring stays consistent if `f` panics.
    pub fn drain<F>(&mut self, max: usize, mut f: F) -> usize
    where
        F: FnMut(T),
    {
        self.cached_head = self.shared.head.load(Ordering::Acquire);
        let available = (self.cached_head - self.tail) as usize;
        let count = available.min(max);

        for _ in 0..count {
            let idx = (self.tail as usize) & self.shared.mask;
            let value = unsafe { (*self.shared.slots[idx].get()).assume_init_read() };
            self.tail += 1;
            self.shared.tail.store(self.tail, Ordering::Release);
            f(value);
        }

        count
    }

    /// Number of items currently available to read.
    pub fn available(&mut self) -> usize {
        self.cached_head = self.shared.head.load(Ordering::Acquire);
        (self.cached_head - self.tail) as usize
    }

    pub fn capacity(&self) -> usize {
        self.shared.capacity
    }

    pub fn len(&self) -> usize {
        let head = self.shared.head.load(Ordering::Acquire);
        (head - self.tail) as usize
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl<T> Drop for Shared<T> {
    fn drop(&mut self) {
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Relaxed);
        for i in tail..head {
            let idx = (i as usize) & self.mask;
            unsafe {
                (*self.slots[idx].get()).assume_init_drop();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn push_pop_single() {
        let (mut p, mut c) = spsc::<u64>(4);
        assert!(p.try_push(42).is_ok());
        assert_eq!(c.try_pop(), Some(42));
        assert_eq!(c.try_pop(), None);
    }

    #[test]
    fn empty_returns_none() {
        let (_p, mut c) = spsc::<u64>(4);
        assert_eq!(c.try_pop(), None);
    }

    #[test]
    fn full_returns_err() {
        let (mut p, _c) = spsc::<u64>(4);
        for i in 0..4 {
            assert!(p.try_push(i).is_ok());
        }
        assert_eq!(p.try_push(99), Err(99));
    }

    #[test]
    fn full_then_pop_unblocks() {
        let (mut p, mut c) = spsc::<u64>(4);
        for i in 0..4 {
            p.try_push(i).unwrap();
        }
        assert!(p.try_push(99).is_err());
        assert_eq!(c.try_pop(), Some(0));
        assert!(p.try_push(99).is_ok());
    }

    #[test]
    fn capacity_rounds_to_power_of_two() {
        let (p, _c) = spsc::<u64>(3);
        assert_eq!(p.capacity(), 4);
        let (p, _c) = spsc::<u64>(5);
        assert_eq!(p.capacity(), 8);
        let (p, _c) = spsc::<u64>(8);
        assert_eq!(p.capacity(), 8);
        let (p, _c) = spsc::<u64>(1);
        assert_eq!(p.capacity(), 1);
    }

    #[test]
    fn wraparound() {
        let (mut p, mut c) = spsc::<u64>(4);
        for round in 0..10u64 {
            for i in 0..4 {
                p.try_push(round * 4 + i).unwrap();
            }
            assert!(p.try_push(999).is_err());
            for i in 0..4 {
                assert_eq!(c.try_pop(), Some(round * 4 + i));
            }
            assert_eq!(c.try_pop(), None);
        }
    }

    #[test]
    fn drain_all() {
        let (mut p, mut c) = spsc::<u64>(8);
        for i in 0..5 {
            p.try_push(i).unwrap();
        }
        let mut items = Vec::new();
        let count = c.drain(10, |v| items.push(v));
        assert_eq!(count, 5);
        assert_eq!(items, vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn drain_partial() {
        let (mut p, mut c) = spsc::<u64>(8);
        for i in 0..5 {
            p.try_push(i).unwrap();
        }
        let mut items = Vec::new();
        let count = c.drain(3, |v| items.push(v));
        assert_eq!(count, 3);
        assert_eq!(items, vec![0, 1, 2]);
        assert_eq!(c.try_pop(), Some(3));
        assert_eq!(c.try_pop(), Some(4));
        assert_eq!(c.try_pop(), None);
    }

    #[test]
    fn drain_empty() {
        let (_p, mut c) = spsc::<u64>(4);
        let count = c.drain(10, |_| panic!("should not be called"));
        assert_eq!(count, 0);
    }

    #[test]
    fn drain_unblocks_producer() {
        let (mut p, mut c) = spsc::<u64>(4);
        for i in 0..4 {
            p.try_push(i).unwrap();
        }
        assert!(p.try_push(99).is_err());
        c.drain(2, |_| {});
        assert!(p.try_push(10).is_ok());
        assert!(p.try_push(11).is_ok());
        assert!(p.try_push(12).is_err());
    }

    #[test]
    fn available_count() {
        let (mut p, mut c) = spsc::<u64>(8);
        assert_eq!(c.available(), 0);
        p.try_push(1).unwrap();
        p.try_push(2).unwrap();
        assert_eq!(c.available(), 2);
        c.try_pop();
        assert_eq!(c.available(), 1);
    }

    #[test]
    fn len_and_is_empty() {
        let (mut p, c) = spsc::<u64>(4);
        assert!(p.is_empty());
        assert!(c.is_empty());
        p.try_push(1).unwrap();
        assert_eq!(p.len(), 1);
        assert_eq!(c.len(), 1);
        assert!(!p.is_empty());
    }

    #[test]
    fn drop_cleans_up_remaining() {
        static DROP_COUNT: AtomicUsize = AtomicUsize::new(0);

        #[derive(Debug)]
        struct Tracked;
        impl Drop for Tracked {
            fn drop(&mut self) {
                DROP_COUNT.fetch_add(1, Ordering::Relaxed);
            }
        }

        DROP_COUNT.store(0, Ordering::Relaxed);
        {
            let (mut p, _c) = spsc::<Tracked>(4);
            p.try_push(Tracked).unwrap();
            p.try_push(Tracked).unwrap();
            p.try_push(Tracked).unwrap();
        }
        assert_eq!(DROP_COUNT.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn cross_thread_sequential() {
        let (mut p, mut c) = spsc::<u64>(256);
        let n = 1_000_000u64;

        let producer = std::thread::spawn(move || {
            for i in 0..n {
                while p.try_push(i).is_err() {
                    std::hint::spin_loop();
                }
            }
        });

        let consumer = std::thread::spawn(move || {
            let mut next = 0u64;
            while next < n {
                if let Some(v) = c.try_pop() {
                    assert_eq!(v, next);
                    next += 1;
                } else {
                    std::hint::spin_loop();
                }
            }
        });

        producer.join().unwrap();
        consumer.join().unwrap();
    }

    #[test]
    fn cross_thread_batch_drain() {
        let (mut p, mut c) = spsc::<u64>(256);
        let n = 1_000_000u64;

        let producer = std::thread::spawn(move || {
            for i in 0..n {
                while p.try_push(i).is_err() {
                    std::hint::spin_loop();
                }
            }
        });

        let consumer = std::thread::spawn(move || {
            let mut next = 0u64;
            while next < n {
                let drained = c.drain(32, |v| {
                    assert_eq!(v, next);
                    next += 1;
                });
                if drained == 0 {
                    std::hint::spin_loop();
                }
            }
        });

        producer.join().unwrap();
        consumer.join().unwrap();
    }

    #[test]
    #[should_panic(expected = "capacity must be > 0")]
    fn zero_capacity_panics() {
        let _ = spsc::<u64>(0);
    }
}
