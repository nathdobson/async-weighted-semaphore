//! An async weighted semaphore: a synchronization primitive for limiting concurrent usage of a
//! resource or signaling availability of a resource to a consumer.
//!
//! A [`Semaphore`] starts with an initial counter of permits. Calling [release](#method.release) will increase the
//! counter. Calling [acquire](#method.acquire) will attempt to decrease the counter, waiting if the counter
//! would be negative.
//!
//! # Examples
//! A semaphore can limit memory usage of concurrent futures:
//! ```
//! # use async_weighted_semaphore::Semaphore;
//! # use std::{io};
//! # use async_std::fs;
//! struct ChecksumPool(Semaphore);
//! impl ChecksumPool{
//!     async fn checksum(&self, path: &str) -> io::Result<u64> {
//!         let len = fs::metadata(path).await?.len();
//!         // Acquire enough permits to create a buffer
//!         let _guard = self.0.acquire(len as usize).await.unwrap();
//!         // Create a buffer
//!         let contents = fs::read(path).await?;
//!         Ok(contents.into_iter().map(|x| x as u64).sum::<u64>())
//!         // End of scope: buffer is dropped and then _guard is dropped, releasing the permits.
//!     }
//! }
//! ```
//! A semaphore can limit memory usage of a producer-consumer queue:
//! ```
//! # use async_weighted_semaphore::{Semaphore, SemaphoreGuardArc};
//! # use std::sync::Arc;
//! # use futures::executor::block_on;
//! # use std::mem;
//! # use futures::join;
//! use async_channel::{Sender, Receiver, unbounded, SendError};
//! # block_on(async {
//! let (sender, receiver) = unbounded();
//! let sender = async move {
//!     // The total size of strings in queue and being parsed will not exceed 10.
//!     let capacity = 10;
//!     let semaphore = Arc::new(Semaphore::new(capacity));
//!     for i in 0..100 {
//!         let data = format!("{}", i);
//!         // Don't deadlock if data.len() exceeds capacity.
//!         let permits = data.len().max(capacity);
//!         let guard = semaphore.acquire_arc(permits).await.unwrap();
//!         if let Err(SendError(_)) = sender.send((guard, data)).await {
//!             break;
//!         }
//!     }
//! };
//! let receiver = async {
//!     for i in 0..100 {
//!         if let Ok((guard, data)) = receiver.recv().await{
//!             assert_eq!(Ok(i), data.parse());
//!             mem::drop(data);
//!             // Drop guard after data to ensure data being parsed counts against the capacity.
//!             mem::drop(guard);
//!         }
//!     }
//! };
//! join!(receiver, sender);
//! # });
//! ```
//! A semaphore can signal the availability of data for batch processing:
//! ```
//! # use std::collections::VecDeque;
//! # use async_weighted_semaphore::Semaphore;
//! # use std::sync::Arc;
//! # use futures::executor::block_on;
//! # use async_std::sync::Mutex;
//! # use futures::join;
//! # block_on(async {
//! let buffer1 = Arc::new((Semaphore::new(0), Mutex::new(VecDeque::<u8>::new())));
//! let buffer2 = buffer1.clone();
//! let sender = async move {
//!     for i in 0..100 {
//!         buffer1.1.lock().await.extend(b"AAA");
//!         buffer1.0.release(3);
//!     }
//!     // Indicate no more data will arrive.
//!     buffer1.0.poison();
//! };
//! let receiver = async {
//!     for i in 0..100 {
//!         if let Ok(guard) = buffer2.0.acquire(2).await {
//!             guard.forget();
//!         }
//!         let batch = buffer2.1.lock().await.drain(0..2).collect::<Vec<_>>();
//!         assert!(batch == b"" || batch == b"A" || batch == b"AA");
//!         if batch.len() < 2 {
//!             break;
//!         }
//!     }
//! };
//! join!(receiver, sender);
//! # });
//! ```
//! # Priority
//! Acquiring has "first-in-first-out" semantics: calls to `acquire` finish in the same order that
//! they start. If there is a pending call to `acquire`, a new call to `acquire` will always block,
//! even if there are enough permits available for the new call. This policy reduces starvation and
//! tail latency at the cost of utilization.
//! ```
//! # use async_weighted_semaphore::Semaphore;
//! # use futures::executor::block_on;
//! # block_on(async{
//! # use futures::pin_mut;
//! # use futures::poll;
//! let sem = Semaphore::new(1);
//! let a = sem.acquire(2);
//! let b = sem.acquire(1);
//! pin_mut!(a);
//! pin_mut!(b);
//! assert!(poll!(&mut a).is_pending());
//! assert!(poll!(&mut b).is_pending());
//! # });
//! ```
//!
//! # Poisoning
//! If a guard is dropped while panicking, or the number of available permits exceeds [`Semaphore::MAX_AVAILABLE`],
//! the semaphore will be permanently poisoned. All current and future acquires will fail,
//! and release will become a no-op. This is similar in principle to poisoning a [`std::sync::Mutex`].
//! Explicitly poisoning with [`Semaphore::poison`] can also be useful to coordinate termination
//! (e.g. closing a producer-consumer channel).
//!
//! # Performance
//! [`Semaphore`] uses no heap allocations. Most calls are lock-free. The only operation that may
//! wait for a lock is cancellation: if a [`AcquireFuture`] or [`AcquireFutureArc`] is dropped
//! before [`Future::poll`] returns [`Poll::Ready`], the drop may synchronously wait for a lock.

#![doc(html_root_url = "https://docs.rs/async-weighted-semaphore/0.1.0")]

#[cfg(test)]
#[macro_use]
extern crate lazy_static;

pub use crate::errors::{TryAcquireError, PoisonError};
pub use crate::guard::SemaphoreGuard;
pub use crate::guard::SemaphoreGuardArc;
pub use crate::acquire::{AcquireFuture, AcquireFutureArc};
pub use semaphore::Semaphore;
use std::future::Future;
use std::task::Poll;

mod atomic;
mod waker;
mod guard;
mod state;
mod errors;
mod acquire;
mod release;
#[cfg(test)]
mod tests;
mod semaphore;

#[test]
fn test_readme_deps() {
    version_sync::assert_markdown_deps_updated!("README.md");
}

#[test]
fn test_html_root_url() {
    version_sync::assert_html_root_url_updated!("src/lib.rs");
}