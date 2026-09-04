//! Bounded execution backends for codec reconstruction work.

#[cfg(not(target_arch = "wasm32"))]
use std::sync::OnceLock;
use std::{
    collections::VecDeque,
    fmt,
    sync::{Arc, Mutex},
};

/// One owned unit of codec work.
pub type DecodeTask = Box<dyn FnOnce() + Send + 'static>;

/// Failure to enqueue decoder work.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubmitError {
    message: String,
}

impl SubmitError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for SubmitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for SubmitError {}

/// Shared boundary between codec work planning and its execution backend.
///
/// Native backends execute submitted work asynchronously. Cooperative backends retain work until
/// [`DecodeExecutor::poll`] is called, allowing a browser host to yield between bounded units.
pub trait DecodeExecutor: fmt::Debug + Send + Sync {
    /// Enqueues one owned unit of work without blocking for queue capacity.
    ///
    /// # Errors
    ///
    /// Returns an error when the bounded queue is full or the executor has stopped.
    fn submit(&self, task: DecodeTask) -> Result<(), SubmitError>;

    /// Executes up to `max_tasks` cooperative jobs and returns the number completed.
    ///
    /// Native executors return zero because their worker threads make progress independently.
    fn poll(&self, max_tasks: usize) -> usize;

    /// Returns the maximum number of jobs that can execute simultaneously.
    fn parallelism(&self) -> usize;

    /// Returns whether work requires calls to [`DecodeExecutor::poll`] to make progress.
    fn is_cooperative(&self) -> bool;
}

/// Deterministic single-thread executor used by baseline WebAssembly and tests.
pub struct InlineDecodeExecutor {
    queue: Mutex<VecDeque<DecodeTask>>,
    capacity: usize,
}

impl InlineDecodeExecutor {
    /// Creates a cooperative executor with a bounded queue.
    ///
    /// # Errors
    ///
    /// Returns an error when `capacity` is zero.
    pub fn new(capacity: usize) -> Result<Self, SubmitError> {
        if capacity == 0 {
            return Err(SubmitError::new(
                "inline decode executor capacity must be positive",
            ));
        }
        Ok(Self {
            queue: Mutex::new(VecDeque::with_capacity(capacity)),
            capacity,
        })
    }

    fn queued_tasks(&self) -> usize {
        self.queue.lock().map_or(0, |queue| queue.len())
    }
}

impl fmt::Debug for InlineDecodeExecutor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("InlineDecodeExecutor")
            .field("capacity", &self.capacity)
            .field("queued_tasks", &self.queued_tasks())
            .finish_non_exhaustive()
    }
}

impl DecodeExecutor for InlineDecodeExecutor {
    fn submit(&self, task: DecodeTask) -> Result<(), SubmitError> {
        let mut queue = self
            .queue
            .lock()
            .map_err(|_| SubmitError::new("inline decode executor queue is poisoned"))?;
        if queue.len() >= self.capacity {
            return Err(SubmitError::new("inline decode executor queue is full"));
        }
        queue.push_back(task);
        Ok(())
    }

    fn poll(&self, max_tasks: usize) -> usize {
        let mut completed = 0;
        while completed < max_tasks {
            let task = self
                .queue
                .lock()
                .ok()
                .and_then(|mut queue| queue.pop_front());
            let Some(task) = task else {
                break;
            };
            task();
            completed += 1;
        }
        completed
    }

    fn parallelism(&self) -> usize {
        1
    }

    fn is_cooperative(&self) -> bool {
        true
    }
}

#[cfg(not(target_arch = "wasm32"))]
mod native {
    use std::{
        sync::mpsc::{self, Receiver, SyncSender, TrySendError},
        thread::{self, JoinHandle},
    };

    use super::{Arc, DecodeExecutor, DecodeTask, Mutex, OnceLock, SubmitError, fmt};

    struct NativeInner {
        sender: Option<SyncSender<DecodeTask>>,
        workers: Vec<JoinHandle<()>>,
        capacity: usize,
    }

    impl fmt::Debug for NativeInner {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter
                .debug_struct("NativeInner")
                .field("parallelism", &self.workers.len())
                .field("capacity", &self.capacity)
                .finish_non_exhaustive()
        }
    }

    impl Drop for NativeInner {
        fn drop(&mut self) {
            self.sender.take();
            for worker in self.workers.drain(..) {
                let _ = worker.join();
            }
        }
    }

    /// Fixed-size native worker pool with a bounded submission queue.
    #[derive(Clone, Debug)]
    pub struct NativeDecodeExecutor {
        inner: Arc<NativeInner>,
    }

    impl NativeDecodeExecutor {
        /// Starts `parallelism` workers sharing a queue of `capacity` jobs.
        ///
        /// # Errors
        ///
        /// Returns an error for zero limits or when a worker thread cannot be created.
        pub fn new(parallelism: usize, capacity: usize) -> Result<Self, SubmitError> {
            if parallelism == 0 || capacity == 0 {
                return Err(SubmitError::new(
                    "native decode executor limits must be positive",
                ));
            }
            let (sender, receiver) = mpsc::sync_channel(capacity);
            let receiver = Arc::new(Mutex::new(receiver));
            let mut workers = Vec::with_capacity(parallelism);
            for index in 0..parallelism {
                let receiver = Arc::clone(&receiver);
                match thread::Builder::new()
                    .name(format!("mmrecode-decode-{index}"))
                    .spawn(move || worker_loop(&receiver))
                {
                    Ok(worker) => workers.push(worker),
                    Err(error) => {
                        drop(sender);
                        for worker in workers {
                            let _ = worker.join();
                        }
                        return Err(SubmitError::new(format!(
                            "cannot start native decode worker: {error}"
                        )));
                    }
                }
            }
            Ok(Self {
                inner: Arc::new(NativeInner {
                    sender: Some(sender),
                    workers,
                    capacity,
                }),
            })
        }
    }

    fn worker_loop(receiver: &Mutex<Receiver<DecodeTask>>) {
        loop {
            let task = receiver
                .lock()
                .ok()
                .and_then(|receiver| receiver.recv().ok());
            let Some(task) = task else {
                return;
            };
            task();
        }
    }

    impl DecodeExecutor for NativeDecodeExecutor {
        fn submit(&self, task: DecodeTask) -> Result<(), SubmitError> {
            let Some(sender) = &self.inner.sender else {
                return Err(SubmitError::new("native decode executor has stopped"));
            };
            sender.try_send(task).map_err(|error| match error {
                TrySendError::Full(_) => SubmitError::new("native decode executor queue is full"),
                TrySendError::Disconnected(_) => {
                    SubmitError::new("native decode executor has stopped")
                }
            })
        }

        fn poll(&self, _max_tasks: usize) -> usize {
            0
        }

        fn parallelism(&self) -> usize {
            self.inner.workers.len()
        }

        fn is_cooperative(&self) -> bool {
            false
        }
    }

    pub(super) fn default_executor() -> Result<Arc<dyn DecodeExecutor>, String> {
        static EXECUTOR: OnceLock<Result<Arc<NativeDecodeExecutor>, SubmitError>> = OnceLock::new();
        let executor = EXECUTOR.get_or_init(|| {
            let available = thread::available_parallelism().map_or(1, usize::from);
            let parallelism = available.saturating_sub(1).max(1);
            NativeDecodeExecutor::new(parallelism, parallelism.saturating_mul(64)).map(Arc::new)
        });
        executor
            .as_ref()
            .map(|executor| Arc::clone(executor) as Arc<dyn DecodeExecutor>)
            .map_err(ToString::to_string)
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub use native::NativeDecodeExecutor;

/// Returns the process-wide native pool or the cooperative WebAssembly executor.
///
/// # Errors
///
/// Returns an error when the native worker pool cannot be started.
pub fn default_decode_executor() -> Result<Arc<dyn DecodeExecutor>, String> {
    #[cfg(not(target_arch = "wasm32"))]
    {
        native::default_executor()
    }
    #[cfg(target_arch = "wasm32")]
    {
        InlineDecodeExecutor::new(256)
            .map(|executor| Arc::new(executor) as Arc<dyn DecodeExecutor>)
            .map_err(|error| error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    #[cfg(not(target_arch = "wasm32"))]
    use super::NativeDecodeExecutor;
    use super::{DecodeExecutor, InlineDecodeExecutor};

    #[test]
    fn inline_executor_runs_only_the_polled_budget() {
        let executor = InlineDecodeExecutor::new(3).unwrap();
        let completed = Arc::new(AtomicUsize::new(0));
        for _ in 0..3 {
            let completed = Arc::clone(&completed);
            executor
                .submit(Box::new(move || {
                    completed.fetch_add(1, Ordering::Relaxed);
                }))
                .unwrap();
        }
        assert_eq!(executor.poll(2), 2);
        assert_eq!(completed.load(Ordering::Relaxed), 2);
        assert_eq!(executor.poll(2), 1);
        assert_eq!(completed.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn inline_executor_applies_backpressure() {
        let executor = InlineDecodeExecutor::new(1).unwrap();
        executor.submit(Box::new(|| {})).unwrap();
        assert!(executor.submit(Box::new(|| {})).is_err());
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn native_executor_runs_jobs_concurrently() {
        let executor = NativeDecodeExecutor::new(2, 4).unwrap();
        let (started_sender, started_receiver) = std::sync::mpsc::channel();
        let (first_release, first_wait) = std::sync::mpsc::channel();
        let first_started = started_sender.clone();
        executor
            .submit(Box::new(move || {
                first_started.send(1).unwrap();
                first_wait.recv().unwrap();
            }))
            .unwrap();
        let (second_release, second_wait) = std::sync::mpsc::channel();
        executor
            .submit(Box::new(move || {
                started_sender.send(2).unwrap();
                second_wait.recv().unwrap();
            }))
            .unwrap();
        let timeout = std::time::Duration::from_secs(2);
        let mut started = [
            started_receiver.recv_timeout(timeout).unwrap(),
            started_receiver.recv_timeout(timeout).unwrap(),
        ];
        started.sort_unstable();
        assert_eq!(started, [1, 2]);
        assert_eq!(executor.parallelism(), 2);
        first_release.send(()).unwrap();
        second_release.send(()).unwrap();
    }
}
