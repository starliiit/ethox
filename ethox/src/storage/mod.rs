/*! Specialized containers.

The `storage` module provides containers for use in other modules.
The containers support both pre-allocated memory, without the `std`
or `alloc` crates being available, and heap-allocated memory.
*/

pub mod assembler;
mod ring_buffer;

pub use self::assembler::Assembler;
pub use self::ring_buffer::RingBuffer;

/// A trait for setting a value to a known state.
///
/// In-place analog of Default.
pub trait Resettable {
    /// Clear or set all elements to a default value.
    fn reset(&mut self);
}
