# `batch-queue`

A queue which receives items in batches, intended for use cases where the speed
of the receiver matters more than any individual item or sender's latency.

### Acknowledgments

This crate was inspired by Jon Gjenset's [left-right], and his wonderful
[stream][left-right-stream] on the topic. Mara Bos's excellent [Rust Atomics
and Locks][rust-atomics] was also very helpful in figuring out what guarantees I
needed to be enforcing to keep this safe. I'd encourage you to check both out if
you haven't already.

[left-right]: https://crates.io/crates/left-right
[left-right-stream]: youtube.com/watch?v=eLNAMEoKAAc
[rust-atomics]:https://marabos.nl/atomics/

### Concept

`batch-queue` is backed by two separate `Blocks` of contiguous memory, a send
half and a receive half. Each Block also keeps two atomic watermarks internally.
One for the number of slots which have been "locked" by some sender, and one for
the number of slots which have been fully "written".

```
           ┌─────┐
   ┌───────┤Recv │
   │       └──┬──┘
   │          │
   │          │
   ▼          │
 Block A      │   Block B
┌──────────┐  │  ┌──────────┐
│written   │  │  │written   │
├──────────┤  │  ├──────────┤
│locked    │  │  │locked    │
├──────────┤  │  ├──────────┤
│slots ... │  │  │slots ... │
│          │  │  │          │
│          │  │  │          │
└──────────┘  │  └──────────┘
              │     ▲
              │     │
              ▼     │
           ┌─────┐  │
           │Send ├──┘
           └─────┘
```

**Sending:**

- Atomically load the current send block
- Atomically mark a slot as locked (increase the `locked` watermark)
  - This can fail if the block is already at capacity, in which case
    the thread will go to sleep, and restart from the beginning when woken.
- Write the new value into the buffer's slot at the index we just locked
- Wait until `written` is equal to our locked index (anyone who locked
  before us is done)
- Increment `written`

**Receiving:**

- Check if the current block is empty, and if so swap the blocks
- Atomically acquire `written` from it's block (pairs with the atomic release
  after write)
- Yield all the fully written values

**Swapping:**

When the receiver decides to swap it will:

- Ensure that it's current block has been fully reset to default
- Atomically swap the senders' block pointer with the current block
- Wake all threads waiting on the newly current block, so that they can re-start
  the send process.

**Receiver Dropping:**

- Atomically replace the shared block pointer with `null`, so that new senders
  know that the queue is closed
- Mark the old sender block as dropping (set it's `locked` greater than it's `capacity`)
- Wait for all senders to finish writing (atomic acquire to pair with slot writes)
- Drop all values in both blocks which have not been yielded
- Deallocate both blocks
