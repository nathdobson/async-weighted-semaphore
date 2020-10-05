use crate::state::{Waiter, AcquireStep, Permits};
use std::cell::UnsafeCell;
use std::marker::{PhantomPinned, PhantomData};
use crate::{Semaphore, SemaphoreGuard, SemaphoreGuardArc};
use std::sync::Arc;
use std::panic::{UnwindSafe, RefUnwindSafe};
use std::fmt::{Debug, Formatter};
use std::{fmt};
use std::task::{Context, Poll};
use crate::state::AcquireState::{Available, Queued};
use std::ptr::null;
use std::pin::Pin;
use std::future::Future;
use crate::waker::{WakerResult};
use crate::errors::PoisonError;

/// A [`Future`] returned by [`Semaphore::acquire`] that produces a [`SemaphoreGuard`].
pub struct AcquireFuture<'a>(pub(crate) UnsafeCell<Waiter>, pub(crate) PhantomData<&'a Semaphore>, pub(crate) PhantomPinned);

/// A [`Future`] returned by [`Semaphore::acquire_arc`] that produces a [`SemaphoreGuardArc`].
pub struct AcquireFutureArc {
    pub(crate) arc: Arc<Semaphore>,
    pub(crate) inner: AcquireFuture<'static>,
}

unsafe impl<'a> Sync for AcquireFuture<'a> {}

unsafe impl<'a> Send for AcquireFuture<'a> {}

impl<'a> UnwindSafe for AcquireFuture<'a> {}

impl<'a> RefUnwindSafe for AcquireFuture<'a> {}

impl<'a> Debug for AcquireFuture<'a> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_tuple("AcquireFuture").field(&unsafe { self.waiter() }.amount).finish()
    }
}

impl Debug for AcquireFutureArc {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_tuple("AcquireFutureArc").field(&unsafe { self.inner.waiter() }.amount).finish()
    }
}

impl<'a> AcquireFuture<'a> {
    unsafe fn waiter(&self) -> &Waiter {
        &*self.0.get()
    }
    // Try to acquire or add to queue.
    unsafe fn poll_enter(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<SemaphoreGuard<'a>, PoisonError>> {
        (*self.waiter().semaphore).acquire.transact(|mut acquire| {
            let (available, back) = match *acquire {
                Queued(back) => (0, back),
                Available(available) => {
                    let available = match available.into_usize() {
                        None => {
                            *self.waiter().step.get() = AcquireStep::Done;
                            return Ok(Poll::Ready(Err(PoisonError)));
                        }
                        Some(available) => available,
                    };
                    if self.waiter().amount <= available {
                        *acquire = Available(Permits::new(available - self.waiter().amount));
                        acquire.commit()?;
                        *self.waiter().step.get() = AcquireStep::Done;
                        return Ok(Poll::Ready(Ok(SemaphoreGuard::new(
                            &*self.waiter().semaphore, self.waiter().amount))));
                    } else {
                        (available, null())
                    }
                }
            };
            assert!(self.waiter().waker.poll(cx).is_pending());
            *self.waiter().prev.get() = back;
            *acquire = Queued(self.0.get());
            acquire.commit()?;
            *self.waiter().step.get() = AcquireStep::Waiting;
            // Even if available==0, this is necessary to set release to LockedDirty.
            (*self.waiter().semaphore).release(available);
            Ok(Poll::Pending)
        })
    }

    unsafe fn poll_waiting(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<SemaphoreGuard<'a>, PoisonError>> {
        match self.waiter().waker.poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(poisoned) => {
                *self.waiter().step.get() = AcquireStep::Done;
                if poisoned {
                    Poll::Ready(Err(PoisonError))
                } else {
                    Poll::Ready(Ok(SemaphoreGuard::new(&*self.waiter().semaphore, self.waiter().amount)))
                }
            }
        }
    }
}

impl<'a> Future for AcquireFuture<'a> {
    type Output = Result<SemaphoreGuard<'a>, PoisonError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        unsafe {
            match *(*self.0.get()).step.get() {
                AcquireStep::Entering => {
                    self.poll_enter(cx)
                }
                AcquireStep::Waiting => {
                    self.poll_waiting(cx)
                }
                AcquireStep::Done => panic!("Polling completed future.")
            }
        }
    }
}

impl Future for AcquireFutureArc {
    type Output = Result<SemaphoreGuardArc, PoisonError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        unsafe {
            let this = self.get_unchecked_mut();
            match Pin::new_unchecked(&mut this.inner).poll(cx) {
                Poll::Ready(guard) => {
                    let result =
                        SemaphoreGuardArc::new(this.arc.clone(), guard?.forget());
                    Poll::Ready(Ok(result))
                }
                Poll::Pending => Poll::Pending,
            }
        }
    }
}

impl<'a> Drop for AcquireFuture<'a> {
    fn drop(&mut self) {
        unsafe {
            match *self.waiter().step.get() {
                AcquireStep::Waiting => {
                    // Decide whether the finish or cancel wins if there is a race.
                    match self.waiter().waker.start_cancel() {
                        WakerResult::Cancelling => {
                            // Push onto the cancel queue.
                            (*self.waiter().semaphore).next_cancel.transact(|mut next_cancel| {
                                *self.waiter().next_cancel.get() = *next_cancel;
                                *next_cancel = self.0.get();
                                next_cancel.commit()?;
                                Ok(())
                            });
                            // Ensure a flush of the cancel queue is completed or at least scheduled.
                            (*self.waiter().semaphore).release(0);
                            // Wait for a notification that the node can be dropped
                            self.waiter().waker.wait_cancel();
                        }
                        WakerResult::Finished { poisoned } => {
                            // The acquire finished before it could be cancelled. Pretend like
                            // nothing happened and release the acquired permits.
                            if !poisoned {
                                (*self.waiter().semaphore).release(self.waiter().amount);
                            }
                        }
                    }
                }
                AcquireStep::Entering { .. } => {}
                AcquireStep::Done => {}
            }
        }
    }
}