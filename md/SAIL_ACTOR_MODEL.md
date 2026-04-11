# Sail 的 Actor 模型：輕量級並發架構詳解

## 什麼是 Actor 模型？

Actor 模型是一種並發計算的數學模型，最早由 Carl Hewitt 在 1973 年提出。在 Actor 模型中，"Actor" 是並發計算的基本單位，每個 Actor：

1. 擁有自己的私有狀態（其他 Actor 無法直接存取）
2. 透過訊息傳遞與其他 Actor 通訊
3. 順序處理訊息（一次處理一個訊息）
4. 可以創建新的 Actor
5. 可以發送訊息給其他 Actor

這種模型避免了傳統多執行緒程式設計中的共享記憶體和鎖競爭問題。

## 為什麼 Sail 需要 Actor 模型？

Sail 是一個分散式查詢引擎，面臨以下挑戰：

🔸 **並發管理**
- Spark Connect 伺服器需要管理多個客戶端連線
- 每個連線可能有獨立的 Session 和執行上下文
- 需要協調 Driver 和多個 Worker 之間的通訊

🔸 **狀態隔離**
- Session 之間的狀態必須隔離
- Worker 和 Driver 的狀態需要獨立管理
- 避免資料競爭和死鎖

🔸 **訊息驅動**
- 分散式系統本質上是訊息驅動的
- RPC 調用、任務調度、狀態更新都是訊息
- Actor 模型天然適合這種場景

傳統方法（共享記憶體 + 鎖）的問題：
```rust
// 傳統方法：需要手動管理鎖
struct SessionManager {
    sessions: Arc<Mutex<HashMap<SessionKey, SessionContext>>>,
}

impl SessionManager {
    fn get_or_create_session(&self, key: SessionKey) -> SessionContext {
        let mut sessions = self.sessions.lock().unwrap(); // 需要持鎖
        sessions.entry(key).or_insert_with(|| {
            // 如果這裡需要呼叫其他持鎖的操作，容易死鎖
            create_session()
        }).clone()
    }
}
```

Actor 方法（訊息傳遞）：
```rust
// Actor 方法：透過訊息傳遞，無需手動管理鎖
impl SessionManager {
    async fn get_or_create_session(&self, key: SessionKey) -> SessionContext {
        let (tx, rx) = oneshot::channel();
        self.handle.send(SessionManagerEvent::GetOrCreateSession {
            key,
            result: tx,
        }).await?;
        rx.await? // 等待 Actor 處理並回覆
    }
}
```

## Sail Actor 架構總覽

🔸 位置：`crates/sail-server/src/actor.rs`

Sail 實作了一個輕量級的 Actor 系統，核心組件包括：

```
┌─────────────────────────────────────────────────────────────┐
│                        ActorSystem                          │
│  ┌───────────────────────────────────────────────────────┐  │
│  │              JoinSet<ActorRunner>                     │  │
│  │  ┌─────────────┐  ┌─────────────┐  ┌─────────────┐    │  │
│  │  │ Runner 1    │  │ Runner 2    │  │ Runner 3    │    │  │
│  │  └─────────────┘  └─────────────┘  └─────────────┘    │  │
│  └───────────────────────────────────────────────────────┘  │
└─────────────────────────────────────────────────────────────┘
         ↑                  ↑                  ↑
         │                  │                  │
    ActorHandle        ActorHandle        ActorHandle
         │                  │                  │
         └──────────┬───────┴────────┬─────────┘
                    │                │
               send(message)    send(message)
```

### 核心組件

| 組件 | 職責 | 特性 |
|------|------|------|
| `Actor` trait | 定義 Actor 行為 | 實作者需定義訊息處理邏輯 |
| `ActorSystem` | 管理所有 Actor | 生成和追蹤 Actor 生命週期 |
| `ActorHandle` | Actor 的引用 | 可跨執行緒傳遞，用於發送訊息 |
| `ActorContext` | Actor 執行上下文 | 提供生成子任務、延遲發送等功能 |
| `ActorRunner` | Actor 事件迴圈 | 接收訊息並調用 Actor 處理 |

## Actor Trait 定義

```rust
#[tonic::async_trait]
pub trait Actor: Sized + Send + 'static {
    type Message: Send + 'static;
    type Options;

    fn new(options: Self::Options) -> Self;

    async fn start(&mut self, ctx: &mut ActorContext<Self>) {}

    fn receive(&mut self, ctx: &mut ActorContext<Self>, message: Self::Message) -> ActorAction;

    async fn stop(self, ctx: &mut ActorContext<Self>) {}
}
```

🔸 **關聯型別**

- `Message`：Actor 可以接收的訊息型別
  - 必須實作 `Send + 'static`，確保可以跨執行緒傳遞
  - 通常是一個 `enum`，每個變體代表一種訊息
- `Options`：Actor 初始化時的配置
  - 可以是任何型別，不需要額外約束
  - 用於傳遞初始狀態、設定等

🔸 **生命週期鉤子**

1. `fn new(options: Self::Options) -> Self`
   - Actor 的建構函數
   - 在 `ActorSystem::spawn` 中被呼叫
   - 應該快速完成，不應執行異步操作

2. `async fn start(&mut self, ctx: &mut ActorContext<Self>)`
   - Actor 啟動時呼叫（在處理第一個訊息之前）
   - 可以執行異步初始化（如啟動 gRPC 伺服器）
   - 預設實作為空

3. `fn receive(&mut self, ctx: &mut ActorContext<Self>, message: Self::Message) -> ActorAction`
   - **核心方法**：處理訊息
   - 順序處理訊息（一次處理一個）
   - 回傳 `ActorAction` 決定下一步動作
   - **不應阻塞**：如果需要執行長時間操作，應使用 `ctx.spawn`

4. `async fn stop(self, ctx: &mut ActorContext<Self>)`
   - Actor 停止時呼叫（在事件迴圈結束後）
   - 可以執行清理操作
   - 預設實作為空

🔸 **ActorAction 回傳值**

```rust
pub enum ActorAction {
    Continue,         // 繼續處理下一個訊息
    Warn(String),     // 記錄警告並繼續
    Fail(String),     // 記錄錯誤並停止 Actor
    Stop,             // 正常停止 Actor
}
```

## ActorSystem：Actor 的容器

```rust
pub struct ActorSystem {
    tasks: JoinSet<()>,
}

impl ActorSystem {
    pub fn new() -> Self {
        Self {
            tasks: JoinSet::new(),
        }
    }

    pub fn spawn<T: Actor>(&mut self, options: T::Options) -> ActorHandle<T> {
        let (tx, rx) = mpsc::channel(ACTOR_CHANNEL_SIZE);
        let handle = ActorHandle { sender: tx };
        let runner = ActorRunner {
            actor: T::new(options),
            ctx: ActorContext::new(&handle),
            receiver: rx,
        };
        self.tasks.spawn(runner.run());
        handle
    }

    pub async fn join(&mut self) {
        while let Some(result) = self.tasks.join_next().await {
            match result {
                Ok(()) => {}
                Err(e) => {
                    error!("failed to join task spawned by actor system: {e}");
                }
            }
        }
    }
}
```

### spawn 方法的執行流程

```
1. 建立 mpsc channel (buffer size = 8)
   ├─ tx: Sender<T::Message> (可 clone 多個)
   └─ rx: Receiver<T::Message> (只有一個)

2. 建立 ActorHandle
   └─ 包裝 Sender，用於發送訊息

3. 建立 ActorRunner
   ├─ actor: T::new(options) - 建立 Actor 實例
   ├─ ctx: ActorContext::new(&handle) - 建立上下文
   └─ receiver: rx - 持有 Receiver

4. 在 tokio 執行緒池中生成事件迴圈
   └─ self.tasks.spawn(runner.run())

5. 回傳 ActorHandle
   └─ 呼叫者可用 handle 發送訊息
```

Rust 語法解說：
- `mpsc::channel(ACTOR_CHANNEL_SIZE)`：多生產者單消費者通道
  - `ACTOR_CHANNEL_SIZE = 8`：緩衝區大小
  - 當緩衝區滿時，發送端會阻塞
- `JoinSet<()>`：追蹤一組異步任務
  - `spawn` 方法添加新任務
  - `join_next` 方法等待任一任務完成
  - 當 `JoinSet` 被 drop 時，所有任務會被中止

## ActorHandle：Actor 的引用

```rust
pub struct ActorHandle<T: Actor> {
    sender: mpsc::Sender<T::Message>,
}

impl<T: Actor> ActorHandle<T> {
    pub async fn send(
        &self,
        message: T::Message,
    ) -> Result<(), mpsc::error::SendError<T::Message>> {
        self.sender.send(message).await
    }
}
```

🔸 **特性**

1. **可 Clone**：可以創建多個 handle 指向同一個 Actor
2. **跨執行緒傳遞**：可以在不同執行緒間傳遞
3. **型別安全**：只能發送正確型別的訊息
4. **異步發送**：`send` 方法是 async，會在緩衝區滿時等待

🔸 **使用範例**

```rust
// 建立 Actor
let mut system = ActorSystem::new();
let handle = system.spawn::<MyActor>(options);

// Clone handle 到另一個執行緒
let handle2 = handle.clone();
tokio::spawn(async move {
    handle2.send(MyMessage::Foo).await.unwrap();
});

// 在主執行緒發送訊息
handle.send(MyMessage::Bar).await.unwrap();
```

## ActorContext：Actor 的工具箱

```rust
pub struct ActorContext<T: Actor> {
    handle: ActorHandle<T>,
    tasks: JoinSet<()>,
}

impl<T: Actor> ActorContext<T> {
    pub fn send(&mut self, message: T::Message) {
        let handle = self.handle.clone();
        self.spawn(async move {
            let _ = handle.send(message).await;
        });
    }

    pub fn send_with_delay(&mut self, message: T::Message, delay: Duration) {
        let handle = self.handle.clone();
        self.spawn(async move {
            tokio::time::sleep(delay).await;
            let _ = handle.send(message).await;
        });
    }

    pub fn spawn(
        &mut self,
        task: impl Future<Output = ()> + Send + 'static,
    ) -> AbortHandle {
        self.tasks.spawn(task)
    }

    pub fn reap(&mut self) {
        while let Some(result) = self.tasks.try_join_next() {
            match result {
                Ok(()) => {}
                Err(e) => {
                    error!("failed to join task spawned by actor: {e}");
                }
            }
        }
    }
}
```

🔸 **主要方法**

1. **send**：發送訊息給自己
   - 用於在處理訊息後觸發下一個動作
   - 不會阻塞，訊息進入佇列

2. **send_with_delay**：延遲發送訊息
   - 用於實作定時器、超時檢測
   - 例如：Session 閒置超時檢測

3. **spawn**：生成子任務
   - 用於執行長時間操作（如 I/O、RPC 調用）
   - 任務在背景執行，不阻塞 Actor 事件迴圈
   - 回傳 `AbortHandle`，可用於取消任務

4. **reap**：清理已完成的子任務
   - 在每次處理訊息後自動呼叫
   - 記錄任何錯誤

🔸 **為什麼需要 spawn？**

Actor 的 `receive` 方法是同步的（不是 async fn），這是刻意的設計：

```rust
// ❌ 錯誤：阻塞事件迴圈
fn receive(&mut self, ctx: &mut ActorContext<Self>, message: Self::Message) -> ActorAction {
    match message {
        MyMessage::ProcessData(data) => {
            // 這會阻塞事件迴圈，其他訊息無法處理
            std::thread::sleep(Duration::from_secs(10));
            ActorAction::Continue
        }
    }
}

// ✅ 正確：使用 spawn 在背景處理
fn receive(&mut self, ctx: &mut ActorContext<Self>, message: Self::Message) -> ActorAction {
    match message {
        MyMessage::ProcessData(data) => {
            ctx.spawn(async move {
                // 這在背景執行，不阻塞事件迴圈
                tokio::time::sleep(Duration::from_secs(10)).await;
                process_data(data).await;
            });
            ActorAction::Continue
        }
    }
}
```

## ActorRunner：事件迴圈

```rust
struct ActorRunner<T: Actor> {
    actor: T,
    ctx: ActorContext<T>,
    receiver: mpsc::Receiver<T::Message>,
}

impl<T: Actor> ActorRunner<T> {
    async fn run(mut self) {
        // 1. 啟動鉤子
        self.actor.start(&mut self.ctx).await;

        // 2. 事件迴圈
        while let Some(message) = self.receiver.recv().await {
            let action = self.actor.receive(&mut self.ctx, message);
            match action {
                ActorAction::Continue => {}
                ActorAction::Warn(message) => {
                    log::warn!("{message}");
                }
                ActorAction::Fail(message) => {
                    log::error!("{message}");
                    break;
                }
                ActorAction::Stop => {
                    break;
                }
            }
            self.ctx.reap();
        }

        // 3. 停止鉤子
        self.actor.stop(&mut self.ctx).await;
    }
}
```

🔸 **事件迴圈的執行流程**

```
┌──────────────────────────────────────────────┐
│ 1. actor.start(&mut ctx).await               │
│    - 執行異步初始化                          │
│    - 例如：啟動 gRPC 伺服器                  │
└──────────────────────────────────────────────┘
                    ↓
┌──────────────────────────────────────────────┐
│ 2. while let Some(message) = rx.recv().await │
│    ┌────────────────────────────────────┐   │
│    │ 2.1 actor.receive(ctx, message)    │   │
│    │     - 處理訊息                     │   │
│    │     - 回傳 ActorAction             │   │
│    └────────────────────────────────────┘   │
│                    ↓                          │
│    ┌────────────────────────────────────┐   │
│    │ 2.2 match action                   │   │
│    │     - Continue: 繼續迴圈           │   │
│    │     - Warn: 記錄警告後繼續         │   │
│    │     - Fail/Stop: 跳出迴圈          │   │
│    └────────────────────────────────────┘   │
│                    ↓                          │
│    ┌────────────────────────────────────┐   │
│    │ 2.3 ctx.reap()                     │   │
│    │     - 清理已完成的子任務           │   │
│    └────────────────────────────────────┘   │
└──────────────────────────────────────────────┘
                    ↓
┌──────────────────────────────────────────────┐
│ 3. actor.stop(ctx).await                     │
│    - 執行清理操作                            │
│    - 例如：關閉連線、釋放資源               │
└──────────────────────────────────────────────┘
```

## Sail 中的 Actor 實例

Sail 中有三個主要的 Actor 實作：

| Actor | 位置 | 職責 |
|-------|------|------|
| `SessionManagerActor` | sail-spark-connect | 管理 Spark 會話，處理會話建立/閒置檢測 |
| `WorkerActor` | sail-execution | 執行查詢任務，與 Driver 通訊 |
| `DriverActor` | sail-execution | 調度任務到 Worker，管理分散式執行 |

### 實例 1：SessionManagerActor

🔸 位置：`crates/sail-spark-connect/src/session_manager.rs:319-407`

```rust
// 訊息定義
enum SessionManagerEvent {
    GetOrCreateSession {
        key: SessionKey,
        system: Arc<Mutex<ActorSystem>>,
        result: oneshot::Sender<SparkResult<SessionContext>>,
    },
    ProbeIdleSession {
        key: SessionKey,
        instant: Instant,
    },
}

// Actor 實作
#[tonic::async_trait]
impl Actor for SessionManagerActor {
    type Message = SessionManagerEvent;
    type Options = SessionManagerOptions;

    fn new(options: Self::Options) -> Self {
        Self {
            options,
            sessions: HashMap::new(),
            global_file_listing_cache: None,
            global_file_statistics_cache: None,
            global_file_metadata_cache: None,
        }
    }

    fn receive(&mut self, ctx: &mut ActorContext<Self>, message: Self::Message) -> ActorAction {
        match message {
            SessionManagerEvent::GetOrCreateSession {
                key,
                system,
                result,
            } => self.handle_get_or_create_session(ctx, key, system, result),
            SessionManagerEvent::ProbeIdleSession { key, instant } => {
                self.handle_probe_idle_session(ctx, key, instant)
            }
        }
    }
}
```

🔸 **訊息處理範例**

```rust
fn handle_get_or_create_session(
    &mut self,
    ctx: &mut ActorContext<Self>,
    key: SessionKey,
    system: Arc<Mutex<ActorSystem>>,
    result: oneshot::Sender<SparkResult<SessionContext>>,
) -> ActorAction {
    // 1. 檢查是否已存在
    let context = if let Some(context) = self.sessions.get(&key) {
        Ok(context.clone())
    } else {
        // 2. 建立新會話
        info!("creating session {key}");
        match self.create_session_context(system, key.clone()) {
            Ok(context) => {
                self.sessions.insert(key, context.clone());
                Ok(context)
            }
            Err(e) => Err(e),
        }
    };

    // 3. 設定閒置超時檢測
    if let Ok(context) = &context {
        if let Ok(active_at) = context
            .extension::<SparkSession>()
            .and_then(|spark| spark.track_activity())
        {
            // 延遲發送 ProbeIdleSession 訊息
            ctx.send_with_delay(
                SessionManagerEvent::ProbeIdleSession {
                    key,
                    instant: active_at,
                },
                Duration::from_secs(self.options.config.spark.session_timeout_secs),
            );
        }
    }

    // 4. 回覆結果
    let _ = result.send(context);
    ActorAction::Continue
}
```

這個範例展示了 Actor 模型的幾個關鍵特性：

1. **狀態封裝**：`self.sessions` 只能被 Actor 內部存取
2. **訊息驅動**：透過 `oneshot::Sender` 回覆呼叫者
3. **延遲訊息**：使用 `send_with_delay` 實作超時檢測
4. **順序處理**：每次只處理一個訊息，避免資料競爭

### 實例 2：WorkerActor

🔸 位置：`crates/sail-execution/src/worker/actor/core.rs:38-99`

```rust
// 訊息定義（簡化版）
pub enum WorkerEvent {
    ServerReady { port: u16, signal: oneshot::Sender<()> },
    StartHeartbeat,
    RunTask { task_id: TaskId, attempt: u32, plan: Vec<u8>, ... },
    StopTask { task_id: TaskId, attempt: u32 },
    ReportTaskStatus { task_id: TaskId, status: TaskStatus, ... },
    // ... 更多訊息
}

// Actor 實作
#[tonic::async_trait]
impl Actor for WorkerActor {
    type Message = WorkerEvent;
    type Options = WorkerOptions;

    fn new(options: WorkerOptions) -> Self {
        let driver_client = DriverClient::new(ClientOptions {
            enable_tls: options.enable_tls,
            host: options.driver_host.clone(),
            port: options.driver_port,
        });
        Self {
            options,
            server: ServerMonitor::new(),
            driver_client,
            worker_clients: HashMap::new(),
            task_signals: HashMap::new(),
            local_streams: HashMap::new(),
            session_context: None,
            physical_plan_codec: Box::new(RemoteExecutionCodec::new(...)),
            sequence: 42,
        }
    }

    async fn start(&mut self, ctx: &mut ActorContext<Self>) {
        // 啟動 gRPC 伺服器
        let addr = (
            self.options.worker_listen_host.clone(),
            self.options.worker_listen_port,
        );
        let server = mem::take(&mut self.server);
        self.server = server.start(Self::serve(ctx.handle().clone(), addr)).await;
    }

    fn receive(&mut self, ctx: &mut ActorContext<Self>, message: WorkerEvent) -> ActorAction {
        match message {
            WorkerEvent::ServerReady { port, signal } => {
                self.handle_server_ready(ctx, port, signal)
            }
            WorkerEvent::StartHeartbeat => self.handle_start_heartbeat(ctx),
            WorkerEvent::RunTask { task_id, attempt, plan, partition, channel } => {
                self.handle_run_task(ctx, task_id, attempt, plan, partition, channel)
            }
            WorkerEvent::StopTask { task_id, attempt } => {
                self.handle_stop_task(ctx, task_id, attempt)
            }
            // ... 處理其他訊息
        }
    }
}
```

🔸 **RunTask 訊息處理**

```rust
fn handle_run_task(
    &mut self,
    ctx: &mut ActorContext<Self>,
    task_id: TaskId,
    attempt: u32,
    plan: Vec<u8>,
    partition: usize,
    channel: ChannelName,
) -> ActorAction {
    // 1. 解碼物理計劃
    let plan = match self.physical_plan_codec.decode(&plan) {
        Ok(plan) => plan,
        Err(e) => {
            error!("failed to decode physical plan: {e}");
            return ActorAction::Fail(format!("failed to decode plan: {e}"));
        }
    };

    // 2. 建立取消訊號
    let (cancel_tx, cancel_rx) = oneshot::channel();
    self.task_signals.insert(TaskAttempt { task_id, attempt }, cancel_tx);

    // 3. 在背景執行任務
    let handle = ctx.handle().clone();
    let session_context = self.session_context.clone().unwrap();
    ctx.spawn(async move {
        let result = execute_task(session_context, plan, partition, cancel_rx).await;

        // 4. 回報任務狀態給自己（透過訊息）
        let status = match result {
            Ok(_) => TaskStatus::Success,
            Err(e) => TaskStatus::Failed,
        };
        let _ = handle.send(WorkerEvent::ReportTaskStatus {
            task_id,
            attempt,
            status,
            message: result.err().map(|e| e.to_string()),
            cause: None,
        }).await;
    });

    ActorAction::Continue
}
```

這個範例展示了：

1. **背景執行**：使用 `ctx.spawn` 執行長時間任務
2. **自我訊息**：任務完成後發送 `ReportTaskStatus` 給自己
3. **取消支援**：透過 `oneshot::channel` 實作任務取消
4. **錯誤處理**：解碼失敗時回傳 `ActorAction::Fail`

### 實例 3：DriverActor

🔸 位置：`crates/sail-execution/src/driver/actor/core.rs:35-99`

```rust
// 訊息定義（簡化版）
pub enum DriverEvent {
    ServerReady { port: u16, signal: oneshot::Sender<()> },
    RegisterWorker { worker_id: WorkerId, host: String, port: u16, ... },
    WorkerHeartbeat { worker_id: WorkerId },
    ProbeIdleWorker { worker_id: WorkerId, instant: Instant },
    ExecuteJob { plan: Arc<dyn ExecutionPlan>, result: oneshot::Sender<...> },
    UpdateTask { task_id: TaskId, status: TaskStatus, ... },
    // ... 更多訊息
}

// Actor 實作
#[tonic::async_trait]
impl Actor for DriverActor {
    type Message = DriverEvent;
    type Options = DriverOptions;

    fn new(options: DriverOptions) -> Self {
        let worker_manager: Arc<dyn WorkerManager> = match &options.worker_manager {
            WorkerManagerOptions::Local => {
                Arc::new(LocalWorkerManager::new(options.runtime.clone()))
            }
            WorkerManagerOptions::Kubernetes(opts) => {
                Arc::new(KubernetesWorkerManager::new(opts.clone()))
            }
        };
        Self {
            options,
            state: DriverState::new(),
            server: ServerMonitor::new(),
            worker_manager,
            worker_clients: HashMap::new(),
            physical_plan_codec: Box::new(RemoteExecutionCodec::new(...)),
            task_queue: VecDeque::new(),
            task_sequences: HashMap::new(),
            job_outputs: HashMap::new(),
        }
    }

    fn receive(&mut self, ctx: &mut ActorContext<Self>, message: DriverEvent) -> ActorAction {
        match message {
            DriverEvent::RegisterWorker { worker_id, host, port, result } => {
                self.handle_register_worker(ctx, worker_id, host, port, result)
            }
            DriverEvent::ExecuteJob { plan, result } => {
                self.handle_execute_job(ctx, plan, result)
            }
            DriverEvent::UpdateTask { task_id, attempt, status, ... } => {
                self.handle_update_task(ctx, task_id, attempt, status, ...)
            }
            // ... 處理其他訊息
        }
    }
}
```

DriverActor 協調整個分散式執行流程：

1. **接收查詢計劃**：`ExecuteJob` 訊息
2. **分解為任務**：將計劃切分為多個 Task
3. **調度到 Worker**：透過 `WorkerClient` 發送 RPC
4. **追蹤任務狀態**：接收 `UpdateTask` 訊息
5. **回報結果**：透過 `oneshot::Sender` 回覆呼叫者

## Actor 之間的通訊模式

### 模式 1：請求-回應（Request-Reply）

使用 `oneshot::channel` 實作同步語意：

```rust
// 呼叫者
async fn get_session(&self, key: SessionKey) -> Result<SessionContext> {
    let (tx, rx) = oneshot::channel();
    self.handle.send(SessionManagerEvent::GetOrCreateSession {
        key,
        result: tx,
    }).await?;
    rx.await? // 等待回覆
}

// Actor 處理
fn receive(&mut self, ctx: &mut ActorContext<Self>, message: Self::Message) -> ActorAction {
    match message {
        SessionManagerEvent::GetOrCreateSession { key, result } => {
            let context = self.sessions.get(&key).cloned();
            let _ = result.send(context); // 發送回覆
            ActorAction::Continue
        }
    }
}
```

### 模式 2：觸發即忘（Fire-and-Forget）

不需要回應的訊息：

```rust
// 發送訊息，不等待回覆
handle.send(WorkerEvent::StartHeartbeat).await?;
```

### 模式 3：延遲訊息（Delayed Message）

用於實作定時器、超時檢測：

```rust
// 設定 60 秒後檢查 session 是否閒置
ctx.send_with_delay(
    SessionManagerEvent::ProbeIdleSession { key, instant },
    Duration::from_secs(60),
);
```

### 模式 4：Actor 間通訊

Actor 之間透過 RPC 或訊息通訊：

```
┌──────────────┐                    ┌──────────────┐
│ DriverActor  │                    │ WorkerActor  │
│              │                    │              │
│  handle_     │  ──RPC: RunTask──> │              │
│  execute_job │                    │  handle_     │
│              │                    │  run_task    │
│              │ <─RPC: TaskStatus─ │              │
│  handle_     │                    │  (完成後)    │
│  update_task │                    │              │
└──────────────┘                    └──────────────┘
```

Worker 和 Driver 透過 gRPC 通訊，但在各自內部使用 Actor 訊息處理：

```rust
// WorkerActor 收到 RPC 請求
impl WorkerService for WorkerServiceImpl {
    async fn run_task(&self, request: RunTaskRequest) -> Result<RunTaskResponse> {
        // 將 RPC 轉換為 Actor 訊息
        self.actor_handle.send(WorkerEvent::RunTask {
            task_id: request.task_id,
            plan: request.plan,
            // ...
        }).await?;
        Ok(RunTaskResponse {})
    }
}
```

## Actor 模型的優勢

🔸 **避免資料競爭**

傳統多執行緒程式設計：
```rust
// ❌ 需要手動管理鎖，容易死鎖
let sessions = Arc::new(Mutex::new(HashMap::new()));

// 執行緒 1
let mut sessions = sessions.lock().unwrap();
sessions.insert(key1, value1);
// 如果這裡呼叫其他持鎖的函數，可能死鎖

// 執行緒 2
let sessions = sessions.lock().unwrap(); // 等待執行緒 1 釋放鎖
```

Actor 模型：
```rust
// ✅ 沒有鎖，訊息順序處理
handle.send(SessionManagerEvent::Insert { key, value }).await?;
// 不會阻塞，訊息進入佇列
// Actor 會按順序處理
```

🔸 **錯誤隔離**

一個 Actor 崩潰不會影響其他 Actor：

```rust
// SessionManagerActor 崩潰
if something_wrong {
    return ActorAction::Fail("actor failed".to_string());
}
// Actor 停止，但 WorkerActor 和 DriverActor 繼續運行
```

🔸 **背壓（Backpressure）**

當 Actor 處理速度跟不上訊息發送速度時，channel 緩衝區會滿：

```rust
// 緩衝區滿時，send 會等待
handle.send(message).await?; // 阻塞直到有空間
```

這自然實作了背壓機制，防止記憶體耗盡。

🔸 **測試友善**

Actor 可以單獨測試：

```rust
#[tokio::test]
async fn test_session_manager() {
    let mut system = ActorSystem::new();
    let handle = system.spawn::<SessionManagerActor>(options);

    // 發送訊息
    let (tx, rx) = oneshot::channel();
    handle.send(SessionManagerEvent::GetOrCreateSession {
        key: SessionKey { user_id: "test", session_id: "123" },
        result: tx,
    }).await.unwrap();

    // 驗證回應
    let context = rx.await.unwrap();
    assert!(context.is_ok());
}
```

## Actor 模型的限制與注意事項

🔸 **順序處理限制**

Actor 一次只能處理一個訊息，如果處理邏輯太慢，會成為瓶頸：

```rust
// ❌ 錯誤：長時間操作阻塞事件迴圈
fn receive(&mut self, ctx: &mut ActorContext<Self>, message: Self::Message) -> ActorAction {
    expensive_computation(); // 這會阻塞其他訊息
    ActorAction::Continue
}

// ✅ 正確：使用 spawn 在背景處理
fn receive(&mut self, ctx: &mut ActorContext<Self>, message: Self::Message) -> ActorAction {
    ctx.spawn(async move {
        expensive_computation().await;
    });
    ActorAction::Continue
}
```

🔸 **記憶體洩漏風險**

如果訊息處理速度太慢，channel 緩衝區可能不斷累積：

```rust
// 如果 Actor 處理速度是 1 msg/s，但發送速度是 10 msg/s
// channel 會不斷累積訊息，最終記憶體耗盡

// 解決方法：
// 1. 增加 Actor 實例數量（水平擴展）
// 2. 增加緩衝區大小（垂直擴展）
// 3. 實作訊息丟棄策略
```

🔸 **訊息順序保證**

同一個發送者發送的訊息順序有保證，但不同發送者的訊息順序無保證：

```rust
// 發送者 A
handle.send(Message1).await;
handle.send(Message2).await;
// Message1 保證在 Message2 之前處理

// 發送者 A 和 B
// 發送者 A: handle.send(MessageA1).await;
// 發送者 B: handle.send(MessageB1).await;
// MessageA1 和 MessageB1 的順序無保證
```

## 實作建議

🔸 **訊息設計**

```rust
// ✅ 好的設計：使用 enum 清楚定義所有訊息
pub enum MyActorEvent {
    DoSomething { param: String },
    Stop,
}

// ❌ 不好的設計：使用 trait object
pub trait Message {}
// 無法用 match，不知道有哪些訊息型別
```

🔸 **錯誤處理**

```rust
// ✅ 在 Actor 內部處理錯誤
fn receive(&mut self, ctx: &mut ActorContext<Self>, message: Self::Message) -> ActorAction {
    match message {
        MyMessage::DoSomething { param } => {
            match self.do_something(param) {
                Ok(_) => ActorAction::Continue,
                Err(e) => {
                    error!("failed to do something: {e}");
                    // 決定是繼續還是停止
                    ActorAction::Warn(e.to_string())
                }
            }
        }
    }
}
```

🔸 **狀態管理**

```rust
// ✅ Actor 封裝所有狀態
pub struct MyActor {
    state: MyState,
    config: MyConfig,
    // 所有欄位都是私有的
}

// ❌ 不要在 Actor 外部共享可變狀態
pub struct MyActor {
    state: Arc<Mutex<MyState>>, // 違反 Actor 原則
}
```

## 總結

Sail 的 Actor 模型提供了：

✅ **簡潔的並發模型**：訊息傳遞取代鎖
✅ **型別安全**：編譯期保證訊息型別
✅ **錯誤隔離**：Actor 崩潰不影響其他 Actor
✅ **背壓支援**：自動防止記憶體耗盡
✅ **測試友善**：可單獨測試每個 Actor

Sail 中的三個主要 Actor：

| Actor | 職責 | 特點 |
|-------|------|------|
| SessionManagerActor | 管理 Spark 會話 | 使用延遲訊息實作超時檢測 |
| WorkerActor | 執行查詢任務 | 啟動 gRPC 伺服器，背景執行任務 |
| DriverActor | 調度分散式執行 | 管理 Worker 池，追蹤任務狀態 |

透過 Actor 模型，Sail 實作了一個簡潔、安全、可擴展的並發架構。
