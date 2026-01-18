# DriverActor 深度解析

本文解析 Sail 的 `DriverActor` 如何處理分散式執行，從接收 ExecutionPlan 到返回結果的完整流程。

---

## DriverActor 的角色

在 Sail 的 Cluster Mode 中，`DriverActor` 扮演**協調者**的角色：

```
PySpark Client
     |
SparkConnectService (gRPC Server)
     |
ClusterJobRunner
     |
DriverActor (協調者) ← 本文重點
     |
|--- Tasks 分配到 Workers
     |
返回結果給 Client
```

🔸 **DriverActor 負責的工作**

1. **接收執行計畫**：從 `ClusterJobRunner` 接收 DataFusion 的 `ExecutionPlan`
2. **建構 JobGraph**：分析 Stages，識別 shuffle 邊界
3. **創建 Tasks**：將每個 Stage 的每個 partition 變成一個 Task
4. **管理 Workers**：追蹤 Worker 狀態，處理註冊和心跳
5. **調度 Tasks**：將 Tasks 分配給可用的 Workers
6. **追蹤狀態**：監控 Task 從 Pending → Running → Success/Failed
7. **返回結果**：收集最終 Stage 的結果並返回給 Client

---

## DriverActor 的資料結構

位置：`crates/sail-execution/src/driver/actor/core.rs:20-33`

```rust
pub struct DriverActor {
    // 配置：Driver 運行和 Worker 管理選項
    options: DriverOptions,

    // 核心狀態：追蹤所有 Workers、Jobs、Tasks
    pub(super) state: DriverState,

    // gRPC Server：讓 Workers 可以透過 server 註冊和回報 Task 狀態
    pub(super) server: ServerMonitor,

    // Worker 管理器：Local 或 Kubernetes
    pub(super) worker_manager: Arc<dyn WorkerManager>,

    // Worker gRPC 客戶端：用來向 Worker 發送 RPC 請求
    worker_clients: HashMap<WorkerId, WorkerClient>,

    // 序列化 codec：用來將 ExecutionPlan 編碼後傳給 Worker
    pub(super) physical_plan_codec: Box<dyn PhysicalExtensionCodec>,

    // 核心調度器：等待調度的 Task 佇列
    pub(super) task_queue: VecDeque<TaskId>,

    // Task 序列號：防止過時的狀態更新覆蓋新狀態
    pub(super) task_sequences: HashMap<TaskId, u64>,

    // Job 輸出：用來收集結果並返回給呼叫者
    pub(super) job_outputs: HashMap<JobId, JobOutput>,
}
```

---

## DriverState：追蹤所有狀態

位置：`crates/sail-execution/src/driver/state.rs:14-21`

```rust
pub struct DriverState {
    // 核心狀態：所有 Workers 的狀態
    workers: HashMap<WorkerId, WorkerDescriptor>,

    // 核心狀態：所有 Jobs 的狀態
    jobs: HashMap<JobId, JobDescriptor>,

    // 核心狀態：所有 Tasks 的狀態
    tasks: HashMap<TaskId, TaskDescriptor>,

    // ID 生成器
    job_id_generator: IdGenerator<JobId>,
    task_id_generator: IdGenerator<TaskId>,
    worker_id_generator: IdGenerator<WorkerId>,
}
```

### 🔸 WorkerDescriptor

位置：`crates/sail-execution/src/driver/state.rs:280-305`

```rust
pub struct WorkerDescriptor {
    pub state: WorkerState,
    pub messages: Vec<String>,  // 錯誤訊息
}

pub enum WorkerState {
    // Worker 尚未啟動，等待註冊
    Pending,

    // Worker 正在運行
    Running {
        host: String,
        port: u16,
        tasks: HashSet<TaskId>,        // 正在運行的 Tasks
        jobs: HashSet<JobId>,          // 參與的 Worker 的 Jobs
        updated_at: Instant,           // 最後更新時間
        heartbeat_at: Instant,         // 最後心跳時間
    },

    // Worker 已停止
    Stopped,

    // Worker 失敗
    Failed,
}
```

### 🔸 JobDescriptor

位置：`crates/sail-execution/src/driver/state.rs:308-317`

```rust
pub struct JobDescriptor {
    pub stages: Vec<JobStage>,
}

pub struct JobStage {
    // 這個 Stage 的執行計畫
    pub plan: Arc<dyn ExecutionPlan>,

    // 這個 Stage 的所有 Task IDs（每個 partition 一個）
    pub tasks: Vec<TaskId>,
}
```

### 🔸 TaskDescriptor

位置：`crates/sail-execution/src/driver/state.rs:319-333`

```rust
pub struct TaskDescriptor {
    pub job_id: JobId,
    pub stage: usize,      // 屬於哪個 Stage
    pub partition: usize,  // 處理哪個 partition
    pub attempt: usize,    // 重試次數
    pub mode: TaskMode,    // Blocking 或 Pipelined
    pub state: TaskState,  // 當前狀態
    pub messages: Vec<String>,

    // 核心機制：最終 Stage 會透過這個 channel
    // Worker 將結果寫入 channel，Driver 從中讀取
    pub channel: Option<ChannelName>,
}

pub enum TaskState {
    Created,                           // 剛創建，尚未進入佇列
    Pending,                           // 等待分配給 Worker
    Scheduled { worker_id: WorkerId }, // 已分配給 Worker，等待執行
    Running { worker_id: WorkerId },   // Worker 正在運行
    Succeeded { worker_id: WorkerId }, // 成功
    Failed { worker_id: WorkerId },    // 失敗
}
```

---

## 完整執行流程

從接收執行計畫到返回結果的完整流程：

```
1. ClusterJobRunner.execute()
   | 發送 DriverEvent::ExecuteJob
   v
2. DriverActor.handle_execute_job()
   | 呼叫 accept_job()
   v
3. accept_job()
   | JobGraph::try_new(plan)  // 將 ExecutionPlan 切分成 Stages
   | 將每個 Stage 的每個 partition 變成一個 Task
   | 所有 Tasks 放入 task_queue
   v
4. schedule_tasks()
   | 從 task_queue 取出 Tasks
   | 為每個 Task 找到可用的 Worker
   | 呼叫 schedule_task()
   v
5. schedule_task()
   | 透過 gRPC 呼叫 Worker.RunTask()
   | Task 狀態從 Pending 變成 Scheduled
   v
6. Worker 開始執行
   | 透過 gRPC 回報 UpdateTask(Running)
   v
7. DriverActor.handle_update_task()
   | 更新 Task 狀態為 Running
   | 如果是最終 Stage，監聽 channel 收集結果
   v
8. Worker 執行完成
   | 透過 gRPC 回報 UpdateTask(Succeeded)
   v
9. DriverActor.handle_update_task()
   | 更新 Task 狀態為 Succeeded
   | 檢查 Job 的所有 Tasks 是否完成，完成則 Job 結束
   v
10. 返回結果給 ClusterJobRunner
```

---

## 流程 1：接收執行計畫

位置：`crates/sail-execution/src/driver/actor/handler.rs:183-201`

```rust
pub(super) fn handle_execute_job(
    &mut self,
    ctx: &mut ActorContext<Self>,
    plan: Arc<dyn ExecutionPlan>,  // DataFusion 執行計畫
    result: oneshot::Sender<ExecutionResult<SendableRecordBatchStream>>,
) -> ActorAction {
    // 1. 建構 Job、Stages 和 Tasks
    match self.accept_job(ctx, plan) {
        Ok(job_id) => {
            // 2. 保存 Job 輸出 channel（用來返回結果）
            self.job_outputs.insert(job_id, JobOutput::Pending { result });

            // 3. 啟動 Workers（如果需要）
            self.scale_up_workers(ctx);

            // 4. 核心調度：開始調度 Tasks
            self.schedule_tasks(ctx);
        }
        Err(e) => {
            // 失敗時返回錯誤
            let _ = result.send(Err(e));
        }
    }
    ActorAction::Continue
}
```

---

## 流程 2：建構 JobGraph

### 🔸 什麼是 JobGraph

`JobGraph` 將 DataFusion 的 `ExecutionPlan` 切分成 **Stages**，每個 Stage 之間透過 **Shuffle** 分隔。

**Shuffle 邊界**：出現在
1. `RepartitionExec`：hash/range 重新分區
2. `CoalescePartitionsExec`：將多個 partition 合併成一個

位置：`crates/sail-execution/src/driver/planner.rs:14-40`

```rust
pub struct JobGraph {
    stages: Vec<Arc<dyn ExecutionPlan>>,
}

impl JobGraph {
    pub fn try_new(plan: Arc<dyn ExecutionPlan>) -> ExecutionResult<Self> {
        let mut graph = Self { stages: vec![] };

        // 核心演算法：遞迴建構 JobGraph
        let last = build_job_graph(plan, &mut graph)?;

        // 加入最終 Stage
        graph.stages.push(last);

        Ok(graph)
    }
}
```

### 🔸 build_job_graph 遞迴邏輯

位置：`crates/sail-execution/src/driver/planner.rs:42-87`

```rust
fn build_job_graph(
    plan: Arc<dyn ExecutionPlan>,
    graph: &mut JobGraph,
) -> ExecutionResult<Arc<dyn ExecutionPlan>> {
    // 1. 遞迴處理所有子節點
    let children = plan
        .children()
        .into_iter()
        .map(|x| build_job_graph(x.clone(), graph))
        .collect::<ExecutionResult<Vec<_>>>()?;

    let plan = with_new_children_if_necessary(plan, children)?;

    // 2. 核心邏輯：檢查是否需要插入 Shuffle
    let plan = if let Some(repartition) = plan.as_any().downcast_ref::<RepartitionExec>() {
        // 如果是 RepartitionExec，需要 Shuffle
        match repartition.partitioning() {
            Partitioning::UnknownPartitioning(_) | Partitioning::RoundRobinBatch(_) => {
                // 這些不需要 shuffle
                get_one_child_plan(&plan)?
            }
            partitioning => {
                let child = get_one_child_plan(&plan)?;

                // 核心動作：創建 Shuffle（ShuffleWrite + ShuffleRead）
                create_shuffle(&child, graph, partitioning.clone(), ShuffleConsumption::Single)?
            }
        }
    } else if plan.as_any().downcast_ref::<CoalescePartitionsExec>().is_some() {
        // 如果是 CoalescePartitionsExec，也需要 Shuffle
        let child = get_one_child_plan(&plan)?;
        let partitioning = child.properties().output_partitioning();

        let child = create_shuffle(&child, graph, partitioning.clone(), ShuffleConsumption::Multiple)?;

        with_new_children_if_necessary(plan, vec![child])?
    } else {
        plan
    };

    Ok(plan)
}
```

### 🔸 create_shuffle：插入 ShuffleWrite 和 ShuffleRead

位置：`crates/sail-execution/src/driver/planner.rs:100-121`

```rust
fn create_shuffle(
    plan: &Arc<dyn ExecutionPlan>,
    graph: &mut JobGraph,
    partitioning: Partitioning,
    consumption: ShuffleConsumption,
) -> ExecutionResult<Arc<dyn ExecutionPlan>> {
    let stage = graph.stages.len();

    // 核心動作：創建 ShuffleWriteExec（寫入 shuffle 資料）
    let writer = Arc::new(ShuffleWriteExec::new(
        stage,
        plan.clone(),
        partitioning.clone(),
        consumption,
    ));

    // 將這個 Stage 加入 JobGraph
    graph.stages.push(writer);

    // 核心動作：創建 ShuffleReadExec（讀取 shuffle 資料）
    Ok(Arc::new(ShuffleReadExec::new(
        stage,
        plan.schema(),
        partitioning,
    )))
}
```

### 🔸 範例：一個簡單的 SQL 如何轉成 JobGraph

假設有這樣的查詢：

```sql
SELECT dept, COUNT(*)
FROM employees
GROUP BY dept
```

DataFusion 的執行計畫：

```
AggregateExec (final aggregation)
  |
CoalescePartitionsExec (合併所有 partition)
  |
AggregateExec (partial aggregation)
  |
ParquetExec (讀取資料，4 partitions)
```

`JobGraph::try_new()` 處理後：

```
Stage 0:
  ShuffleWriteExec
    |
  AggregateExec (partial)
    |
  ParquetExec (4 partitions)

  → 會產生 4 個 Tasks (每個 partition 一個)

Stage 1:
  AggregateExec (final)
    |
  ShuffleReadExec (讀取 Stage 0 的輸出)

  → 會產生 1 個 Task (因為 CoalescePartitions)
```

---

## 流程 3：創建 Tasks

位置：`crates/sail-execution/src/driver/actor/handler.rs:266-315`

```rust
fn accept_job(
    &mut self,
    _ctx: &mut ActorContext<Self>,
    plan: Arc<dyn ExecutionPlan>,
) -> ExecutionResult<JobId> {
    // 1. 生成 Job ID
    let job_id = self.state.next_job_id()?;

    debug!(
        "job {} execution plan\n{}",
        job_id,
        DisplayableExecutionPlan::new(plan.as_ref()).indent(true)
    );

    // 2. 核心步驟：建構 JobGraph
    let graph = JobGraph::try_new(plan)?;
    debug!("job {job_id} job graph \n{graph}");

    // 3. 核心步驟：為每個 Stage 創建 Tasks
    let mut stages = vec![];
    for (s, stage) in graph.stages().iter().enumerate() {
        let last = s == graph.stages().len() - 1;  // 檢查是否是最終 Stage
        let mut tasks = vec![];

        // 為每個 partition 創建一個 Task
        for p in 0..stage.output_partitioning().partition_count() {
            let task_id = self.state.next_task_id()?;
            let attempt = 0;

            // 核心機制：最終 Stage 的 Task 需要一個 channel（用來返回結果給 Driver）
            let channel = if last {
                Some(format!("job-{job_id}/task-{task_id}/attempt-{attempt}").into())
            } else {
                None
            };

            // 創建 Task
            self.state.add_task(
                task_id,
                TaskDescriptor {
                    job_id,
                    stage: s,
                    partition: p,
                    attempt,
                    mode: TaskMode::Pipelined,
                    state: TaskState::Created,
                    messages: vec![],
                    channel,
                },
            );

            // 核心動作：加入佇列
            self.task_queue.push_back(task_id);
            tasks.push(task_id);
        }

        stages.push(JobStage {
            plan: Arc::clone(stage),
            tasks,
        })
    }

    // 4. 保存 Job
    let descriptor = JobDescriptor { stages };
    self.state.add_job(job_id, descriptor);

    Ok(job_id)
}
```

---

## 流程 4：調度 Tasks

### 🔸 schedule_tasks 主迴圈

位置：`crates/sail-execution/src/driver/actor/handler.rs:526-556`

```rust
fn schedule_tasks(&mut self, ctx: &mut ActorContext<Self>) {
    // 1. 找出所有可用的 Worker slots
    let slots = self.find_idle_task_slots();
    let mut assigner = TaskSlotAssigner::new(slots);

    let mut skipped_tasks = vec![];

    // 2. 核心迴圈：處理 task_queue
    while let Some(task_id) = self.task_queue.pop_front() {
        // 3. 檢查 Task 是否可以調度（前一個 Stage 的所有 Tasks 是否都在運行）
        if !self.state.can_schedule_task(task_id) {
            skipped_tasks.push(task_id);
            continue;
        }

        // 4. 準備 Pending Task（狀態從 Created 變成 Pending）
        match self.prepare_pending_task(ctx, task_id) {
            Ok(()) => {}
            Err(e) => {
                warn!("failed to prepare pending task {task_id}: {e}");
                continue;
            }
        };

        // 5. 核心步驟：找到可用的 Worker
        let Some(worker_id) = assigner.next() else {
            skipped_tasks.push(task_id);
            continue;  // 沒有可用的 Worker，將 Task 放回佇列
        };

        // 6. 核心步驟：將 Task 分配給 Worker
        match self.schedule_task(ctx, task_id, worker_id) {
            Ok(()) => {}
            Err(e) => {
                warn!("failed to schedule task {task_id} to worker {worker_id}: {e}");
            }
        };
    }

    // 7. 將跳過的 Tasks 放回佇列
    self.task_queue.extend(skipped_tasks);
}
```

### 🔸 can_schedule_task：依賴檢查

位置：`crates/sail-execution/src/driver/state.rs:104-121`

```rust
pub fn can_schedule_task(&self, task_id: TaskId) -> bool {
    let Some(task) = self.tasks.get(&task_id) else {
        return false;
    };
    let Some(job) = self.jobs.get(&task.job_id) else {
        return false;
    };

    // 核心邏輯：檢查前面所有 Stages 的 Tasks 是否都在運行
    job.stages.iter().take(task.stage).all(|stage| {
        stage.tasks.iter().all(|&task_id| {
            self.tasks.get(&task_id).is_some_and(|task| {
                matches!(
                    task.state,
                    TaskState::Running { .. } | TaskState::Succeeded { .. }
                )
            })
        })
    })
}
```

### 🔸 schedule_task：發送 RPC 給 Worker

位置：`crates/sail-execution/src/driver/actor/handler.rs:577-629`

```rust
fn schedule_task(
    &mut self,
    ctx: &mut ActorContext<Self>,
    task_id: TaskId,
    worker_id: WorkerId,
) -> ExecutionResult<()> {
    let Some(task) = self.state.get_task(task_id) else {
        return Err(ExecutionError::InternalError(format!("task {task_id} not found")));
    };

    let job_id = task.job_id;
    let stage = task.stage;
    let partition = task.partition;
    let attempt = task.attempt;
    let channel = task.channel.clone();

    let Some(job) = self.state.get_job(job_id) else {
        return Err(ExecutionError::InternalError(format!("job {job_id} not found")));
    };

    let Some(job_stage) = job.stages.get(stage) else {
        return Err(ExecutionError::InternalError(format!(
            "stage {stage} not found in job {job_id}"
        )));
    };

    // 核心步驟：序列化執行計畫
    let plan = serialize_execution_plan(&job_stage.plan, self.physical_plan_codec.as_ref())?;

    // 核心步驟：透過 gRPC 發送 RunTask 請求給 Worker
    let mut client = self.worker_client(worker_id)?;
    let request = gen::RunTaskRequest {
        job_id: job_id.into(),
        stage: stage as u32,
        partition: partition as u32,
        attempt: attempt as u32,
        plan,
        channel: channel.map(|x| x.to_string()),
    };

    ctx.spawn(async move {
        if let Err(e) = client.run_task(request).await {
            error!("failed to run task {task_id} on worker {worker_id}: {e}");
        }
    });

    // 核心步驟：更新 Task 狀態從 Pending 變成 Scheduled
    self.state.update_task(
        task_id,
        attempt,
        TaskState::Scheduled { worker_id },
        None,
    );

    // 將 Task 綁定到 Worker
    self.state.attach_task_to_worker(task_id);

    info!("task {task_id} is scheduled to worker {worker_id}");

    Ok(())
}
```

---

## 流程 5：Worker 管理

### 🔸 Worker 註冊

位置：`crates/sail-execution/src/driver/actor/handler.rs:58-104`

```rust
pub(super) fn handle_register_worker(
    &mut self,
    ctx: &mut ActorContext<Self>,
    worker_id: WorkerId,
    host: String,
    port: u16,
    result: oneshot::Sender<ExecutionResult<()>>,
) -> ActorAction {
    info!("worker {worker_id} is available at {host}:{port}");

    let out = if let Some(worker) = self.state.get_worker(worker_id) {
        match worker.state {
            WorkerState::Pending => {
                // 核心步驟：Worker 從 Pending 變成 Running
                self.state.update_worker(
                    worker_id,
                    WorkerState::Running {
                        host,
                        port,
                        tasks: Default::default(),
                        jobs: Default::default(),
                        updated_at: Instant::now(),
                        heartbeat_at: Instant::now(),
                    },
                    None,
                );

                // 核心步驟：啟動監控機制
                self.schedule_lost_worker_probe(ctx, worker_id);
                self.schedule_idle_worker_probe(ctx, worker_id);

                // 核心步驟：開始調度 Tasks
                self.schedule_tasks(ctx);

                Ok(())
            }
            WorkerState::Running { .. } => {
                Err(ExecutionError::InternalError(format!(
                    "worker {worker_id} is already running"
                )))
            }
            WorkerState::Stopped => {
                Err(ExecutionError::InternalError(format!(
                    "worker {worker_id} is stopped"
                )))
            }
            WorkerState::Failed => {
                Err(ExecutionError::InternalError(format!(
                    "worker {worker_id} is failed"
                )))
            }
        }
    } else {
        Err(ExecutionError::InvalidArgument(format!(
            "worker {worker_id} not found"
        )))
    };

    let _ = result.send(out);
    ActorAction::Continue
}
```

### 🔸 Worker 心跳

位置：`crates/sail-execution/src/driver/actor/handler.rs:106-111`

```rust
pub(super) fn handle_worker_heartbeat(
    &mut self,
    _ctx: &mut ActorContext<Self>,
    worker_id: WorkerId,
) -> ActorAction {
    // 核心步驟：更新心跳時間
    self.state.record_worker_heartbeat(worker_id);
    ActorAction::Continue
}
```

位置：`crates/sail-execution/src/driver/state.rs:78-86`

```rust
pub fn record_worker_heartbeat(&mut self, worker_id: WorkerId) {
    let Some(worker) = self.workers.get_mut(&worker_id) else {
        warn!("worker {worker_id} not found");
        return;
    };

    if let WorkerState::Running { heartbeat_at, .. } = &mut worker.state {
        *heartbeat_at = Instant::now();  // 核心步驟：更新心跳時間
    }
}
```

### 🔸 Worker 失聯監控

DriverActor 會定期檢查 Worker 是否失聯：

```rust
// 註冊 Worker 後啟動監控機制
self.schedule_lost_worker_probe(ctx, worker_id);

// 監控邏輯
pub(super) fn handle_probe_lost_worker(
    &mut self,
    ctx: &mut ActorContext<Self>,
    worker_id: WorkerId,
    instant: Instant,
) -> ActorAction {
    let Some(worker) = self.state.get_worker(worker_id) else {
        return ActorAction::Continue;
    };

    if let WorkerState::Running { heartbeat_at, tasks, .. } = &worker.state {
        // 核心邏輯：檢查心跳是否過期
        if *heartbeat_at <= instant {
            warn!("worker {worker_id} lost");

            // 將 Worker 標記為 Failed
            self.state.update_worker(worker_id, WorkerState::Failed, Some("lost".to_string()));

            // 核心步驟：重新調度這個 Worker 上的所有 Tasks
            for &task_id in tasks {
                self.reschedule_task(ctx, task_id);
            }
        } else {
            // 繼續監控
            self.schedule_lost_worker_probe(ctx, worker_id);
        }
    }

    ActorAction::Continue
}
```

---

## 流程 6：Task 狀態更新

### 🔸 Worker 回報 Task 狀態

Worker 執行 Task 時會透過 gRPC 回報狀態更新：

```
Worker.RunTask() 開始執行
  |
Worker 呼叫 Driver.UpdateTask(Running)
  |
DriverActor.handle_update_task()
  |
更新 Task 狀態從 Scheduled 變成 Running
  |
Worker 執行完成
  |
Worker 呼叫 Driver.UpdateTask(Succeeded)
  |
DriverActor.handle_update_task()
  |
更新 Task 狀態從 Running 變成 Succeeded
  |
檢查 Job 是否完成
```

位置：`crates/sail-execution/src/driver/actor/handler.rs:216-240`

```rust
pub(super) fn handle_update_task(
    &mut self,
    ctx: &mut ActorContext<Self>,
    task_id: TaskId,
    attempt: usize,
    status: TaskStatus,
    message: Option<String>,
    cause: Option<CommonErrorCause>,
    sequence: Option<u64>,
) -> ActorAction {
    // 1. 核心機制：檢查序列號防止過時更新（防止亂序）
    if let Some(sequence) = sequence {
        if self.task_sequences.get(&task_id).is_some_and(|s| sequence <= *s) {
            warn!("task {task_id} sequence {sequence} is stale");
            return ActorAction::Continue;
        }
        self.task_sequences.insert(task_id, sequence);
    }

    // 2. 核心步驟：更新 Task 狀態
    self.update_task(ctx, task_id, attempt, status, message, cause);

    ActorAction::Continue
}
```

---

## 流程 7：返回結果

對於最終 Stage 的 Tasks，Driver 需要收集它們的執行結果並返回給呼叫者。

### 🔸 JobOutput

```rust
enum JobOutput {
    // 初始狀態，等待執行
    Pending {
        result: oneshot::Sender<ExecutionResult<SendableRecordBatchStream>>,
    },

    // 正在收集結果
    Streaming {
        // ... stream reader
    },

    // 完成
    Done,
}
```

當最終 Stage 的第一個 Task 開始運行時，Driver 會：

1. 從 Worker 的 shuffle storage 讀取結果
2. 創建一個 `SendableRecordBatchStream`
3. 透過 `oneshot::Sender` 返回給 `ClusterJobRunner`
4. `ClusterJobRunner` 將結果返回給 `SparkConnectService`
5. `SparkConnectService` 透過 gRPC 串流返回給客戶端

---

## Actor 模型在 Sail 中的應用

### 🔸 什麼是 Actor 模型

Actor 模型是一種並行計算模型，核心概念是**訊息傳遞**：

```
Actor 1 → Message → Actor 2
```

**優點**：
1. **避免共享狀態**：Actor 內部的狀態是私有的
2. **訊息驅動**：透過訊息通訊
3. **容錯**：訊息可以重試，Actor 可以重啟

### 🔸 DriverActor 的 Actor 實作

位置：`crates/sail-execution/src/driver/actor/core.rs:35-121`

```rust
#[tonic::async_trait]
impl Actor for DriverActor {
    type Message = DriverEvent;  // 核心定義：訊息類型
    type Options = DriverOptions;

    fn new(options: DriverOptions) -> Self {
        // 創建 Actor
        // ...
    }

    async fn start(&mut self, ctx: &mut ActorContext<Self>) {
        // 核心啟動：啟動 gRPC Server（接收 Worker 的註冊和狀態回報）
        let addr = (
            self.options().driver_listen_host.clone(),
            self.options().driver_listen_port,
        );
        let server = mem::take(&mut self.server);
        self.server = server.start(Self::serve(ctx.handle().clone(), addr)).await;
    }

    // 核心邏輯：處理訊息
    fn receive(&mut self, ctx: &mut ActorContext<Self>, message: DriverEvent) -> ActorAction {
        match message {
            DriverEvent::ServerReady { port, signal } => {
                self.handle_server_ready(ctx, port, signal)
            }
            DriverEvent::RegisterWorker { worker_id, host, port, result } => {
                self.handle_register_worker(ctx, worker_id, host, port, result)
            }
            DriverEvent::WorkerHeartbeat { worker_id } => {
                self.handle_worker_heartbeat(ctx, worker_id)
            }
            DriverEvent::ExecuteJob { plan, result } => {
                self.handle_execute_job(ctx, plan, result)
            }
            DriverEvent::UpdateTask { task_id, attempt, status, message, cause, sequence } => {
                self.handle_update_task(ctx, task_id, attempt, status, message, cause, sequence)
            }
            // ... 其他訊息
            DriverEvent::Shutdown => ActorAction::Stop,
        }
    }

    async fn stop(mut self, ctx: &mut ActorContext<Self>) {
        // 核心清理：停止所有 Workers
        self.stop_all_workers(ctx);

        // 停止 gRPC Server
        self.server.stop().await;

        // 停止 Worker Manager
        self.worker_manager.stop().await;
    }
}
```

### 🔸 DriverEvent：所有訊息類型

位置：`crates/sail-execution/src/driver/event.rs:13-62`

```rust
pub enum DriverEvent {
    // gRPC Server 啟動完成
    ServerReady {
        port: u16,
        signal: oneshot::Sender<()>,
    },

    // Worker 註冊
    RegisterWorker {
        worker_id: WorkerId,
        host: String,
        port: u16,
        result: oneshot::Sender<ExecutionResult<()>>,
    },

    // Worker 心跳
    WorkerHeartbeat {
        worker_id: WorkerId,
    },

    // 監控 Worker 失聯
    ProbeLostWorker {
        worker_id: WorkerId,
        instant: Instant,
    },

    // 核心訊息：執行任務
    ExecuteJob {
        plan: Arc<dyn ExecutionPlan>,
        result: oneshot::Sender<ExecutionResult<SendableRecordBatchStream>>,
    },

    // 清理 Job
    CleanUpJob {
        job_id: JobId,
    },

    // 核心訊息：Task 狀態更新（來自 Worker 的回報）
    UpdateTask {
        task_id: TaskId,
        attempt: usize,
        status: TaskStatus,
        message: Option<String>,
        cause: Option<CommonErrorCause>,
        sequence: Option<u64>,
    },

    // 關閉 Driver
    Shutdown,
}
```

---

## 完整生命週期

```
1. SessionManager 創建 ClusterJobRunner
   |
2. ClusterJobRunner 啟動 DriverActor
   |
3. DriverActor.start()
   - 啟動 gRPC Server
   - WorkerManager 啟動 Workers
   |
4. Workers 向 DriverActor 註冊
   - DriverEvent::RegisterWorker
   - Worker 狀態從 Pending 變成 Running
   |
5. ClusterJobRunner 發送任務
   - DriverEvent::ExecuteJob
   - 建構 JobGraph
   - 創建 Tasks
   - 調度 Tasks
   |
6. DriverActor 向 Workers 發送 RunTask RPC
   - Worker 開始執行
   |
7. Worker 回報狀態
   - DriverEvent::UpdateTask(Running)
   - DriverEvent::UpdateTask(Succeeded)
   |
8. DriverActor 返回結果
   - 收集最終 Stage 的結果
   - 返回給 ClusterJobRunner
   |
9. ClusterJobRunner 返回給 SparkConnectService
   |
10. SparkConnectService 返回給 PySpark Client
```

---

## 總結：DriverActor 的核心設計

### 🔸 核心優點

1. **Actor 模型**：透過訊息傳遞避免共享狀態，避免競態條件
2. **非阻塞並行**：所有 I/O 操作都是異步的（gRPC、任務調度）
3. **容錯機制**：Worker 失聯檢測、Task 重試機制
4. **階段依賴**：Stage 之間有依賴關係，前一個 Stage 必須執行完才能執行下一個
5. **流式結果**：結果透過 Stream 返回，不需要一次性載入記憶體

### 🔸 關鍵資料結構

| 結構 | 用途 |
|------|------|
| `JobGraph` | 將 ExecutionPlan 切分成 Stages |
| `DriverState` | 追蹤所有 Workers、Jobs、Tasks |
| `task_queue` | 等待調度的 Tasks |
| `job_outputs` | 收集結果並返回給呼叫者 |
| `worker_clients` | 向 Workers 發送 RPC |

### 🔸 狀態轉換

**Task 狀態轉換**
```
Created → Pending → Scheduled → Running → Succeeded
                                       → Failed (重試)
```

**Worker 狀態轉換**
```
Pending → Running → Stopped
                 → Failed
```

---

希望這份文件能幫助你理解 DriverActor 如何處理分散式執行！
