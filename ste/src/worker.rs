use crate::adapter::Adapter;
use crate::linked_list::{LinkedList, Node};
use crate::loom::sync::atomic::{AtomicIsize, Ordering};
use crate::loom::sync::Mutex;
use crate::parker::Parker;
use crate::tagged::Tag;
use crate::Panicked;
use std::mem;
use std::ptr;
use std::task::Waker;

/// The type of the prelude function.
pub(super) type Prelude = dyn Fn() + Send + 'static;

struct Locked {
    running: bool,
    queue: LinkedList<Entry>,
}

// Shared state between the worker thread and [Thread].
pub(super) struct Shared {
    locked: Mutex<Locked>,
    parker: Parker,
}

impl Shared {
    /// Construct new shared state.
    pub(super) fn new() -> Self {
        Self {
            locked: Mutex::new(Locked {
                running: true,
                queue: LinkedList::new(),
            }),
            parker: Parker::new(),
        }
    }

    // Release all shared state, this will hang until the number of modifiers is
    // zero, after which it will pop all elements from the queue and release
    // them.
    unsafe fn panic_join(&self) {
        let mut local = {
            let mut guard = self.locked.lock().unwrap();
            debug_assert!(guard.running);
            guard.running = false;
            guard.queue.steal()
        };

        release_local_queue(&mut local);
    }

    /// Process the given entry on the remote thread.
    ///
    /// # Safety
    ///
    /// We're sending the entry to be executed on a remote thread, the caller
    /// must assure that anything being referenced in it is owned by the caller
    /// and will not be dropped or deallocated for the duration of this call.
    pub(super) unsafe fn schedule_in_place(
        &self,
        parker: ptr::NonNull<Parker>,
        entry: Entry,
    ) -> Result<(), Panicked> {
        let mut node = Node::new(entry);

        let first = {
            let mut guard = self.locked.lock().unwrap();

            if !guard.running {
                return Err(Panicked(()));
            }

            guard.queue.push_front(ptr::NonNull::from(&mut node))
        };

        if first {
            self.parker.unpark();
        }

        // NB: We must park here until the remote task wakes us up to allow
        // the task to access things from the environment in the other
        // thread safely.
        //
        // We also know fully that the parker is balanced - i.e. there are
        // no sporadic wakes that can happen because we contrl the state of
        // the submitted task exactly above.
        parker.as_ref().park();
        Ok(())
    }

    /// What should happen when the shared state is joined.
    ///
    /// We mark the modifiers count as negative to signal any entering threads
    /// that they are no longer permitted to push tasks onto the task set.
    pub(super) fn outer_join(&self) {
        self.locked.lock().unwrap().running = false;
        self.parker.unpark();
    }
}

pub(super) struct ModifierGuard<'a> {
    modifiers: &'a AtomicIsize,
}

impl Drop for ModifierGuard<'_> {
    fn drop(&mut self) {
        self.modifiers.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Worker thread.
pub(super) fn run(prelude: Option<Box<Prelude>>, shared: ptr::NonNull<Shared>) {
    unsafe {
        let shared = shared.as_ref();
        let tag = Tag(shared as *const _ as usize);

        if let Some(prelude) = prelude {
            let guard = PoisonGuard { shared };
            prelude();
            mem::forget(guard);
        }

        loop {
            let mut local = {
                let mut guard = shared.locked.lock().unwrap();

                if !guard.running {
                    release_local_queue(&mut guard.queue);
                    break;
                }

                guard.queue.steal()
            };

            if local.is_empty() {
                shared.parker.park();
                continue;
            }

            while let Some(mut entry) = local.pop_front() {
                match &mut entry.as_mut().value {
                    Entry::Poll(poll) => {
                        // Safety: At this point, we know the waker has been
                        // replaced by the polling task and can safely deref it into
                        // the underlying waker.
                        let waker = poll.waker.as_ref();

                        let guard = WakerPoisonGuard {
                            shared,
                            waker,
                            parker: poll.parker.as_ref(),
                            local: &mut local,
                        };

                        poll.adapter.as_mut().poll(tag, waker);
                        mem::forget(guard);
                        poll.parker.as_ref().unpark();
                    }
                    Entry::Schedule(schedule) => {
                        let guard = SchedulePoisonGuard {
                            shared,
                            parker: schedule.parker.as_ref(),
                            local: &mut local,
                        };

                        schedule.task.as_mut()(tag);
                        mem::forget(guard);
                        schedule.parker.as_ref().unpark();
                    }
                }
            }
        }
    }

    /// Guard used to mark the state of the executed as "panicked". This is
    /// accomplished by asserting that the only reason this destructor would
    /// be called would be due to an unwinding panic.
    struct PoisonGuard<'a> {
        shared: &'a Shared,
    }

    impl Drop for PoisonGuard<'_> {
        fn drop(&mut self) {
            unsafe {
                self.shared.panic_join();
            }
        }
    }

    struct WakerPoisonGuard<'a> {
        shared: &'a Shared,
        waker: &'a Waker,
        parker: &'a Parker,
        local: &'a mut LinkedList<Entry>,
    }

    impl Drop for WakerPoisonGuard<'_> {
        fn drop(&mut self) {
            // Safety: We know that the task holding the flag owns the
            // reference.
            unsafe {
                self.shared.panic_join();
                self.waker.wake_by_ref();
                self.parker.unpark();
                release_local_queue(self.local);
            }
        }
    }

    struct SchedulePoisonGuard<'a> {
        shared: &'a Shared,
        parker: &'a Parker,
        local: &'a mut LinkedList<Entry>,
    }

    impl<'a> Drop for SchedulePoisonGuard<'a> {
        fn drop(&mut self) {
            // Safety: We know that the task holding the flag owns the
            // reference.
            unsafe {
                self.shared.panic_join();
                self.parker.unpark();
                release_local_queue(self.local);
            }
        }
    }
}

/// An entry onto the task queue.
#[derive(Debug)]
pub(super) enum Entry {
    /// An entry to immediately be scheduled.
    Schedule(ScheduleEntry),
    /// An entry to be polled.
    Poll(PollEntry),
}

impl Entry {
    /// Release all resources associated with the entry.
    unsafe fn release(&self) {
        match self {
            Entry::Poll(poll) => {
                poll.waker.as_ref().wake_by_ref();
                poll.parker.as_ref().unpark();
            }
            Entry::Schedule(schedule) => {
                schedule.parker.as_ref().unpark();
            }
        }
    }
}

/// A task submitted to the executor.
#[derive(Debug)]
pub(super) struct ScheduleEntry {
    task: ptr::NonNull<dyn FnMut(Tag) + Send + 'static>,
    parker: ptr::NonNull<Parker>,
}

impl ScheduleEntry {
    pub(super) unsafe fn new(
        task: ptr::NonNull<dyn FnMut(Tag) + Send + 'static>,
        parker: ptr::NonNull<Parker>,
    ) -> Self {
        Self { task, parker }
    }
}

/// A task to be polled by the scheduler.
#[derive(Debug)]
pub(super) struct PollEntry {
    /// Polling adapter to poll.
    adapter: ptr::NonNull<dyn Adapter + 'static>,
    /// Waker to use.
    waker: ptr::NonNull<Waker>,
    /// Parker to wake once a poll completes.
    parker: ptr::NonNull<Parker>,
}

impl PollEntry {
    pub(super) unsafe fn new(
        adapter: ptr::NonNull<dyn Adapter + 'static>,
        waker: ptr::NonNull<Waker>,
        parker: ptr::NonNull<Parker>,
    ) -> Self {
        Self {
            adapter,
            waker,
            parker,
        }
    }
}

/// Helper function to release a local queue.
///
/// This is useful when a queue is stolen, because it disassociates the stolen
/// part of the queue from the rest.
unsafe fn release_local_queue(queue: &mut LinkedList<Entry>) {
    while let Some(entry) = queue.pop_back() {
        entry.as_ref().value.release();
    }
}
