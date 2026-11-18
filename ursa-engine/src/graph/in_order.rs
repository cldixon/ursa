//! Reading a plan's partitions in parallel, and its batches in partition
//! order (#351).
//!
//! An index build interns its edges in the order the plan's partitions give
//! them, partition 0 first: that order numbers the nodes (see
//! [`super::exec`]). Reading the partitions one after another kept that
//! order, but left one core decoding: a filtered view over a large Parquet
//! file spent its build in zstd decompression on one thread while the other
//! scan workers waited.
//!
//! [`InOrder`] runs every partition as its own task, so they decode at once,
//! and hands their batches on in exactly the order a serial read gives:
//! every batch of partition 0, then every batch of partition 1, and so on.
//! Which batches a partition produces does not depend on when it runs (the
//! session turns off file-scan work stealing), so the index built from them
//! is the one a serial read builds, node numbering and all.
//!
//! A partition ahead of the one being read keeps its batches until their
//! turn comes, within a read-ahead budget for all of them; past that it
//! waits. The partition being read can always hand on a batch or two, so
//! the read never waits on batches that are themselves waiting for it.
//!
//! The budget is memory the build holds on top of the index it builds, so it
//! is bounded on purpose (#385, N45). Unless `ursa.build_read_ahead` is `SET`,
//! it is [`READ_AHEAD_PER_PARTITION`] (256 MiB) per partition of the plan, at
//! most [`MAX_READ_AHEAD`] (1 GiB, what every build held before) and at most
//! an eighth of the memory available to the process (see
//! [`default_read_ahead`]): 1 GiB from 4 partitions up (one per core, as a
//! rule), unless memory is short. Each partition but the one being read
//! needs room to decode ahead, or it waits and the one being read decodes
//! alone; less than 1 GiB slowed a 96M-edge build on 4 cores by about 10%
//! (512 MiB) to 13% (256 MiB), and more did not speed it up, since then
//! interning, not decoding, is what the build waits on. So a plan of fewer
//! partitions gets less, and a machine short of memory gets less, at that
//! cost in time. `SET ursa.build_read_ahead = 0` reads the partitions one
//! after another, holding nothing ahead.
//!
//! In the browser (wasm32) there is no runtime to spawn on, and a plan with
//! one partition gains nothing from a task: both are read serially, on the
//! task that polls the reader.

use std::sync::Arc;

use datafusion::arrow::record_batch::RecordBatch;
use datafusion::error::Result;
use datafusion::execution::TaskContext;
use datafusion::physical_plan::{
    ExecutionPlan, ExecutionPlanProperties, SendableRecordBatchStream,
};
use datafusion::prelude::SessionConfig;
use futures::StreamExt;

/// The most bytes of decoded batches the partitions read ahead of the one
/// being read may hold between them by default: about 64M `Int64` endpoint
/// pairs.
pub(crate) const MAX_READ_AHEAD: usize = 1 << 30;

/// The default read-ahead per partition of the plan read.
pub(crate) const READ_AHEAD_PER_PARTITION: usize = 256 << 20;

/// The default read-ahead is at most this share (one in N) of the memory
/// available to the process.
const READ_AHEAD_MEMORY_SHARE: u64 = 8;

/// The default read-ahead of a plan of `partitions` partitions, when
/// `available` bytes of memory are available to the process (`None` if that
/// cannot be read): [`READ_AHEAD_PER_PARTITION`] per partition, at most
/// [`MAX_READ_AHEAD`] and at most an eighth of `available` (see the module
/// docs).
pub(crate) fn default_read_ahead(partitions: usize, available: Option<u64>) -> usize {
    let scaled = READ_AHEAD_PER_PARTITION
        .saturating_mul(partitions.max(1))
        .min(MAX_READ_AHEAD);
    match available {
        Some(bytes) => {
            scaled.min(usize::try_from(bytes / READ_AHEAD_MEMORY_SHARE).unwrap_or(usize::MAX))
        }
        None => scaled,
    }
}

/// The read-ahead a read of `partitions` partitions may hold in `config`'s
/// session: `ursa.build_read_ahead` if it is `SET` (`None`, read serially,
/// when it is 0), otherwise [`default_read_ahead`].
pub(crate) fn read_ahead(config: &SessionConfig, partitions: usize) -> Option<usize> {
    match crate::options::build_read_ahead_set(config) {
        Some(0) => None,
        Some(bytes) => Some(usize::try_from(bytes).unwrap_or(usize::MAX)),
        None => Some(default_read_ahead(partitions, available_memory())),
    }
}

/// The bytes of memory available to this process, as far as the system
/// says: Linux's `MemAvailable`, less if the process's cgroup (v2) allows
/// less. `None` elsewhere, or if neither can be read.
pub(crate) fn available_memory() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let meminfo = std::fs::read_to_string("/proc/meminfo")
            .ok()
            .and_then(|text| {
                let line = text.lines().find(|l| l.starts_with("MemAvailable:"))?;
                let kib: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
                Some(kib.saturating_mul(1024))
            });
        let read = |name: &str| -> Option<u64> {
            std::fs::read_to_string(format!("/sys/fs/cgroup/{name}"))
                .ok()?
                .trim()
                .parse()
                .ok()
        };
        // `memory.max` is `max` when the cgroup sets no limit.
        let cgroup =
            read("memory.max").map(|max| max.saturating_sub(read("memory.current").unwrap_or(0)));
        match (meminfo, cgroup) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// The batches the partition being read may hand on past the read-ahead
/// budget.
#[cfg(not(target_arch = "wasm32"))]
const HEAD_DEPTH: usize = 2;

/// The batches of a plan's partitions, partition by partition in order,
/// decoded in parallel where they can be (see the module docs).
pub(crate) struct InOrder {
    inner: Inner,
}

enum Inner {
    Serial(Serial),
    #[cfg(not(target_arch = "wasm32"))]
    Parallel(parallel::Parallel),
}

impl InOrder {
    /// The batches of `plan`, decoding its partitions in parallel within the
    /// session's read-ahead ([`read_ahead`]).
    pub(crate) fn new(plan: &Arc<dyn ExecutionPlan>, context: &Arc<TaskContext>) -> Self {
        let partitions = plan.output_partitioning().partition_count();
        let budget = read_ahead(context.session_config(), partitions);
        Self::with_budget(plan, context, budget)
    }

    /// The batches of `plan`, read one partition after another.
    #[cfg(test)]
    pub(crate) fn serial(plan: &Arc<dyn ExecutionPlan>, context: &Arc<TaskContext>) -> Self {
        Self::with_budget(plan, context, None)
    }

    /// In parallel within `budget` read-ahead bytes; serially with none.
    pub(crate) fn with_budget(
        plan: &Arc<dyn ExecutionPlan>,
        context: &Arc<TaskContext>,
        budget: Option<usize>,
    ) -> Self {
        let partitions = plan.output_partitioning().partition_count();
        #[cfg(not(target_arch = "wasm32"))]
        if let (Some(budget), true) = (budget, partitions > 1) {
            return Self {
                inner: Inner::Parallel(parallel::Parallel::start(plan, context, budget)),
            };
        }
        let _ = budget;
        Self {
            inner: Inner::Serial(Serial {
                plan: Arc::clone(plan),
                context: Arc::clone(context),
                partitions,
                next: 0,
                stream: None,
            }),
        }
    }

    /// The next batch, in partition order; `None` after the last.
    pub(crate) async fn next(&mut self) -> Option<Result<RecordBatch>> {
        match &mut self.inner {
            Inner::Serial(serial) => serial.next().await,
            #[cfg(not(target_arch = "wasm32"))]
            Inner::Parallel(parallel) => parallel.next().await,
        }
    }
}

/// Partitions read one after another, on the task that polls.
struct Serial {
    plan: Arc<dyn ExecutionPlan>,
    context: Arc<TaskContext>,
    partitions: usize,
    /// The partition to open next.
    next: usize,
    stream: Option<SendableRecordBatchStream>,
}

impl Serial {
    async fn next(&mut self) -> Option<Result<RecordBatch>> {
        loop {
            if let Some(stream) = &mut self.stream {
                match stream.next().await {
                    Some(batch) => return Some(batch),
                    None => self.stream = None,
                }
            }
            if self.next >= self.partitions {
                return None;
            }
            match self.plan.execute(self.next, Arc::clone(&self.context)) {
                Ok(stream) => self.stream = Some(stream),
                Err(e) => {
                    // Nothing after an error.
                    self.next = self.partitions;
                    return Some(Err(e));
                }
            }
            self.next += 1;
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
mod parallel {
    use std::sync::{Arc, Mutex};

    use datafusion::arrow::record_batch::RecordBatch;
    use datafusion::common::runtime::SpawnedTask;
    use datafusion::error::{DataFusionError, Result};
    use datafusion::execution::TaskContext;
    use datafusion::physical_plan::{ExecutionPlan, ExecutionPlanProperties};
    use futures::StreamExt;
    use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
    use tokio::sync::Notify;

    use super::HEAD_DEPTH;

    /// What a partition's task hands the reader.
    enum Item {
        /// A batch, and the bytes it holds against the read-ahead budget.
        Batch(RecordBatch, usize),
        Error(DataFusionError),
        /// The partition has no more batches.
        End,
    }

    /// The read-ahead budget the partitions' tasks share.
    struct Budget {
        limit: usize,
        state: Mutex<State>,
        /// Signalled when bytes are freed or the partition being read
        /// changes.
        changed: Notify,
    }

    struct State {
        /// The partition being read.
        head: usize,
        /// Bytes of batches handed on and not yet read, over all partitions.
        held: usize,
        /// Batches handed on and not yet read, per partition.
        pending: Vec<usize>,
    }

    impl Budget {
        fn state(&self) -> std::sync::MutexGuard<'_, State> {
            self.state.lock().unwrap_or_else(|e| e.into_inner())
        }

        /// Wait until `partition` may hand on a batch of `bytes`, and count
        /// it: when the batch fits the budget, or `partition` is the one
        /// being read and has fewer than [`HEAD_DEPTH`] batches waiting.
        async fn admit(&self, partition: usize, bytes: usize) {
            loop {
                // Registered before the check, so a change made after it
                // still wakes this wait.
                let changed = self.changed.notified();
                {
                    let mut state = self.state();
                    let fits = state.held + bytes <= self.limit;
                    let head = state.head == partition && state.pending[partition] < HEAD_DEPTH;
                    if fits || head {
                        state.held += bytes;
                        state.pending[partition] += 1;
                        return;
                    }
                }
                changed.await;
            }
        }

        /// A batch of `bytes` from `partition` has been read.
        fn release(&self, partition: usize, bytes: usize) {
            {
                let mut state = self.state();
                state.held -= bytes;
                state.pending[partition] -= 1;
            }
            self.changed.notify_waiters();
        }

        /// `head` is now the partition being read.
        fn advance(&self, head: usize) {
            self.state().head = head;
            self.changed.notify_waiters();
        }
    }

    pub(super) struct Parallel {
        budget: Arc<Budget>,
        receivers: Vec<UnboundedReceiver<Item>>,
        /// The partition being read.
        head: usize,
        /// The partitions' tasks, aborted when the reader is dropped.
        _tasks: Vec<SpawnedTask<()>>,
    }

    impl Parallel {
        pub(super) fn start(
            plan: &Arc<dyn ExecutionPlan>,
            context: &Arc<TaskContext>,
            limit: usize,
        ) -> Self {
            let partitions = plan.output_partitioning().partition_count();
            let budget = Arc::new(Budget {
                limit,
                state: Mutex::new(State {
                    head: 0,
                    held: 0,
                    pending: vec![0; partitions],
                }),
                changed: Notify::new(),
            });
            let mut receivers = Vec::with_capacity(partitions);
            let mut tasks = Vec::with_capacity(partitions);
            for partition in 0..partitions {
                let (tx, rx) = unbounded_channel();
                receivers.push(rx);
                let (plan, context) = (Arc::clone(plan), Arc::clone(context));
                let budget = Arc::clone(&budget);
                tasks.push(SpawnedTask::spawn(read_partition(
                    plan, partition, context, budget, tx,
                )));
            }
            Self {
                budget,
                receivers,
                head: 0,
                _tasks: tasks,
            }
        }

        pub(super) async fn next(&mut self) -> Option<Result<RecordBatch>> {
            while self.head < self.receivers.len() {
                match self.receivers[self.head].recv().await {
                    Some(Item::Batch(batch, bytes)) => {
                        self.budget.release(self.head, bytes);
                        return Some(Ok(batch));
                    }
                    Some(Item::Error(e)) => {
                        self.head = self.receivers.len();
                        return Some(Err(e));
                    }
                    Some(Item::End) => {
                        self.head += 1;
                        self.budget.advance(self.head);
                    }
                    None => {
                        // The task stopped without saying it was done (it
                        // panicked): its partition's rows are not all here.
                        let partition = self.head;
                        self.head = self.receivers.len();
                        return Some(Err(DataFusionError::Execution(format!(
                            "reading partition {partition} of a graph's edges stopped early"
                        ))));
                    }
                }
            }
            None
        }
    }

    /// Read `partition` of `plan`, handing its batches on within the budget.
    async fn read_partition(
        plan: Arc<dyn ExecutionPlan>,
        partition: usize,
        context: Arc<TaskContext>,
        budget: Arc<Budget>,
        tx: UnboundedSender<Item>,
    ) {
        let mut stream = match plan.execute(partition, context) {
            Ok(stream) => stream,
            Err(e) => {
                let _ = tx.send(Item::Error(e));
                return;
            }
        };
        while let Some(batch) = stream.next().await {
            let batch = match batch {
                Ok(batch) => batch,
                Err(e) => {
                    let _ = tx.send(Item::Error(e));
                    return;
                }
            };
            let bytes = batch.get_array_memory_size();
            budget.admit(partition, bytes).await;
            if tx.send(Item::Batch(batch, bytes)).is_err() {
                // The reader is gone.
                return;
            }
        }
        let _ = tx.send(Item::End);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use datafusion::arrow::array::{Array, Int64Array};
    use datafusion::prelude::{SessionConfig, SessionContext};

    /// Every value of the first column `reader` gives, in order.
    async fn values(mut reader: InOrder) -> Vec<i64> {
        let mut out = Vec::new();
        while let Some(batch) = reader.next().await {
            let batch = batch.unwrap();
            let column = batch.column(0);
            let column = column.as_any().downcast_ref::<Int64Array>().unwrap();
            out.extend(column.values().iter().copied());
        }
        out
    }

    #[test]
    fn the_default_read_ahead_scales_with_partitions_within_the_memory() {
        const MIB: usize = 1 << 20;
        assert_eq!(default_read_ahead(1, None), 256 * MIB);
        assert_eq!(default_read_ahead(3, None), 768 * MIB);
        assert_eq!(default_read_ahead(4, None), MAX_READ_AHEAD);
        assert_eq!(default_read_ahead(64, None), MAX_READ_AHEAD);
        // An eighth of the memory available, at most.
        assert_eq!(default_read_ahead(16, Some(16 << 30)), MAX_READ_AHEAD);
        assert_eq!(default_read_ahead(16, Some(2 << 30)), 256 * MIB);
        assert_eq!(default_read_ahead(2, Some(8 << 30)), 512 * MIB);
        assert_eq!(default_read_ahead(4, Some(0)), 0);
    }

    #[test]
    fn the_read_ahead_setting_overrides_the_default() {
        let set = |value: &str| {
            let mut config = crate::options::with_options(SessionConfig::new());
            config
                .options_mut()
                .set("ursa.build_read_ahead", value)
                .unwrap();
            read_ahead(&config, 4)
        };
        assert_eq!(set("0"), None);
        assert_eq!(set("16MiB"), Some(16 << 20));
        let auto = set("auto").unwrap();
        assert!(auto > 0 && auto <= MAX_READ_AHEAD, "{auto}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn partitions_read_in_parallel_come_in_partition_order() {
        let ctx = SessionContext::new_with_config(SessionConfig::new().with_target_partitions(8));
        // A union has one partition per input; each is a run of values, so
        // partition order is value order.
        let sql = (0..8)
            .map(|i| {
                format!(
                    "SELECT value AS v FROM generate_series({}, {})",
                    i * 100_000,
                    i * 100_000 + 99_999
                )
            })
            .collect::<Vec<_>>()
            .join(" UNION ALL ");
        let plan = ctx
            .sql(&sql)
            .await
            .unwrap()
            .create_physical_plan()
            .await
            .unwrap();
        assert!(plan.output_partitioning().partition_count() > 1);
        let context = ctx.task_ctx();
        let serial = values(InOrder::serial(&plan, &context)).await;
        assert_eq!(serial, (0..800_000).collect::<Vec<i64>>());
        assert_eq!(values(InOrder::new(&plan, &context)).await, serial);
        // A budget too small for any batch: each partition waits its turn,
        // but nothing deadlocks and the order holds.
        assert_eq!(
            values(InOrder::with_budget(&plan, &context, Some(1))).await,
            serial
        );
    }
}
