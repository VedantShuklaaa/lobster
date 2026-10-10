//! Lock-free single-producer / single-consumer ring buffer.
//!
//! Exactly one thread owns the `Producer`, exactly one owns the `Consumer`.
//! That restriction is what makes it cheap: there is no compare-and-swap loop,
//! only one atomic store per push and one per pop.
//!
//! Layout
//! - `head`: next slot the consumer will read. Written only by the consumer.
//! - `tail`: next slot the producer will write. Written only by the producer.
//! - Both are ever-increasing counters (they wrap at usize::MAX, which is fine
//!   because the capacity is a power of two). The slot is `counter & mask`.
//!   Full is `tail - head == capacity`, empty is `tail == head`.
//!
//! Memory ordering
//! - Producer writes the slot, then `tail.store(Release)`. The consumer's
//!   `tail.load(Acquire)` that sees the new tail is therefore guaranteed to see
//!   the slot contents too.
//! - Consumer reads the slot, then `head.store(Release)`. The producer's
//!   `head.load(Acquire)` that sees the new head is guaranteed that the read has
//!   finished, so overwriting the slot is safe.
//! - Each side reads its OWN counter from a plain local copy, never from the
//!   atomic.
//!
//! Batching (optional, `channel_with_batch`)
//! Publishing `tail` after every push makes the consumer's cache line for it
//! bounce between cores once per item. With a batch of N the producer publishes
//! `tail` every N pushes, and the consumer publishes `head` every N pops. The
//! price is latency: an item can sit unpublished until N-1 more arrive. To stay
//! deadlock-free each side also publishes whenever it is about to wait (the
//! producer when the ring looks full, the consumer when it looks empty), and the
//! producer must call `publish()` after its last push. Batch 1 is the old
//! publish-every-item behaviour.
//!
//! Two speedups
//! - `head` and `tail` sit on separate cache lines (`CachePadded`), so the two
//!   threads do not invalidate each other's line on every write (false sharing).
//! - Each side keeps a cached copy of the OTHER side's counter and only re-reads
//!   the atomic when the cache says "full" (producer) or "empty" (consumer).
//!   In steady state most pushes and pops touch no shared cache line but the slot.

use std::cell::UnsafeCell;
use std::mem::MaybeUninit;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// 128 bytes: Apple Silicon cache lines are 128 bytes, and recent Intel parts
/// prefetch adjacent 64-byte line pairs, so 128 is safe on both.
#[repr(align(128))]
struct CachePadded<T>(T);

struct Shared<T> {
    buf: Box<[UnsafeCell<MaybeUninit<T>>]>,
    mask: usize,
    head: CachePadded<AtomicUsize>,
    tail: CachePadded<AtomicUsize>,
}

// SAFETY: a slot is only ever touched by one side at a time. The producer
// writes slots in `[head, head + cap)` that the consumer has released, the
// consumer reads slots in `[head, tail)` that the producer has published, and
// the Acquire/Release pairs above order those accesses.
unsafe impl<T: Send> Sync for Shared<T> {}

pub struct Producer<T> {
    shared: Arc<Shared<T>>,
    tail: usize,       // our own counter, authoritative
    head_cache: usize, // last head value we saw
    batch: usize,
    unpublished: usize, // pushes since the shared tail was last stored
}

pub struct Consumer<T> {
    shared: Arc<Shared<T>>,
    head: usize,       // our own counter, authoritative
    tail_cache: usize, // last tail value we saw
    batch: usize,
    unreleased: usize, // pops since the shared head was last stored
}

/// Creates a ring that holds at least `capacity` items (rounded up to a power
/// of two, minimum 2). `T: Copy` keeps the ring free of drop logic: nothing in
/// a slot ever needs dropping.
pub fn channel<T: Copy + Send>(capacity: usize) -> (Producer<T>, Consumer<T>) {
    channel_with_batch(capacity, 1)
}

/// Like `channel`, but each side publishes its counter every `batch` items
/// (see the module docs). `batch` is clamped to at least 1.
pub fn channel_with_batch<T: Copy + Send>(
    capacity: usize,
    batch: usize,
) -> (Producer<T>, Consumer<T>) {
    let batch = batch.max(1);
    let cap = capacity.max(2).next_power_of_two();
    let buf: Box<[UnsafeCell<MaybeUninit<T>>]> = (0..cap)
        .map(|_| UnsafeCell::new(MaybeUninit::uninit()))
        .collect();
    let shared = Arc::new(Shared {
        buf,
        mask: cap - 1,
        head: CachePadded(AtomicUsize::new(0)),
        tail: CachePadded(AtomicUsize::new(0)),
    });
    (
        Producer {
            shared: shared.clone(),
            tail: 0,
            head_cache: 0,
            batch,
            unpublished: 0,
        },
        Consumer {
            shared,
            head: 0,
            tail_cache: 0,
            batch,
            unreleased: 0,
        },
    )
}

impl<T: Copy> Producer<T> {
    pub fn capacity(&self) -> usize {
        self.shared.mask + 1
    }

    /// Returns the value back if the ring is full.
    #[inline]
    pub fn push(&mut self, value: T) -> Result<(), T> {
        let cap = self.shared.mask + 1;
        if self.tail.wrapping_sub(self.head_cache) == cap {
            // Looks full. Publish what we have first: the consumer may be
            // waiting for exactly these items before it frees any space.
            self.publish();
            self.head_cache = self.shared.head.0.load(Ordering::Acquire);
            if self.tail.wrapping_sub(self.head_cache) == cap {
                return Err(value);
            }
        }
        let slot = &self.shared.buf[self.tail & self.shared.mask];
        // SAFETY: slot index `tail` is outside `[head, tail)`, so the consumer
        // is not reading it, and we are the only producer.
        unsafe { (*slot.get()).write(value) };
        self.tail = self.tail.wrapping_add(1);
        self.unpublished += 1;
        if self.unpublished >= self.batch {
            self.publish();
        }
        Ok(())
    }

    /// Makes every pushed item visible to the consumer. Call it after the last
    /// push, and whenever the producer is about to go idle.
    #[inline]
    pub fn publish(&mut self) {
        if self.unpublished > 0 {
            self.shared.tail.0.store(self.tail, Ordering::Release);
            self.unpublished = 0;
        }
    }
}

impl<T: Copy> Consumer<T> {
    /// Returns `None` if the ring is empty.
    #[inline]
    pub fn pop(&mut self) -> Option<T> {
        if self.head == self.tail_cache {
            // Looks empty. Free the slots we have read first: the producer may
            // be waiting for them before it can push more.
            self.release();
            self.tail_cache = self.shared.tail.0.load(Ordering::Acquire);
            if self.head == self.tail_cache {
                return None;
            }
        }
        let slot = &self.shared.buf[self.head & self.shared.mask];
        // SAFETY: `head != tail`, so the producer has published this slot
        // (the Acquire load above pairs with its Release store) and will not
        // touch it again until we publish a newer `head`.
        let value = unsafe { (*slot.get()).assume_init_read() };
        self.head = self.head.wrapping_add(1);
        self.unreleased += 1;
        if self.unreleased >= self.batch {
            self.release();
        }
        Some(value)
    }

    /// Tells the producer which slots have been read and may be reused.
    #[inline]
    fn release(&mut self) {
        if self.unreleased > 0 {
            self.shared.head.0.store(self.head, Ordering::Release);
            self.unreleased = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn capacity_rounds_up_to_power_of_two() {
        let (p, _c) = channel::<u32>(5);
        assert_eq!(p.capacity(), 8);
        let (p, _c) = channel::<u32>(0);
        assert_eq!(p.capacity(), 2);
        let (p, _c) = channel::<u32>(8);
        assert_eq!(p.capacity(), 8);
    }

    #[test]
    fn empty_then_fifo_then_full() {
        let (mut p, mut c) = channel::<u32>(4);
        assert_eq!(c.pop(), None);
        for i in 0..4 {
            assert_eq!(p.push(i), Ok(()));
        }
        assert_eq!(p.push(99), Err(99)); // full: value comes back
        for i in 0..4 {
            assert_eq!(c.pop(), Some(i));
        }
        assert_eq!(c.pop(), None);
    }

    #[test]
    fn wraps_around_many_times() {
        let (mut p, mut c) = channel::<u64>(4);
        for i in 0..1000u64 {
            assert_eq!(p.push(i), Ok(()));
            assert_eq!(c.pop(), Some(i));
        }
        // refill after wrap, mixed depth
        for i in 0..3 {
            p.push(i).unwrap();
        }
        assert_eq!(c.pop(), Some(0));
        p.push(3).unwrap();
        p.push(4).unwrap();
        assert!(p.push(5).is_err());
        for i in 1..5 {
            assert_eq!(c.pop(), Some(i));
        }
    }

    #[test]
    fn two_threads_keep_order_and_lose_nothing() {
        const N: u64 = 2_000_000;
        // small ring so the producer hits "full" and the consumer hits "empty" constantly
        let (mut p, mut c) = channel::<u64>(64);
        let consumer = thread::spawn(move || {
            let mut expect = 0u64;
            while expect < N {
                if let Some(v) = c.pop() {
                    assert_eq!(v, expect, "out of order or lost item");
                    expect += 1;
                } else {
                    std::hint::spin_loop();
                }
            }
            assert_eq!(c.pop(), None);
        });
        for i in 0..N {
            let mut v = i;
            while let Err(back) = p.push(v) {
                v = back;
                std::hint::spin_loop();
            }
        }
        consumer.join().unwrap();
    }

    #[test]
    fn two_threads_with_struct_payload() {
        #[derive(Clone, Copy, PartialEq, Debug)]
        struct Msg {
            a: u64,
            b: u64,
            c: u32,
        }
        const N: u64 = 500_000;
        let (mut p, mut c) = channel::<Msg>(16);
        let consumer = thread::spawn(move || {
            let mut i = 0u64;
            while i < N {
                if let Some(m) = c.pop() {
                    // all fields must belong to the same message: no torn reads
                    assert_eq!(
                        m,
                        Msg {
                            a: i,
                            b: !i,
                            c: i as u32
                        }
                    );
                    i += 1;
                }
            }
        });
        for i in 0..N {
            let mut m = Msg {
                a: i,
                b: !i,
                c: i as u32,
            };
            while let Err(back) = p.push(m) {
                m = back;
            }
        }
        consumer.join().unwrap();
    }

    #[test]
    fn batched_items_become_visible_on_publish() {
        let (mut p, mut c) = channel_with_batch::<u32>(16, 8);
        for i in 0..3 {
            p.push(i).unwrap();
        }
        assert_eq!(c.pop(), None); // 3 < batch: not published yet
        p.publish();
        for i in 0..3 {
            assert_eq!(c.pop(), Some(i));
        }
        assert_eq!(c.pop(), None);
    }

    #[test]
    fn full_ring_publishes_so_a_waiting_consumer_can_proceed() {
        // batch (64) larger than the ring (4): without publish-on-full the
        // producer would report full while the consumer saw nothing.
        let (mut p, mut c) = channel_with_batch::<u32>(4, 64);
        for i in 0..4 {
            p.push(i).unwrap();
        }
        assert!(p.push(4).is_err()); // full, and this call published
        for i in 0..4 {
            assert_eq!(c.pop(), Some(i));
        }
        assert_eq!(c.pop(), None); // empty, and this call released
        for i in 4..8 {
            p.push(i).unwrap(); // the producer sees the freed slots
        }
    }

    #[test]
    fn two_threads_with_batching() {
        const N: u64 = 1_000_000;
        for (cap, batch) in [(64, 1), (64, 8), (64, 64), (2, 64), (1024, 256)] {
            let (mut p, mut c) = channel_with_batch::<u64>(cap, batch);
            let consumer = thread::spawn(move || {
                let mut expect = 0u64;
                while expect < N {
                    if let Some(v) = c.pop() {
                        assert_eq!(v, expect, "cap {cap} batch {batch}");
                        expect += 1;
                    } else {
                        std::hint::spin_loop();
                    }
                }
            });
            for i in 0..N {
                let mut v = i;
                while let Err(back) = p.push(v) {
                    v = back;
                    std::hint::spin_loop();
                }
            }
            p.publish();
            consumer.join().unwrap();
        }
    }
}
