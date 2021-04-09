use crate::adapter::Adapter;
use crate::linked_list::LinkedList;
use crate::parker::Unparker;
use crate::state::{State, NONE_READY, STATE_BUSY, STATE_POLLABLE};
use crate::submit_wake::SubmitWake;
use crate::tagged::Tag;
use parking_lot::{Condvar, Mutex};
use std::mem;
use std::ptr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;

/// The type of the prelude function.
pub(super) type Prelude = dyn Fn() + Send + 'static;

// Shared state between the worker thread and [Thread].
pub(super) struct Shared {
    pub(super) locked: Mutex<Locked>,
    pub(super) cond: Condvar,
}

impl Shared {
    /// Construct new shared state.
    pub(super) fn new() -> Self {
        Self {
            locked: Mutex::new(Locked {
                state: State::Default,
                queue: LinkedList::new(),
            }),
            cond: Condvar::new(),
        }
    }

    // Release all shared state.
    unsafe fn release(&self) {
        let mut guard = self.locked.lock();
        let old = mem::replace(&mut guard.state, State::End);

        // It's not possible for the state to be anything but empty
        // here, because the worker thread takes the state before
        // executing user code which might panic.
        debug_assert!(matches!(old, State::Default));

        while let Some(entry) = guard.queue.pop_back() {
            match &entry.as_ref().value {
                Entry::Poll(poll) => {
                    poll.submit_wake.as_ref().release();
                }
                Entry::Schedule(schedule) => {
                    schedule.release();
                }
            }
        }
    }
}

pub(super) struct Locked {
    pub(super) state: State,
    pub(super) queue: LinkedList<Entry>,
}

/// Worker thread.
pub(super) fn run(prelude: Option<Box<Prelude>>, shared: ptr::NonNull<Shared>) {
    if let Some(prelude) = prelude {
        let guard = PoisonGuard { shared };
        prelude();
        mem::forget(guard);
    }

    unsafe {
        'outer: loop {
            let mut guard = shared.as_ref().locked.lock();

            let mut entry = loop {
                match guard.state {
                    State::End => break 'outer,
                    State::Default => (),
                }

                if let Some(entry) = guard.queue.pop_back() {
                    drop(guard);
                    break entry;
                }

                shared.as_ref().cond.wait(&mut guard);
            };

            let tag = Tag(shared.as_ptr() as usize);

            match &mut entry.as_mut().value {
                Entry::Poll(poll) => {
                    let submit_wake = poll.submit_wake.as_ref();

                    let guard = WakerPoisonGuard {
                        shared,
                        submit_wake,
                    };

                    if poll.adapter.as_mut().poll(tag, submit_wake) {
                        submit_wake.state.store(STATE_POLLABLE, Ordering::Release);
                        submit_wake.inner_wake();
                    } else {
                        // Unset the busy flag and poll it if it has been marked
                        // as pollable.
                        if submit_wake.state.fetch_and(!STATE_BUSY, Ordering::AcqRel)
                            & STATE_POLLABLE
                            != 0
                        {
                            if let Some(waker) = &*submit_wake.waker.lock() {
                                waker.wake_by_ref();
                            }
                        }
                    }

                    mem::forget(guard);
                }
                Entry::Schedule(schedule) => {
                    let guard = SchedulePoisonGuard { shared, schedule };
                    guard.schedule.task.as_mut()(tag);
                    mem::forget(guard);
                    schedule.release();
                }
            }
        }

        struct SchedulePoisonGuard<'a> {
            shared: ptr::NonNull<Shared>,
            schedule: &'a mut ScheduleEntry,
        }

        impl<'a> Drop for SchedulePoisonGuard<'a> {
            fn drop(&mut self) {
                // Safety: We know that the task holding the flag owns the
                // reference.
                unsafe {
                    self.shared.as_ref().release();
                    self.schedule.release();
                }
            }
        }
    }

    /// Guard used to mark the state of the executed as "panicked". This is
    /// accomplished by asserting that the only reason this destructor would
    /// be called would be due to an unwinding panic.
    struct PoisonGuard {
        shared: ptr::NonNull<Shared>,
    }

    impl Drop for PoisonGuard {
        fn drop(&mut self) {
            unsafe {
                self.shared.as_ref().release();
            }
        }
    }

    struct WakerPoisonGuard<'a> {
        shared: ptr::NonNull<Shared>,
        submit_wake: &'a SubmitWake,
    }

    impl Drop for WakerPoisonGuard<'_> {
        fn drop(&mut self) {
            unsafe {
                self.shared.as_ref().release();
                self.submit_wake.release();
            }
        }
    }
}

/// An entry onto the task queue.
pub(super) enum Entry {
    /// An entry to immediately be scheduled.
    Schedule(ScheduleEntry),
    /// An entry to be polled.
    Poll(PollEntry),
}

/// A task submitted to the executor.
pub(super) struct ScheduleEntry {
    pub(super) task: ptr::NonNull<dyn FnMut(Tag) + Send + 'static>,
    pub(super) unparker: Unparker,
    pub(super) flag: ptr::NonNull<AtomicUsize>,
}

impl ScheduleEntry {
    pub(super) fn release(&self) {
        unsafe {
            if self.flag.as_ref().fetch_add(1, Ordering::AcqRel) == NONE_READY {
                // We got here first, so we know the calling thread won't
                // park since they will see our update. If the calling
                // thread got here before us, the value would be 1.
                return;
            }

            while !self.unparker.unpark_one() {
                thread::yield_now();
            }
        }
    }
}

/// A task to be polled by the scheduler.
pub(super) struct PollEntry {
    pub(super) adapter: ptr::NonNull<dyn Adapter + 'static>,
    pub(super) submit_wake: ptr::NonNull<Arc<SubmitWake>>,
}