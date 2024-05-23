use std::{
    cell::Cell,
    future::Future,
    mem::offset_of,
    pin::Pin,
    sync::{atomic::Ordering, Arc},
    task::{Context, Poll},
};

mod atomic_waker;
mod queue;
mod task;
mod vtable;
mod waker;

use crate::{
    runtime::schedular::task::{ErasedTask, Task},
    util::Defer,
};
use queue::Queue;

use self::task::ErasedTaskPtr;

/// A value returned by polling the rquickjs schedular informing about the current state of the
/// schedular and what action it's caller should take to propely drive the pending futures.
#[derive(Debug, Eq, PartialEq, Clone, Copy, Hash)]
pub enum SchedularPoll {
    /// The schedular has determined that a future needs to yield back to the root executor.
    /// If this value is returned by the schedular future calls to poll will likely also return
    /// ShouldYield until the current task has yield to the root executor.
    ShouldYield,
    /// There are no spawned futures so no work could be done.
    Empty,
    /// All futures currently spawned in the schedular are pending and no progress could be made.
    Pending,
    /// There are still futures which are pending, but some futures were awoken and were polled
    /// again, possibly finishing or possibly becoming pending again.
    PendingProgress,
}

pub struct Schedular {
    len: Cell<usize>,
    reentrant: Cell<usize>,
    should_poll: Arc<Queue>,
    all_next: Cell<Option<ErasedTaskPtr>>,
    all_prev: Cell<Option<ErasedTaskPtr>>,
}

impl Schedular {
    /// Create a new schedular.
    pub fn new() -> Self {
        let queue = Arc::new(Queue::new());
        unsafe {
            Pin::new_unchecked(&*queue).init();
        }
        Schedular {
            len: Cell::new(0),
            reentrant: Cell::new(0),
            should_poll: queue,
            all_prev: Cell::new(None),
            all_next: Cell::new(None),
        }
    }

    /// Returns if there are no pending tasks.
    pub fn is_empty(&self) -> bool {
        self.all_next.get().is_none()
    }

    /// # Safety
    /// This function erases any lifetime associated with the future.
    /// Caller must ensure that either the future completes or is dropped before the lifetime
    pub unsafe fn push<F>(&self, f: F)
    where
        F: Future<Output = ()>,
    {
        let queue = Arc::downgrade(&self.should_poll);

        // These should always be the same as task has a repr(C);
        assert_eq!(offset_of!(Task<F>, head), offset_of!(Task<u8>, head));
        assert_eq!(offset_of!(Task<F>, body), offset_of!(Task<u8>, body));

        let task = Arc::new(Task::new(queue, f));

        // One count for the all list and one for the should_poll list.
        let task = ErasedTask::new(task);
        self.push_task_to_all(task.clone());

        let task_ptr = ErasedTask::into_ptr(task);
        Pin::new_unchecked(&*self.should_poll).push(task_ptr.as_node_ptr());
        self.len.set(self.len.get() + 1);
    }

    /// Add a new task to the all task list.
    /// The all task list owns a reference to the task while it is in the list.
    unsafe fn push_task_to_all(&self, task: ErasedTask) {
        let task = ErasedTask::into_ptr(task);

        task.body().next.set(self.all_next.get());

        if let Some(x) = self.all_next.get() {
            x.body().prev.set(Some(task));
        }
        self.all_next.set(Some(task));
        if self.all_prev.get().is_none() {
            self.all_prev.set(Some(task));
        }
    }

    /// Removes the task from the all task list.
    /// Dropping the ownership the list has.
    unsafe fn pop_task_all(&self, task: ErasedTaskPtr) {
        task.body().queued.store(true, Ordering::Release);
        if !task.body().done.replace(true) {
            task.task_drop();
        }

        // detach the task from the all list
        if let Some(next) = task.body().next.get() {
            next.body().prev.set(task.body().prev.get())
        } else {
            self.all_prev.set(task.body().prev.get());
        }
        if let Some(prev) = task.body().prev.get() {
            prev.body().next.set(task.body().next.get())
        } else {
            self.all_next.set(task.body().next.get());
        }

        let _ = unsafe { ErasedTask::from_ptr(task) };
        // drop the ownership of the all list,
        // Task is now dropped or only owned by wakers or
        self.len.set(self.len.get() - 1);
    }

    pub unsafe fn poll(&self, cx: &mut Context) -> SchedularPoll {
        // A task it's ownership is shared among a number of different places.
        // - The all-task list
        // - One or multiple wakers
        // - The should_poll list if scheduled.
        //
        // When a task is retrieved from the should_poll list we transfer it's arc count to a
        // waker. When a waker is cloned it also increments the arc count. If the waker is then
        // woken up the count is transfered back to the should_poll list.

        if self.is_empty() {
            // No tasks, nothing to be done.
            return SchedularPoll::Empty;
        }

        self.should_poll.waker().register(cx.waker());

        let mut iteration = 0;
        let mut yielded = 0;
        let mut popped_running = 0;

        loop {
            // Popped a task, ownership taken from the queue
            let cur = match Pin::new_unchecked(&*self.should_poll).pop() {
                queue::Pop::Empty => {
                    if iteration > 0 {
                        return SchedularPoll::PendingProgress;
                    } else {
                        return SchedularPoll::Pending;
                    }
                }
                queue::Pop::Value(x) => x,
                queue::Pop::Inconsistant => {
                    cx.waker().wake_by_ref();
                    return SchedularPoll::ShouldYield;
                }
            };

            // Take ownership of the task from the schedular.
            let cur_ptr = ErasedTaskPtr::from_nonnull(cur.cast());
            let cur = ErasedTask::from_ptr(cur_ptr);

            if cur.body().done.get() {
                continue;
            }

            // Check for recursive future polling.
            if cur.body().running.get() {
                popped_running += 1;
                Pin::new_unchecked(&*self.should_poll)
                    .push(ErasedTask::into_ptr(cur).as_node_ptr());

                // If we popped more running futures than the reentrant counter then we can be
                // sure that we did a full round of all the popped futures.
                if popped_running > self.reentrant.get() {
                    if iteration > 0 {
                        return SchedularPoll::PendingProgress;
                    } else {
                        return SchedularPoll::Pending;
                    }
                }

                continue;
            }

            let prev = cur.body().queued.swap(false, Ordering::AcqRel);
            assert!(prev);

            // wakers owns the arc count of cur now until the end of the scope.
            // So we can use cur_ptr until the end of the scope waker is only dropped then.
            let waker = waker::get(cur);
            let mut ctx = Context::from_waker(&waker);

            // if drive_task panics we still want to remove the task from the list.
            // So handle it with a drop
            let remove = Defer::new((), |_| self.pop_task_all(cur_ptr));

            iteration += 1;

            // Set reentrant counter, if we ever encounter a non-zero reentrant counter then this
            // function is called recursively.
            self.reentrant.set(self.reentrant.get() + 1);
            cur_ptr.body().running.set(true);
            let res = cur_ptr.task_drive(&mut ctx);
            cur_ptr.body().running.set(false);
            self.reentrant.set(self.reentrant.get() - 1);

            match res {
                Poll::Ready(_) => {
                    // Nothing todo the defer will remove the task from the list.
                }
                Poll::Pending => {
                    cur_ptr.body().running.set(false);

                    // don't remove task from the list.
                    remove.take();

                    // we had a pending and test if a yielded future immediatily queued itself
                    // again.
                    yielded += cur_ptr.body().queued.load(Ordering::Relaxed) as usize;

                    // If we polled all the futures atleas once,
                    // or more then one future immediatily queued itself after being polled,
                    // yield back to the parent schedular.
                    if yielded > 2 || iteration > self.len.get() {
                        cx.waker().wake_by_ref();
                        return SchedularPoll::ShouldYield;
                    }
                }
            }
        }
    }

    /// Remove all tasks from the list.
    pub fn clear(&self) {
        // Clear all pending futures from the all list
        while let Some(c) = self.all_next.get() {
            unsafe { self.pop_task_all(c) }
        }

        loop {
            let cur = match unsafe { Pin::new_unchecked(&*self.should_poll).pop() } {
                queue::Pop::Empty => break,
                queue::Pop::Value(x) => x,
                queue::Pop::Inconsistant => {
                    std::thread::yield_now();
                    continue;
                }
            };

            unsafe { ErasedTask::from_ptr(ErasedTaskPtr::from_nonnull(cur.cast())) };
        }
    }
}

impl Drop for Schedular {
    fn drop(&mut self) {
        self.clear()
    }
}
