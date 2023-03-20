//! # batch-queue
//!
//! Multi-Producer Single-Consumer Queue Implementation that uses a batched strategy for tracking
//! sends. Useful in cases where your consumption of items within a queue is extremely "bursty,"
//! but it's okay if you don't receive every item you could right away.
//!
//! I built this for use in my personal graphics libraries, where we want to deal with requests
//! within the render code. It's imperative that receiving is fast, but it's okay if a sender has
//! to wait an extra frame for the request to be processed.
//!
//! ```rust
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//!
//! let (mut rx, tx) = batch_queue::channel(16);
//!
//! let ts: [_; 16] = std::array::from_fn({
//!     move |j| {
//!         let tx = tx.clone();
//!         std::thread::spawn(move || {
//!             for i in 0..16 {
//!                 tx.blocking_send(i + j * 16)
//!                     .expect("Sender Dropped");
//!             }
//!         })
//!     }
//! });
//!
//! # let mut out = Vec::with_capacity(16 * 16);
//!
//! while rx.may_rx() {
//!     for it in rx.recv() {
//!         println!("Got {it}!");
//! #       out.push(it);
//!     }
//! }
//!
//! for t in ts {
//!     t.join().unwrap();
//! }
//!
//! # for i in 0..(16 * 16) {
//! #   assert!(out.contains(&i));
//! # }
//! # Ok(())
//! # }
//! ```

use atomic_wait::*;
use std::{
    cell::UnsafeCell,
    mem::MaybeUninit,
    ptr::NonNull,
    sync::{
        atomic::{AtomicPtr, AtomicU32, Ordering},
        Arc,
    },
};

#[cfg(test)]
mod tests;

#[non_exhaustive]
#[derive(Debug)]
pub struct SendError<T> {
    pub value: T,
}

#[non_exhaustive]
#[derive(Debug)]
pub struct TrySendError<T> {
    pub value: T,
    pub ty: TrySendErrorType,
}

#[non_exhaustive]
#[derive(Debug)]
pub enum TrySendErrorType {
    Full,
    Closed,
}

pub struct Sender<T> {
    shared: Arc<Shared<T>>,
}

pub struct Receiver<T> {
    /// The amount of items in `block` which we locked when swapping
    locked: u32,

    /// The amount of items in `block` which we've "consumed" (gave away)
    consumed: u32,

    /// Current receiving block structure. The is the only pointer directly to it,
    /// so once [`Block::get_written`] returns `locked` the entire block can be dropped and
    /// cleared.
    block: NonNull<Block<T>>,

    /// Sending half shared structure
    shared: Arc<Shared<T>>,
}

pub struct Batch<'rx, T> {
    inner: &'rx mut Receiver<T>,
    written: u32,
}

struct Shared<T> {
    /// Current sending block structure.
    ///
    /// Will be `null` in the case the the inner block has been dropped due to the sender being
    /// dropped.
    block: AtomicPtr<Block<T>>,
}

struct Block<T> {
    locked: AtomicU32,
    written: AtomicU32,
    data: Box<[UnsafeCell<MaybeUninit<T>>]>,
}

/// Create a new channel with a given capapcity.
/// Capacity must be even and less `<= usize::MAX - 2`. To enforce this it will be rounded
/// up, and capped to that value.
pub fn channel<T>(capacity: usize) -> (Receiver<T>, Sender<T>) {
    // Convert the capacity into a capacity for each half.
    let capacity = if capacity % 2 == 0 {
        capacity / 2
    } else {
        capacity / 2 + 1
    }
    .min(u32::MAX as usize - 1) as _;

    let shared = Arc::new(Shared {
        block: AtomicPtr::new(Block::new_alloc(capacity).as_ptr()),
    });

    let tx = Sender {
        shared: shared.clone(),
    };

    let rx = Receiver {
        locked: 0,
        consumed: 0,
        shared,
        block: Block::new_alloc(capacity),
    };

    (rx, tx)
}

/// Sending functions for Tx. T must be send because Rx and Tx may have been sent across between
/// threads at any point before these are called.
impl<T> Sender<T>
where
    T: Send,
{
    pub fn try_send(&self, value: T) -> Result<(), TrySendError<T>> {
        let Some(block) = self.shared.get_block() else {
            return Err(TrySendError { value, ty: TrySendErrorType::Closed });
        };

        block.push(value).map_err(|value| TrySendError {
            value,
            ty: TrySendErrorType::Full,
        })
    }

    pub fn blocking_send(&self, mut value: T) -> Result<(), SendError<T>> {
        loop {
            // Re-pull block every iteration, since it is likely to have changed
            // if we had to block.
            let Some(block) = self.shared.get_block() else {
                return Err(SendError { value });
            };

            match block.push(value) {
                Ok(()) => return Ok(()),
                Err(e) => value = e,
            }

            block.wait_if_full();
        }
    }
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        Self {
            shared: Arc::clone(&self.shared),
        }
    }
}

impl<T> Receiver<T> {
    fn get_block(&self) -> &Block<T> {
        // Safety:
        // - Alignment, Dereferencable, Initalized: Created by Box::new
        // - Aliasing: We never give out mutable references,
        //   all internal mut is through atomics or `UnsafeCell`
        unsafe { self.block.as_ref() }
    }

    /// Mark blocks as deallocating, block until senders complete, deallocate both blocks.
    ///
    /// Panics:
    /// - Shared block has already been deallocated
    ///
    /// Safety:
    /// - Leaves [`Self::block`] dangling, so most operations on self are no longer valid.
    ///   Must not be called if already called once.
    unsafe fn deallocate_blocks(&mut self) {
        let shared = self.shared.take_block().unwrap();
        let owned = self.block;

        {
            // Safety:
            // - Alignment, Dereferencable, Initalized: Created by Box::new
            // - Aliasing: We never give out mutable references,
            //   all internal mut is through atomics or `UnsafeCell`
            // - Null written when taking, so is not dangling if unwrap returned.
            let shared_ref = unsafe { shared.as_ref() };

            let shared_locked = shared_ref.mark_locked();
            shared_ref.wait_written(shared_locked);
            shared_ref.drop_range_in_place(0, shared_locked);
            shared_ref.reset();
        }

        // Safety:
        // - Alignment, Dereferencable, Initalized: Created by Box::into_raw(Box::new(..))
        // - Aliasing: We never give out mutable references,
        //   all internal mut is through atomics or `UnsafeCell`
        // - Null written when taking, so is not dangling if unwrap returned.
        drop(unsafe { Box::from_raw(shared.as_ptr()) });

        {
            let block = self.get_block();
            block.wait_written(self.locked);
            block.drop_range_in_place(self.consumed, self.locked);
            block.reset();
        }

        // Safety:
        // - Alignment, Dereferencable, Initalized: Created by Box::into_raw(Box::new(..))
        // - Aliasing: We never give out mutable references,
        //   all internal mut is through atomics or `UnsafeCell`
        // - Not dangling by caller
        drop(unsafe { Box::from_raw(self.block.as_ptr()) });
    }

    /// Get a new sender for this queue
    pub fn get_tx(&self) -> Sender<T> {
        Sender {
            shared: self.shared.clone(),
        }
    }

    /// This queue instance has senders
    pub fn has_senders(&self) -> bool {
        Arc::strong_count(&self.shared) != 1
    }

    /// This queue has senders or data that could be received
    pub fn may_rx(&self) -> bool {
        self.has_senders()
            || self.get_block().get_written() != 0
            || self
                .shared
                .get_block()
                .map(|it| it.get_written() != 0)
                .unwrap_or(false)
    }

    /// The current block is entirely consumed
    fn current_consumed(&self) -> bool {
        let written = self.get_block().get_written();

        self.consumed == self.locked && self.consumed == written
    }

    /// The current block has not been consumed at all
    fn current_unconsumed(&self) -> bool {
        self.consumed == 0
    }

    /// Finalizes and resets the current block so that it can be set as the sending block
    ///
    /// Panics:
    /// - Current block has not been entirely consumed
    fn finalize_current(&mut self) {
        debug_assert!(
            self.current_consumed(),
            "Cannot finalize current block while it remains unconsumed"
        );

        self.get_block().reset();
        self.consumed = 0;
        self.locked = 0;
    }

    /// Swap the current consumed block for the shared (unconsumed) block.
    ///
    /// Panics:
    /// - Current block has not been entirely consumed
    fn swap_blocks(&mut self) {
        self.finalize_current();

        let new = self
            .shared
            .swap_block(self.block)
            .expect("swap_blocks called after destructor run");

        self.block = new;

        let new_locked = self.get_block().mark_locked();

        self.locked = new_locked;
    }

    /// Indicates whether we are ready for a swap.
    /// Returning true indicates that a call to [`Self::swap_blocks`] will not panic.
    fn should_swap(&self) -> bool {
        self.current_consumed() && self.shared.non_empty()
    }

    pub fn recv(&mut self) -> Batch<'_, T>
    where
        T: Send,
    {
        // Check swap for first receive after draining the queue.
        if self.should_swap() {
            self.swap_blocks();
        }

        let written = self.get_block().get_written();

        Batch {
            inner: self,
            written,
        }
    }
}

unsafe impl<T> Send for Receiver<T> {}

impl<T> Drop for Batch<'_, T> {
    fn drop(&mut self) {
        let Self { inner: rx, .. } = self;

        // Swap here if we are more likely to have a full batch next time.
        if rx.should_swap() {
            rx.swap_blocks();
        } else if rx.current_consumed() {
            rx.finalize_current();
        }
    }
}

impl<T> Iterator for Batch<'_, T> {
    type Item = T;

    fn next(&mut self) -> Option<Self::Item> {
        let Self { inner: rx, written } = self;
        let next = rx.consumed;

        if next >= *written {
            return None;
        }

        rx.consumed += 1;
        let block = rx.get_block();

        // Safety:
        // - next < written by if guard
        // - `get_written` guarantees all writes to next have been published
        // - we increment consumed each time we consume, so this slot has not yet been consumed
        Some(unsafe { block.take(next) })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let rem = self.written - self.inner.consumed;
        (rem as _, Some(rem as _))
    }
}

impl<T> ExactSizeIterator for Batch<'_, T> {}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        // Safety:
        // - Drop will be called at most 1 times
        // - this is the only `deallocate_blocks` is called
        unsafe { self.deallocate_blocks() };
    }
}

impl<T> Shared<T> {
    fn get_block(&self) -> Option<&Block<T>> {
        let ptr = self.block.load(Ordering::Relaxed);

        // Safety:
        // - Alignment, Dereferencable, Initalized: Created by Box::new
        // - Aliasing: We never give out mutable references,
        //   all internal mut is through atomics or `UnsafeCell`
        unsafe { ptr.as_ref() }
    }

    fn swap_block(&self, new: NonNull<Block<T>>) -> Result<NonNull<Block<T>>, NonNull<Block<T>>> {
        loop {
            let Some(current) = NonNull::new(self.block.load(Ordering::Relaxed) as *mut _) else {
                return Err(new);
            };

            if self
                .block
                .compare_exchange(
                    current.as_ptr(),
                    new.as_ptr(),
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                )
                .is_ok()
            {
                return Ok(current);
            }
        }
    }

    fn take_block(&self) -> Option<NonNull<Block<T>>> {
        loop {
            let Some(current) = NonNull::new(self.block.load(Ordering::Relaxed) as *mut _) else {
                return None;
            };

            if self
                .block
                .compare_exchange_weak(
                    current.as_ptr(),
                    std::ptr::null_mut(),
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                )
                .is_ok()
            {
                return Some(current);
            }
        }
    }

    fn non_empty(&self) -> bool {
        let Some(block) = self.get_block() else {
            return false;
        };

        block.written.load(Ordering::Relaxed) != 0
    }
}

impl<T> Block<T> {
    /// Panics:
    /// - `capacity == u32::MAX` Max sie used as internal sentinel value
    /// - `capacity == 0`, no room for data storage
    fn new(capacity: u32) -> Block<T> {
        debug_assert_ne!(
            capacity,
            u32::MAX,
            "u32::MAX used as sentinel value, cannot be used as capacity"
        );
        debug_assert_ne!(capacity, 0, "0 is too small to be Block capacity");

        let mut data = Vec::with_capacity(capacity as _);
        data.resize_with(capacity as _, || UnsafeCell::new(MaybeUninit::uninit()));
        let data = data.into_boxed_slice();

        Self {
            locked: AtomicU32::default(),
            written: AtomicU32::default(),
            data,
        }
    }

    fn new_alloc(capacity: u32) -> NonNull<Block<T>> {
        let ptr = Box::into_raw(Box::new(Block::new(capacity)));

        // Safety:
        // - non-null by Box::into_raw
        unsafe { NonNull::new_unchecked(ptr) }
    }

    /// Safety:
    /// - Must have been created by [`Block::new_alloc`]
    /// - Must have no outstanding references
    unsafe fn delete_alloc(block: NonNull<Block<T>>) {
        // Safety:
        // - Aliasing by caller
        // - Created in new_alloc by caller
        // - Layout by new_alloc
        drop(unsafe { Box::from_raw(block.as_ptr()) });
    }

    fn capacity(&self) -> u32 {
        self.data.len() as _
    }

    /// Panics:
    /// - `idx` is greater than the capacity
    /// - In debug, `idx` less than `written`
    ///
    /// Safety:
    /// - `idx` must be less than `Self::written` (fully init)
    /// - slot must not have any outstanding mutable references
    /// - slot must not have been destroyed by `take`
    unsafe fn get(&self, idx: u32) -> &'_ T {
        debug_assert!(
            self.written.load(Ordering::Relaxed) > idx,
            "Calling get on uninit data"
        );

        let slot_ref = &self.data[idx as usize];

        // Safety:
        // - No mutable references to slot by caller
        let uninit_ref = unsafe { &*slot_ref.get() };

        // Safety:
        // - slot was initalized at least once because `written > usize`
        // - slot not consumed (de-init) by caller
        unsafe { uninit_ref.assume_init_ref() }
    }

    /// Panics:
    /// - `idx` is greater than the capacity
    /// - In debug, `idx` less than `written`
    ///
    /// Safety:
    /// - `idx` must be less than `Self::written` (fully init)
    /// - slot must not have any outstanding references
    /// - slot must not have been destroyed by `take`
    unsafe fn get_mut(&self, idx: u32) -> &'_ mut T {
        debug_assert!(
            self.written.load(Ordering::Relaxed) > idx,
            "Calling get on uninit data"
        );

        let slot_ref = &self.data[idx as usize];

        // Safety:
        // - No references to slot by caller
        let uninit_ref = unsafe { &mut *slot_ref.get() };

        // Safety:
        // - slot was initalized at least once because `written > usize`
        // - slot not consumed (de-init) by caller
        unsafe { uninit_ref.assume_init_mut() }
    }

    /// Panics:
    /// - `idx` is greater than the capacity
    /// - In debug, `idx` less than `written`
    ///
    /// Safety:
    /// - `idx` must be less than `Self::written` (fully init)
    /// - slot must not have any outstanding references
    /// - slot must not have been already destroyed by `take`
    unsafe fn take(&self, idx: u32) -> T {
        debug_assert!(
            self.written.load(Ordering::Relaxed) > idx,
            "Calling get on uninit data"
        );

        let slot_ref = &self.data[idx as usize];

        // Safety:
        // - No references to slot by caller
        let uninit_ref = unsafe { &*slot_ref.get() };

        // Safety:
        // - slot was initalized at least once because `written > usize`
        // - slot not consumed (de-init) by caller
        unsafe { uninit_ref.assume_init_read() }
    }

    /// Safety:
    /// - `idx` less than than `locked`
    /// - `idx` greater than or eq `written`
    /// - `idx` slot has no outstanding mutable references
    unsafe fn write(&self, idx: u32, it: T) {
        let slot_ref = &self.data[idx as usize];

        // Safety:
        // - Unique access to slot by caller
        let uninit_refm = unsafe { &mut *slot_ref.get() };

        uninit_refm.write(it);
    }

    /// Try to push a new value into this block, returning Err if there is no more room.
    fn push(&self, it: T) -> Result<(), T> {
        let capacity = self.capacity();
        let mut locked;

        // Lock a slot for ourselves
        loop {
            locked = self.locked.load(Ordering::Relaxed);

            // Block full
            if locked >= capacity {
                return Err(it);
            }

            // Increment locked to take our slot
            if self
                .locked
                .compare_exchange_weak(locked, locked + 1, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                break;
            }

            std::hint::spin_loop();
        }

        // Write value
        //
        // Safety:
        // - No outstanding external references due to safety rules of `get`, `get_mut`, `take`
        // - No internal references due to locking CAS loop acquiring our current value.
        unsafe { self.write(locked as _, it) };

        // Mark slot as written, assuming that the previous slot is finished writing.
        //
        // Release used here to pair with `get_written` and `mark_locked_and_wait`, effectively
        // publishing call to write.
        while let Err(curr) = self.written.compare_exchange_weak(
            locked,
            locked + 1,
            Ordering::Release,
            Ordering::Relaxed,
        ) {
            debug_assert!(
                curr <= locked,
                "Locked incremented past us while we were waiting to release slot"
            );
            // We could also block on written here, and unconditionally notify below. The case
            // where this loop is expected to run for long is considered to be rare enough that
            // spin looping shouldn't waste many cycles.
            // It may be worth backing this up with some benchmarks / using a huristic to decide
            // here instead.
            std::hint::spin_loop();
        }

        // This is dropping, mark that we have finished our write
        if self.locked.load(Ordering::Relaxed) == u32::MAX {
            wake_one(&self.written);
        }

        // TODO(josiah) we could also allow for receiver blocking here by waking written if
        //   our written slot was 0. The receiver could then block on that value to receive the
        //   next value. This doesn't really fit the intended use case, but could be useful
        //   none-the-less.

        Ok(())
    }

    /// Mark this item as locked, so that it will refuse all new writers. Returns the count of
    /// locked slots (to be used with [`wait_written`] and [`drop_range_in_place`]).
    ///
    /// Panics:
    /// - previously called without subsequent call to [`reset`].
    fn mark_locked(&self) -> u32 {
        let locked = self.locked.swap(u32::MAX, Ordering::Relaxed);

        if locked == self.capacity() {
            // This block was at max capacity, so further senders may be waiting.
            wake_all(&self.locked);
        }

        debug_assert_ne!(
            locked,
            u32::MAX,
            "Cannot mark locked again until reset called"
        );

        locked
    }

    /// Wait for writers to finish up to `locked`. Function returning indicates that all slots in
    /// the range `[0, locked)` no longer have any sender permits.
    fn wait_written(&self, locked: u32) {
        loop {
            // Acquire used here to pair with release in `push`, ensuring that data changes there
            // have been published.
            let written = self.written.load(Ordering::Acquire);

            if written >= locked {
                break;
            }

            wait(&self.written, written);
        }
    }

    /// Wait on this block if it is fully locked (possibly not fully written)
    fn wait_if_full(&self) {
        let capacity = self.capacity();

        let locked = self.locked.load(Ordering::Relaxed);

        if locked != capacity {
            return;
        }

        // Note: When returning from this call, it is not known that self is still valid.
        //   We are sometimes woken in order to deallocate
        wait(&self.locked, capacity);
    }

    /// Returns the currently written range end (exclusive).
    /// Memory ordering guarantees that all data written to this object have been published to this
    /// thread.
    fn get_written(&self) -> u32 {
        // Acquire used here to pair with release in `push`, ensuring that data changes there
        // have been published.
        self.written.load(Ordering::Acquire)
    }

    /// Safety:
    /// - all slots in range `[start, end)` have been init
    /// - no slots in range have been de-init (by this function, or by take)
    /// - there are no outstanding references to any slots in range
    unsafe fn drop_range_in_place(&self, start: u32, end: u32) {
        if start == end {
            return;
        } else if start > end {
            panic!("[{start}, {end}) is not a valid range. Start cannot be later than end");
        }

        #[cfg(debug_assertions)]
        {
            let written = self.written.load(Ordering::Relaxed);
            let locked = self.locked.load(Ordering::Relaxed);

            assert!(
                written <= locked,
                "Written ({written}) has exceeded locked ({locked})"
            );
            assert!(
                start < written && end <= written,
                "Range to drop [{start}, {end}) is not within written range [0, {written}) (in progress: [{written}, {locked}))"
                );
        }

        for slot_ref in &self.data[start as usize..end as usize] {
            // Safety:
            // - No outstanding references to slots in range by caller
            let item_refm = unsafe { &mut *slot_ref.get() };

            // Safety:
            // - Slots were initialized by caller
            // - Slots were never deinitalized by caller
            unsafe { item_refm.assume_init_drop() };
        }
    }

    /// Warning: this function does not drop any undropped items. This could lead to resources
    /// remaining unreleased.
    fn reset(&self) {
        #[cfg(debug_assertions)]
        {
            let written = self.written.load(Ordering::Relaxed);
            let locked = self.locked.load(Ordering::Acquire);

            assert!(
                locked == written || locked == u32::MAX,
                "reset called when there were still outstanding messages at [{locked}, {written})"
            );
        }

        // Ordering: Release used for lock, which is done second, to make sure that no slots are
        // ever marked un-locked while marked unwritten.
        self.written.store(0, Ordering::Relaxed);
        self.locked.store(0, Ordering::Release);
    }
}

#[cfg(debug_assertions)]
impl<T> Drop for Block<T> {
    fn drop(&mut self) {
        assert_eq!(
            self.written.load(Ordering::Acquire),
            0,
            "Must not drop without clearing data, does not internally track consumed data"
        );
    }
}
