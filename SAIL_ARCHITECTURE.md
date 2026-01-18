# Sail 架構深度解析：從請求到執行的完整旅程

本文深入探討 Sail 如何啟動、接受請求、處理查詢，以及在 Local 和 Cluster 模式下的運作機制。

## 架構總覽

Sail 是一個高性能的 Spark 相容計算引擎，採用 Rust 實作，支援兩種執行模式：

| 模式 | 說明 | 適用場景 |
|------|------|----------|
| **Local** | 單一程序，多執行緒執行 | 開發、測試、小規模資料處理 |
| **LocalCluster** | 本地啟動多個 Worker 程序 | 測試分散式邏輯 |
| **KubernetesCluster** | 在 K8s 上啟動 Worker Pod | 生產環境大規模資料處理 |

🔸 位置：`crates/sail-common/src/config/application.rs:66-78`

```rust
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionMode {
    Local,
    #[serde(alias = "local-cluster")]
    LocalCluster,
    #[serde(
        alias = "kubernetes-cluster",
        alias = "k8s-cluster",
        alias = "k8s_cluster",
        alias = "kube-cluster",
        alias = "kube_cluster"
    )]
    KubernetesCluster,
}
```

## 整體架構圖

```
┌─────────────────────────────────────────────────────────────────┐
│                        PySpark Client                           │
│  spark = SparkSession.builder.remote("sc://localhost:50051")    │
│  df = spark.sql("SELECT * FROM table WHERE id > 100")           │
└────────────────────────┬────────────────────────────────────────┘
                         │ gRPC (Spark Connect Protocol)
                         ↓
┌─────────────────────────────────────────────────────────────────┐
│                    Sail Spark Connect Server                    │
│                                                                 │
│  ┌──────────────────────────────────────────────────────────┐   │
│  │          SparkConnectServer (gRPC Service)               │   │
│  │  - execute_plan()    - analyze_plan()                    │   │
│  │  - config()          - add_artifacts()                   │   │
│  └──────────────────┬───────────────────────────────────────┘   │
│                     │                                           │
│  ┌──────────────────▼───────────────────────────────────────┐   │
│  │         SessionManagerActor (Actor)                      │   │
│  │  - 管理 Session 生命週期                                   │   │
│  │  - 建立 SessionContext                                    │   │
│  │  - 閒置 Session 清理                                       │   │
│  └──────────────────┬───────────────────────────────────────┘   │
│                     │                                           │
│  ┌──────────────────▼───────────────────────────────────────┐   │
│  │            SessionContext (DataFusion)                   │   │
│  │  - SparkSession Extension                                │   │
│  │  - JobRunner (Local 或 Cluster)                          │   │
│  │  - Catalog Provider                                      │   │
│  └──────────────────┬───────────────────────────────────────┘   │
└────────────────────┬┴───────────────────────────────────────────┘
                     │
        ┌────────────┴────────────┐
        │                         │
        ▼ Local Mode              ▼ Cluster Mode
┌───────────────────┐     ┌──────────────────────────────────────┐
│ LocalJobRunner    │     │      ClusterJobRunner                │
│                   │     │  ┌────────────────────────────────┐  │
│ DataFusion        │     │  │   DriverActor (Actor)          │  │
│ execute_stream()  │     │  │  - 任務調度                     │  │
│                   │     │  │  - Worker 管理                 │  │
│ 多執行緒執行        │     │  │  - 狀態追蹤                     │  │
└───────────────────┘     │  └────────┬───────────────────────┘  │
                          │           │ gRPC (內部協議)           │
                          │           ↓                          │
                          │  ┌────────────────────────────────┐  │
                          │  │   WorkerActor (Actor) × N      │  │
                          │  │  - 執行任務                     │  │
                          │  │  - 資料 Shuffle                │  │
                          │  │  - 回報狀態                     │  │
                          │  └────────────────────────────────┘  │
                          └──────────────────────────────────────┘
```

## 第一部分：服務器啟動流程

### 啟動命令

```bash
sail spark server --port 50051
```

### 啟動調用鏈

```
main.rs::main()
  │
  ├─> Python::initialize()              # 初始化嵌入式 Python
  │
  └─> runner::main(args)
        │
        └─> run_spark_connect_server(ip, port)
              │
              ├─> init_telemetry()      # 初始化日誌和追蹤
              │
              ├─> AppConfig::load()     # 載入設定（含 ExecutionMode）
              │
              ├─> RuntimeManager::try_new()  # 建立 Tokio 執行時
              │
              └─> runtime.block_on(async {
                    │
                    ├─> TcpListener::bind((ip, port))  # 綁定 TCP 埠
                    │
                    └─> serve(listener, shutdown(), options)
                          │
                          ├─> SessionManager::new(options)
                          │     │
                          │     └─> ActorSystem::spawn::<SessionManagerActor>()
                          │
                          ├─> SparkConnectServer::new(session_manager)
                          │
                          ├─> SparkConnectServiceServer::new(server)
                          │
                          └─> ServerBuilder::new(...).serve(...)
                                └─> 啟動 gRPC 伺服器，等待請求
                  })
```

### 關鍵配置：ExecutionMode

🔸 位置：`crates/sail-common/src/config/application.yaml`（內嵌）

```yaml
mode: local  # 或 local-cluster、kubernetes-cluster

cluster:
  enable_tls: false
  driver_listen_host: "0.0.0.0"
  driver_listen_port: 50052
  driver_external_host: "localhost"
  driver_external_port: 50052
  worker_initial_count: 2
  worker_max_count: 10
  worker_task_slots: 4
  # ... 更多設定
```

可透過環境變數覆蓋：
```bash
export SAIL_MODE=local-cluster
export SAIL_CLUSTER__WORKER_INITIAL_COUNT=4
sail spark server --port 50051
```

## 第二部分：接收與處理請求

### PySpark 客戶端發送查詢

```python
from pyspark.sql import SparkSession

spark = SparkSession.builder.remote("sc://localhost:50051").getOrCreate()
df = spark.sql("SELECT id, name FROM users WHERE age > 18")
df.show()
```

### gRPC 請求流程

```
PySpark Client
  │ 1. 建立 Spark Connect gRPC 連線
  │
  ├─> ExecutePlanRequest {
  │     session_id: "abc-123"
  │     user_context: { user_id: "user1" }
  │     plan: {
  │       op_type: Root {
  │         relation: Sql {
  │           query: "SELECT id, name FROM users WHERE age > 18"
  │         }
  │       }
  │     }
  │   }
  │
  ↓ gRPC: /spark.connect.SparkConnectService/ExecutePlan

SparkConnectServer::execute_plan()
```

### 服務器處理請求

🔸 位置：`crates/sail-spark-connect/src/server.rs:54-161`

```rust
async fn execute_plan(
    &self,
    request: Request<ExecutePlanRequest>,
) -> Result<Response<Self::ExecutePlanStream>, Status> {
    let request = request.into_inner();

    // 1. 提取 Session 資訊
    let session_key = SessionKey {
        user_id: request.user_context.map(|u| u.user_id).unwrap_or_default(),
        session_id: request.session_id,
    };

    // 2. 取得或建立 Session
    let ctx = self
        .session_manager
        .get_or_create_session_context(session_key)
        .await?;

    // 3. 解析計劃
    let Plan { op_type: op } = request.plan.required("plan")?;
    let op = op.required("plan op")?;

    // 4. 分發處理
    let stream = match op {
        plan::OpType::Root(relation) => {
            service::handle_execute_relation(&ctx, relation, metadata).await?
        }
        plan::OpType::Command(Command { command_type }) => {
            // 處理各種命令（WriteOperation、RegisterFunction 等）
        }
    };

    Ok(Response::new(stream))
}
```

### Session 建立流程

```
SessionManager::get_or_create_session_context(session_key)
  │
  ├─> 發送訊息到 SessionManagerActor
  │     SessionManagerEvent::GetOrCreateSession {
  │       key: session_key,
  │       result: oneshot::channel(),
  │     }
  │
  ↓ Actor 處理訊息

SessionManagerActor::handle_get_or_create_session()
  │
  ├─> 檢查 self.sessions.get(&key)
  │     ├─ 存在: 回傳既有 Session
  │     └─ 不存在: 建立新 Session
  │
  └─> create_session_context(system, key)
        │
        ├─> 建立 JobRunner（根據 ExecutionMode）
        │     ├─ Local: LocalJobRunner::new()
        │     └─ Cluster: ClusterJobRunner::new(system, options)
        │           └─> ActorSystem::spawn::<DriverActor>(options)
        │
        ├─> SessionConfig::new()
        │     ├─> 註冊 CatalogManager Extension
        │     └─> 註冊 SparkSession Extension (含 JobRunner)
        │
        ├─> SessionStateBuilder
        │     ├─> 設定 ObjectStore Registry
        │     ├─> 設定 Optimizer Rules
        │     └─> 設定 Query Planner
        │
        └─> SessionContext::new_with_state(state)
```

### 查詢執行流程

🔸 位置：`crates/sail-spark-connect/src/service/plan_executor.rs:147-154`

```rust
pub(crate) async fn handle_execute_relation(
    ctx: &SessionContext,
    relation: Relation,
    metadata: ExecutorMetadata,
) -> SparkResult<ExecutePlanResponseStream> {
    let plan = relation.try_into()?;  // Spark Plan -> Sail Plan
    handle_execute_plan(ctx, plan, metadata, ExecutePlanMode::Lazy).await
}
```

完整處理流程：

```
handle_execute_relation(ctx, relation, metadata)
  │
  ├─> 1. Spark Plan 轉換為 Sail Spec Plan
  │      Relation (protobuf) -> spec::Plan (內部表示)
  │
  └─> handle_execute_plan(ctx, plan, metadata, mode)
        │
        ├─> 2. 解析與優化計劃
        │      resolve_and_execute_plan(ctx, plan_config, plan)
        │        │
        │        ├─> Resolver::resolve(plan)
        │        │     - 解析 SQL 語法
        │        │     - 解析 Table 引用
        │        │     - 解析 UDF 調用
        │        │
        │        ├─> DataFusion LogicalPlan
        │        │
        │        ├─> Optimizer::optimize()
        │        │     - 謂詞下推
        │        │     - 投影下推
        │        │     - 常數折疊
        │        │     - Join 重排序
        │        │
        │        └─> DataFusion PhysicalPlan
        │
        └─> 3. 執行計劃（根據 JobRunner 型別）
              spark.job_runner().execute(ctx, physical_plan)
                │
                ├─ LocalJobRunner:
                │    └─> datafusion::execute_stream(plan, task_ctx)
                │          - 多執行緒執行
                │          - 回傳 SendableRecordBatchStream
                │
                └─ ClusterJobRunner:
                     └─> driver.send(DriverEvent::ExecuteJob { plan })
                           │
                           └─> DriverActor 處理（詳見下節）
```

## 第三部分：Local Mode 執行

### Local Mode 架構

```
┌────────────────────────────────────────────────────────┐
│                  Sail Server (單一程序)                │
│                                                         │
│  ┌──────────────────────────────────────────────────┐ │
│  │         SparkConnectServer (gRPC)                │ │
│  └────────────────────┬─────────────────────────────┘ │
│                       │                                │
│  ┌────────────────────▼─────────────────────────────┐ │
│  │        SessionManagerActor                       │ │
│  │  sessions: HashMap<SessionKey, SessionContext>   │ │
│  └────────────────────┬─────────────────────────────┘ │
│                       │                                │
│  ┌────────────────────▼─────────────────────────────┐ │
│  │         SessionContext                           │ │
│  │  - SparkSession Extension                        │ │
│  │    - job_runner: LocalJobRunner                  │ │
│  └────────────────────┬─────────────────────────────┘ │
│                       │                                │
│  ┌────────────────────▼─────────────────────────────┐ │
│  │         LocalJobRunner                           │ │
│  │  execute(ctx, plan) {                            │ │
│  │    datafusion::execute_stream(plan, task_ctx)    │ │
│  │  }                                               │ │
│  └────────────────────┬─────────────────────────────┘ │
│                       │                                │
│  ┌────────────────────▼─────────────────────────────┐ │
│  │       DataFusion Execution (多執行緒)           │ │
│  │                                                  │ │
│  │  ┌────────┐  ┌────────┐  ┌────────┐            │ │
│  │  │Thread 1│  │Thread 2│  │Thread 3│  ...       │ │
│  │  │Partition│  │Partition│  │Partition│           │ │
│  │  │   0    │  │   1    │  │   2    │            │ │
│  │  └────────┘  └────────┘  └────────┘            │ │
│  │       │           │           │                 │ │
│  │       └───────────┴───────────┘                 │ │
│  │                   │                             │ │
│  │         SendableRecordBatchStream               │ │
│  └──────────────────────────────────────────────────┘ │
└─────────────────────────────────────────────────────────┘
```

### LocalJobRunner 實作

🔸 位置：`crates/sail-execution/src/job/runner.rs:24-60`

```rust
pub struct LocalJobRunner {
    stopped: AtomicBool,
}

#[tonic::async_trait]
impl JobRunner for LocalJobRunner {
    async fn execute(
        &self,
        ctx: &SessionContext,
        plan: Arc<dyn ExecutionPlan>,
    ) -> ExecutionResult<SendableRecordBatchStream> {
        if self.stopped.load(Ordering::Relaxed) {
            return Err(ExecutionError::InternalError(
                "job runner is stopped".to_string(),
            ));
        }
        // DataFusion 執行，使用多執行緒處理分區
        Ok(execute_stream(plan, ctx.task_ctx())?)
    }

    async fn stop(&self) {
        self.stopped.store(true, Ordering::Relaxed);
    }
}
```

### Local Mode 查詢執行範例

假設查詢：`SELECT id, name FROM users WHERE age > 18`

```
1. 解析 SQL 為 Logical Plan
   └─> Projection [id, name]
         └─> Filter (age > 18)
               └─> TableScan (users)

2. 優化 Logical Plan
   └─> Projection [id, name]
         └─> TableScan (users)
               - 謂詞下推: Filter (age > 18) 推到掃描層
               - 投影下推: 只讀取 [id, name, age] 欄位

3. 轉換為 Physical Plan
   └─> ProjectionExec [id, name]
         └─> CoalesceBatchesExec
               └─> FilterExec (age > 18)
                     └─> ParquetExec
                           - 檔案: s3://bucket/users/*.parquet
                           - 分區: 4 個 (根據 CPU 核心數)

4. DataFusion 執行
   ┌─────────────┬─────────────┬─────────────┬─────────────┐
   │  Thread 1   │  Thread 2   │  Thread 3   │  Thread 4   │
   │ Partition 0 │ Partition 1 │ Partition 2 │ Partition 3 │
   │             │             │             │             │
   │ ParquetExec │ ParquetExec │ ParquetExec │ ParquetExec │
   │ - file_0.pq │ - file_1.pq │ - file_2.pq │ - file_3.pq │
   │             │             │             │             │
   │ FilterExec  │ FilterExec  │ FilterExec  │ FilterExec  │
   │ age > 18    │ age > 18    │ age > 18    │ age > 18    │
   │             │             │             │             │
   │ ProjectExec │ ProjectExec │ ProjectExec │ ProjectExec │
   │ [id, name]  │ [id, name]  │ [id, name]  │ [id, name]  │
   └─────┬───────┴─────┬───────┴─────┬───────┴─────┬───────┘
         │             │             │             │
         └─────────────┴─────────────┴─────────────┘
                           │
                  RecordBatch Stream
                           │
                           ▼
              ┌─────────────────────────┐
              │ gRPC Response Stream    │
              │ (Arrow IPC Format)      │
              └─────────────────────────┘
                           │
                           ▼
                    PySpark Client
                    df.show()
```

### Local Mode 特性

✅ **優點**
- 啟動快速，不需額外程序
- 記憶體效率高，資料不需跨程序傳輸
- 除錯簡單，單一程序
- 適合小規模資料（GB 級別）

❌ **限制**
- 受限於單機資源
- 無法水平擴展
- 大規模資料（TB 級別）處理困難

## 第四部分：Cluster Mode 執行

### Cluster Mode 架構

```
┌──────────────────────────────────────────────────────────────┐
│                    Sail Server (Driver)                      │
│                                                               │
│  ┌────────────────────────────────────────────────────────┐  │
│  │         SparkConnectServer (gRPC)                      │  │
│  └────────────────────┬───────────────────────────────────┘  │
│                       │                                       │
│  ┌────────────────────▼───────────────────────────────────┐  │
│  │        SessionManagerActor                             │  │
│  └────────────────────┬───────────────────────────────────┘  │
│                       │                                       │
│  ┌────────────────────▼───────────────────────────────────┐  │
│  │         SessionContext                                 │  │
│  │  - job_runner: ClusterJobRunner                        │  │
│  │    - driver: ActorHandle<DriverActor>                  │  │
│  └────────────────────┬───────────────────────────────────┘  │
│                       │                                       │
│  ┌────────────────────▼───────────────────────────────────┐  │
│  │         DriverActor (Actor)                            │  │
│  │                                                         │  │
│  │  state:                                                │  │
│  │  - workers: HashMap<WorkerId, WorkerInfo>              │  │
│  │  - jobs: HashMap<JobId, JobState>                      │  │
│  │  - task_queue: VecDeque<TaskId>                        │  │
│  │                                                         │  │
│  │  worker_manager:                                       │  │
│  │  - LocalWorkerManager (LocalCluster 模式)              │  │
│  │  - KubernetesWorkerManager (K8s 模式)                  │  │
│  │                                                         │  │
│  │  worker_clients: HashMap<WorkerId, WorkerClient>       │  │
│  └──────────────┬────────────────────┬────────────────────┘  │
│                 │                    │                        │
│   gRPC (內部協議)                gRPC (內部協議)              │
│                 │                    │                        │
└─────────────────┼────────────────────┼────────────────────────┘
                  │                    │
      ┌───────────▼────────┐   ┌───────▼──────────┐
      │   Worker 1         │   │   Worker 2       │  ...
      │                    │   │                  │
      │  WorkerActor       │   │  WorkerActor     │
      │  - 執行任務        │   │  - 執行任務      │
      │  - 資料 Shuffle    │   │  - 資料 Shuffle  │
      │  - 回報狀態        │   │  - 回報狀態      │
      └────────────────────┘   └──────────────────┘
```

### ClusterJobRunner 實作

🔸 位置：`crates/sail-execution/src/job/runner.rs:62-93`

```rust
pub struct ClusterJobRunner {
    driver: ActorHandle<DriverActor>,
}

impl ClusterJobRunner {
    pub fn new(system: &mut ActorSystem, options: DriverOptions) -> Self {
        // 在 Actor 系統中生成 DriverActor
        let driver = system.spawn(options);
        Self { driver }
    }
}

#[tonic::async_trait]
impl JobRunner for ClusterJobRunner {
    async fn execute(
        &self,
        _ctx: &SessionContext,
        plan: Arc<dyn ExecutionPlan>,
    ) -> ExecutionResult<SendableRecordBatchStream> {
        // 發送訊息給 DriverActor，等待結果
        let (tx, rx) = oneshot::channel();
        self.driver
            .send(DriverEvent::ExecuteJob { plan, result: tx })
            .await?;
        rx.await.map_err(|e| {
            ExecutionError::InternalError(format!("failed to create job stream: {e}"))
        })?
    }

    async fn stop(&self) {
        let _ = self.driver.send(DriverEvent::Shutdown).await;
    }
}
```

### DriverActor 的職責

🔸 位置：`crates/sail-execution/src/driver/actor/core.rs:19-99`

```rust
pub struct DriverActor {
    options: DriverOptions,
    state: DriverState,
    server: ServerMonitor,  // gRPC 伺服器
    worker_manager: Arc<dyn WorkerManager>,  // Worker 生命週期管理
    worker_clients: HashMap<WorkerId, WorkerClient>,  // gRPC 客戶端
    physical_plan_codec: Box<dyn PhysicalExtensionCodec>,
    task_queue: VecDeque<TaskId>,
    task_sequences: HashMap<TaskId, u64>,
    job_outputs: HashMap<JobId, JobOutput>,
}
```

**主要訊息處理**：

1. **ExecuteJob**：執行分散式查詢
2. **RegisterWorker**：Worker 註冊
3. **WorkerHeartbeat**：Worker 心跳
4. **UpdateTask**：任務狀態更新
5. **ProbeIdleWorker**：檢測閒置 Worker

### WorkerActor 的職責

🔸 位置：`crates/sail-execution/src/worker/actor/core.rs:24-99`

```rust
pub struct WorkerActor {
    options: WorkerOptions,
    server: ServerMonitor,  // gRPC 伺服器
    driver_client: DriverClient,  // 與 Driver 通訊
    worker_clients: HashMap<WorkerId, WorkerClient>,  // 與其他 Worker 通訊
    task_signals: HashMap<TaskAttempt, oneshot::Sender<()>>,  // 任務取消信號
    local_streams: HashMap<ChannelName, Box<dyn LocalStream>>,  // 本地資料流
    session_context: Option<Arc<SessionContext>>,
    physical_plan_codec: Box<dyn PhysicalExtensionCodec>,
    sequence: u64,
}
```

**主要訊息處理**：

1. **RunTask**：執行任務
2. **StopTask**：停止任務
3. **CreateLocalStream**：建立本地資料流（用於 Shuffle）
4. **CreateRemoteStream**：建立遠端資料流
5. **ReportTaskStatus**：向 Driver 回報狀態

## 第五部分：Driver-Worker gRPC 通訊協議

### 控制平面協議

🔸 Driver Service 協議（位置：`crates/sail-execution/proto/sail/driver/service.proto`）

```protobuf
service DriverService {
  rpc RegisterWorker(RegisterWorkerRequest) returns (RegisterWorkerResponse) {}
  rpc ReportWorkerHeartbeat(ReportWorkerHeartbeatRequest) returns (ReportWorkerHeartbeatResponse) {}
  rpc ReportTaskStatus(ReportTaskStatusRequest) returns (ReportTaskStatusResponse) {}
}

enum TaskStatus {
  TASK_STATUS_RUNNING = 0;
  TASK_STATUS_SUCCEEDED = 1;
  TASK_STATUS_FAILED = 2;
  TASK_STATUS_CANCELED = 3;
}
```

🔸 Worker Service 協議（位置：`crates/sail-execution/proto/sail/worker/service.proto`）

```protobuf
service WorkerService {
  rpc RunTask(RunTaskRequest) returns (RunTaskResponse) {}
  rpc StopTask(StopTaskRequest) returns (StopTaskResponse) {}
  rpc RemoveStream(RemoveStreamRequest) returns (RemoveStreamResponse) {}
  rpc StopWorker(StopWorkerRequest) returns (StopWorkerResponse) {}
}

message RunTaskRequest {
  uint64 task_id = 1;
  uint64 attempt = 2;
  bytes plan = 3;           // 序列化的 PhysicalPlan
  uint64 partition = 4;
  optional string channel = 5;  // Shuffle 輸出通道
}
```

### 資料平面協議

使用 **Arrow Flight**（基於 gRPC）在 Worker 之間交換資料：

```
Worker 1 (Map Side)                   Worker 2 (Reduce Side)
     │                                        │
     │ 1. 執行 Map 任務                       │
     │    產生中間結果                        │
     │                                        │
     │ 2. 寫入 LocalStream                    │
     │    channel: "job_1_stage_0_part_0"     │
     │                                        │
     │ <──── Arrow Flight do_get() ────────  │ 3. Reduce 任務需要資料
     │                                        │
     │ ──── FlightData Stream ────────────>  │ 4. 串流傳輸 RecordBatch
     │      (Arrow IPC Format)                │
     │                                        │
     │ 5. RemoveStream RPC                    │
     │    清理已消費的資料                    │
```

### DriverClient 實作

🔸 位置：`crates/sail-execution/src/driver/client.rs:16-85`

```rust
pub struct DriverClient {
    inner: ClientHandle<DriverServiceClient<Channel>>,
}

impl DriverClient {
    // Worker 向 Driver 註冊
    pub async fn register_worker(
        &self,
        worker_id: WorkerId,
        host: String,
        port: u16,
    ) -> ExecutionResult<()> {
        let request = RegisterWorkerRequest {
            worker_id: worker_id.into(),
            host,
            port: port as u32,
        };
        self.inner.get().await?.register_worker(request).await?;
        Ok(())
    }

    // Worker 發送心跳
    pub async fn report_worker_heartbeat(&self, worker_id: WorkerId) -> ExecutionResult<()> {
        let request = ReportWorkerHeartbeatRequest {
            worker_id: worker_id.into(),
        };
        self.inner.get().await?.report_worker_heartbeat(request).await?;
        Ok(())
    }

    // Worker 回報任務狀態
    pub async fn report_task_status(
        &self,
        task_id: TaskId,
        attempt: usize,
        status: TaskStatus,
        message: Option<String>,
        cause: Option<CommonErrorCause>,
        sequence: u64,
    ) -> ExecutionResult<()> {
        let request = ReportTaskStatusRequest {
            task_id: task_id.into(),
            attempt: attempt as u64,
            status: TaskStatus::from(status) as i32,
            message,
            cause: cause.map(|x| serde_json::to_string(&x)?).transpose()?,
            sequence,
        };
        self.inner.get().await?.report_task_status(request).await?;
        Ok(())
    }
}
```

### WorkerClient 實作

🔸 位置：`crates/sail-execution/src/worker/client.rs:20-103`

```rust
pub struct WorkerClient {
    client: ClientHandle<WorkerServiceClient<Channel>>,  // 控制平面
    flight_client: ClientHandle<FlightServiceClient<Channel>>,  // 資料平面
}

impl WorkerClient {
    // Driver 發送任務給 Worker
    pub async fn run_task(
        &self,
        task_id: TaskId,
        attempt: usize,
        plan: Vec<u8>,
        partition: usize,
        channel: Option<ChannelName>,
    ) -> ExecutionResult<()> {
        let request = RunTaskRequest {
            task_id: task_id.into(),
            attempt: attempt as u64,
            plan,  // 序列化的 PhysicalPlan
            partition: partition as u64,
            channel: channel.map(|x| x.into()),
        };
        self.client.get().await?.run_task(request).await?;
        Ok(())
    }

    // Worker 從另一個 Worker 獲取 Shuffle 資料
    pub async fn fetch_task_stream(
        &self,
        channel: ChannelName,
        _schema: SchemaRef,
    ) -> ExecutionResult<TaskStreamSource> {
        let ticket = TaskStreamTicket {
            channel: channel.into(),
        };
        let ticket = {
            let mut buf = Vec::with_capacity(ticket.encoded_len());
            ticket.encode(&mut buf)?;
            buf
        };
        let request = arrow_flight::Ticket {
            ticket: ticket.into(),
        };
        // Arrow Flight do_get
        let response = self.flight_client.get().await?.do_get(request).await?;
        let stream = response.into_inner().map_err(|e| e.into());
        let stream = FlightRecordBatchStream::new_from_flight_data(stream)
            .map_err(|e| e.into());
        Ok(Box::pin(stream))
    }
}
```

## 第六部分：完整查詢執行流程（Cluster Mode）

假設查詢：
```sql
SELECT department, COUNT(*) as cnt, AVG(salary) as avg_sal
FROM employees
WHERE age > 25
GROUP BY department
```

### 階段 1：Driver 收到查詢

```
PySpark Client
  │
  └─> ExecutePlanRequest (SQL)
        │
        ▼
SparkConnectServer::execute_plan()
  │
  └─> SessionContext (含 ClusterJobRunner)
        │
        └─> ClusterJobRunner::execute(plan)
              │
              └─> DriverActor 收到 ExecuteJob 訊息
```

### 階段 2：Driver 規劃分散式執行

```
DriverActor::handle_execute_job()
  │
  ├─> 1. 分析 PhysicalPlan，識別 Shuffle 邊界
  │
  │      Stage 0 (Map):
  │        ParquetExec (employees)
  │          -> FilterExec (age > 25)
  │          -> PartialAggregateExec (GROUP BY department)
  │          -> ShuffleWriterExec (按 department hash 分區)
  │
  │      Stage 1 (Reduce):
  │        ShuffleReaderExec
  │          -> FinalAggregateExec (COUNT, AVG)
  │          -> ProjectionExec
  │
  ├─> 2. 為每個 Stage 建立 Tasks
  │      Stage 0: 4 個 Task (對應 4 個 Parquet 檔案)
  │        - Task 0: partition 0
  │        - Task 1: partition 1
  │        - Task 2: partition 2
  │        - Task 3: partition 3
  │
  │      Stage 1: 2 個 Task (根據 shuffle 分區數)
  │        - Task 4: partition 0
  │        - Task 5: partition 1
  │
  └─> 3. 確保有足夠的 Worker
        worker_manager.ensure_workers(required_count)
          ├─ LocalWorkerManager: 啟動本地程序
          └─ KubernetesWorkerManager: 建立 K8s Pod
```

### 階段 3：Worker 註冊

```
Worker 程序啟動
  │
  ├─> WorkerActor::start()
  │     └─> 啟動 gRPC 伺服器 (WorkerService)
  │
  └─> WorkerActor 收到 ServerReady 訊息
        │
        └─> DriverClient::register_worker(worker_id, host, port)
              │
              ▼
        DriverActor 收到 RegisterWorker 請求
          │
          ├─> 記錄 Worker 資訊
          │     state.workers.insert(worker_id, WorkerInfo { ... })
          │
          ├─> 建立 WorkerClient
          │     worker_clients.insert(worker_id, WorkerClient::new(...))
          │
          └─> 開始調度任務
                schedule_tasks()
```

### 階段 4：Driver 調度任務（Stage 0）

```
DriverActor::schedule_tasks()
  │
  ├─> 從 task_queue 取出任務
  │     task = task_queue.pop_front()  // Task 0
  │
  ├─> 選擇 Worker
  │     worker = select_worker()  // Worker 1
  │
  ├─> 序列化 PhysicalPlan
  │     let plan_bytes = physical_plan_codec.encode(&plan)?;
  │
  └─> WorkerClient::run_task(task_id, attempt, plan_bytes, partition, channel)
        │
        ▼
  Worker 1 收到 RunTask RPC
    │
    └─> WorkerActor 收到 RunTask 訊息
```

### 階段 5：Worker 執行任務（Stage 0）

```
WorkerActor::handle_run_task(task_id, plan_bytes, partition, channel)
  │
  ├─> 1. 反序列化 PhysicalPlan
  │      let plan = physical_plan_codec.decode(&plan_bytes)?;
  │
  ├─> 2. 在背景執行任務
  │      ctx.spawn(async move {
  │        let result = execute_task(plan, partition).await;
  │        ...
  │      })
  │
  └─> 3. execute_task() 執行
        │
        ├─> ParquetExec::execute(partition)
        │     - 讀取 file_0.parquet
        │     - 回傳 RecordBatch Stream
        │
        ├─> FilterExec::execute()
        │     - 過濾 age > 25
        │
        ├─> PartialAggregateExec::execute()
        │     - 部分聚合：{ dept: "IT", count: 50, sum: 500000 }
        │
        └─> ShuffleWriterExec::execute()
              - 計算 hash(department) % 2
              - 寫入 LocalStream
                  channel: "job_1_stage_0_part_0"
                  channel: "job_1_stage_0_part_1"

              - 回報完成狀態
                DriverClient::report_task_status(
                  task_id,
                  TaskStatus::Succeeded
                )
```

### 階段 6：Driver 收到任務完成，調度 Stage 1

```
DriverActor 收到 ReportTaskStatus (Task 0 完成)
  │
  ├─> 更新任務狀態
  │     state.tasks[task_0].status = Succeeded
  │
  ├─> 檢查 Stage 0 是否全部完成
  │     所有 Task (0, 1, 2, 3) 都完成
  │
  └─> 調度 Stage 1 的任務
        │
        └─> WorkerClient::run_task(task_4, plan_stage1, partition=0, ...)
              │
              ▼
        Worker 2 收到 RunTask (Stage 1, Task 4)
```

### 階段 7：Worker 執行 Shuffle Read（Stage 1）

```
WorkerActor::handle_run_task(task_4, plan_stage1, partition=0)
  │
  └─> execute_task(plan_stage1, partition=0)
        │
        ├─> ShuffleReaderExec::execute()
        │     │
        │     ├─> 1. 計算需要讀取的 channels
        │     │      需要讀取所有 Stage 0 Worker 的 partition=0 資料
        │     │      - Worker 1: "job_1_stage_0_part_0"
        │     │      - Worker 1: "job_1_stage_0_part_0" (來自不同 Task)
        │     │      - Worker 2: "job_1_stage_0_part_0"
        │     │      - Worker 3: "job_1_stage_0_part_0"
        │     │
        │     ├─> 2. 從各個 Worker 獲取資料
        │     │      for channel in channels {
        │     │        let worker_client = get_worker_client(channel.worker_id);
        │     │        let stream = worker_client.fetch_task_stream(channel, schema).await?;
        │     │        streams.push(stream);
        │     │      }
        │     │
        │     └─> 3. 合併所有 streams
        │           stream::select_all(streams)
        │
        ├─> FinalAggregateExec::execute()
        │     - 最終聚合：合併所有 department 的 count 和 sum
        │     - 計算 AVG = sum / count
        │
        └─> ProjectionExec::execute()
              - 投影：[department, cnt, avg_sal]

              - 回報完成
                DriverClient::report_task_status(task_4, Succeeded)
```

### 階段 8：Driver 收集結果，回傳客戶端

```
DriverActor::handle_update_task(task_4, Succeeded)
  │
  ├─> 檢查所有 Stage 1 任務完成
  │
  ├─> 從 Worker 2 收集最終結果
  │     let result_stream = worker_client.fetch_task_stream(output_channel, schema).await?;
  │
  └─> 透過 oneshot::channel 回傳給 ClusterJobRunner
        result_sender.send(result_stream)
          │
          ▼
ClusterJobRunner::execute() 回傳
  │
  └─> handle_execute_plan() 回傳
        │
        └─> SparkConnectServer 串流傳送給 PySpark Client
              │
              ▼
          PySpark Client
          df.show()

          +----------+---+-------+
          |department|cnt|avg_sal|
          +----------+---+-------+
          |IT        |150| 75000 |
          |HR        | 80| 55000 |
          |Sales     |120| 60000 |
          +----------+---+-------+
```

## 第七部分：Cluster Mode 通訊時序圖

```
PySpark     Spark       Session      Cluster      Driver      Worker      Worker
Client      Connect     Manager      JobRunner    Actor       Manager     Actor 1
  │           │            │             │           │           │           │
  │──SQL─────>│            │             │           │           │           │
  │           │            │             │           │           │           │
  │           │──GetSession─────────────>│           │           │           │
  │           │<──────────────────────SessionContext─┤           │           │
  │           │            │             │           │           │           │
  │           │──Execute────────────────────────────>│           │           │
  │           │            │             │           │           │           │
  │           │            │             │     ExecuteJob        │           │
  │           │            │             │           │           │           │
  │           │            │             │      EnsureWorkers───>│           │
  │           │            │             │           │           │           │
  │           │            │             │           │       StartWorker────>│
  │           │            │             │           │           │           │
  │           │            │             │           │<──────────────Register─┤
  │           │            │             │           │           │           │
  │           │            │             │      RunTask──────────────────────>│
  │           │            │             │           │           │           │
  │           │            │             │           │           │     ExecuteTask
  │           │            │             │           │           │           │
  │           │            │             │           │           │<─Heartbeat─┤
  │           │            │             │           │           │           │
  │           │            │             │           │<──────TaskStatus───────┤
  │           │            │             │           │   (Succeeded)          │
  │           │            │             │           │           │           │
  │           │            │             │      FetchResult──────────────────>│
  │           │            │             │           │           │           │
  │           │            │             │<─────────────────RecordBatch───────┤
  │           │            │             │           │           │           │
  │<────────────────────────────────RecordBatch──────┤           │           │
  │           │            │             │           │           │           │
  │  df.show()│            │             │           │           │           │
```

## 第八部分：核心差異對比

| 特性 | Local Mode | LocalCluster Mode | KubernetesCluster Mode |
|------|------------|-------------------|------------------------|
| **程序數量** | 1 個 | 1 個 Server + N 個 Worker | 1 個 Server + K8s Pods |
| **通訊方式** | 執行緒間記憶體共享 | 本地 gRPC (localhost) | 網路 gRPC |
| **任務執行** | DataFusion 多執行緒 | DriverActor → WorkerActor | DriverActor → WorkerActor |
| **資料 Shuffle** | 記憶體內 | Arrow Flight (localhost) | Arrow Flight (網路) |
| **擴展性** | 受限於 CPU 核心數 | 受限於機器資源 | 水平擴展（K8s） |
| **啟動速度** | 快 | 中 | 慢（需建立 Pod） |
| **適用場景** | 開發、測試、小資料 | 測試分散式邏輯 | 生產環境、大資料 |
| **記憶體使用** | 低 | 中 | 高（多 Pod） |
| **容錯能力** | 無 | 低（本地程序） | 高（K8s 自動重啟） |

## 第九部分：關鍵設計決策

### 為什麼使用 Actor 模型？

✅ **優點**
- 避免手動管理鎖，減少死鎖風險
- 訊息驅動，自然適合分散式系統
- 狀態封裝，降低複雜度
- 錯誤隔離，一個 Actor 崩潰不影響其他

❌ **限制**
- 順序處理訊息，可能成為瓶頸
- 需要設計良好的訊息協議
- 除錯相對困難（訊息非同步）

### 為什麼區分控制平面和資料平面？

**控制平面**（內部 Sail gRPC）
- 任務調度、狀態回報、心跳檢測
- 訊息小、頻率高
- 需要可靠性和順序保證

**資料平面**（Arrow Flight）
- Shuffle 資料傳輸、結果回傳
- 資料量大、需要高吞吐量
- 使用 Arrow IPC 格式零拷貝

### 為什麼 Local Mode 不使用 Actor？

Local Mode 使用 `LocalJobRunner`，直接呼叫 DataFusion 的 `execute_stream`：

```rust
// Local Mode: 簡單直接
Ok(execute_stream(plan, ctx.task_ctx())?)
```

原因：
- **簡單**：不需要額外的程序和通訊
- **高效**：避免訊息序列化/反序列化開銷
- **記憶體效率**：資料在執行緒間共享，不需跨程序

Cluster Mode 必須使用 Actor，因為：
- 需要管理多個 Worker 程序
- 需要追蹤分散式任務狀態
- 需要協調 Shuffle 資料傳輸

## 總結

Sail 的架構設計體現了幾個關鍵原則：

🔸 **漸進式複雜度**
- Local Mode 簡單高效
- Cluster Mode 支援大規模處理
- 使用者可根據需求選擇

🔸 **清晰的責任分離**
- SessionManager：Session 生命週期
- JobRunner：執行模式抽象
- DriverActor：任務調度
- WorkerActor：任務執行

🔸 **高效的資料處理**
- 基於 DataFusion 和 Arrow
- 零拷貝資料傳輸（Arrow Flight）
- 多執行緒並行（Local）
- 分散式並行（Cluster）

🔸 **可靠的分散式協調**
- Actor 模型管理狀態
- gRPC 提供可靠通訊
- 心跳檢測和超時機制
- 任務重試和容錯

透過這樣的設計，Sail 在保持簡單易用的同時，提供了從本地開發到生產環境的完整解決方案。
