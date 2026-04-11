# 第零階段：PySpark 客戶端連接建立

## 範例 SQL

```python
# PySpark Client
from pyspark.sql import SparkSession

spark = SparkSession.builder \
    .remote("sc://localhost:50051") \
    .getOrCreate()

result = spark.sql("SELECT 1 + 1 AS sum").collect()
print(result)  # [Row(sum=2)]
```

## 範例代碼

```python
# Line 10-12: 建立連接
spark = SparkSession.builder \
    .remote("sc://localhost:50051") \
    .getOrCreate()
```

這三行代碼看似簡單，實際上會觸發一系列複雜的初始化流程。

---

## 完整調用鏈

```
PySpark Client                        Network                      Sail Server
     |                                   |                              |
     | SparkSession.builder              |                              |
     |   .remote("sc://localhost:50051") |                              |
     |   .getOrCreate()                  |                              |
     |-----------------------------------|                              |
     | 1. 解析連接字串                    |                              |
     | 2. 創建 gRPC Channel (HTTP/2)     |                              |
     | 3. 生成 session_id & user_id      |                              |
     | 4. 創建 SparkSession 對象          |                              |
     |   (此時未發送任何請求)              |                              |
     |                                   |                              |
     | spark.sql("SELECT ...")           |                              |
     | (第一次真正的操作)                  |                              |
     |                                   |                              |
     | 5. ConfigRequest (gRPC)           |                              |
     |---------------------------------->|----------------------------->|
     |    session_id, user_id            |  TCP/IP (port 50051)         | 6. entrypoint.rs::serve()
     |                                   |  HTTP/2 Stream                |    ↓
     |                                   |                              | 7. server.rs::config()
     |                                   |                              |    SparkConnectService trait
     |                                   |                              |    ↓
     |                                   |                              | 8. session_manager.rs
     |                                   |                              |    ::get_or_create_session_context()
     |                                   |                              |    ↓
     |                                   |                              | 9. SessionManagerActor
     |                                   |                              |    (Actor 模型)
     |                                   |                              |    ↓
     |                                   |                              | 10. create_session_context()
     |                                   |                              |     ├─ 創建 JobRunner
     |                                   |                              |     ├─ 配置 SessionConfig
     |                                   |                              |     ├─ 設置 Catalog
     |                                   |                              |     ├─ 配置 RuntimeEnv
     |                                   |                              |     ├─ 設置緩存 (3種)
     |                                   |                              |     └─ 註冊優化規則
     |                                   |                              |    ↓
     |                                   |                              | 11. 存儲 SessionContext
     |                                   |                              |     self.sessions.insert(key, ctx)
     |                                   |                              |    ↓
     | 12. ConfigResponse                |                              | 13. 設置閒置超時檢測
     |<----------------------------------|<-----------------------------|     send_with_delay(timeout)
     |    (配置信息)                      |                              |
     |                                   |                              |
     | 現在可以執行 SQL 了                 |                              | SessionContext 準備就緒
```

---

## 詳細源碼解析

### 步驟 1-4：PySpark 客戶端初始化（本地操作）

🔸 **客戶端做了什麼**

雖然這不是 Sail 的代碼，但了解客戶端邏輯有助於理解整個流程：

```python
# PySpark 內部實作（簡化版）
class Builder:
    def remote(self, url: str):
        # 解析 "sc://localhost:50051"
        # sc:// → Spark Connect 協議
        # localhost → 主機名
        # 50051 → gRPC 端口
        self._connection_string = url
        return self

    def getOrCreate(self):
        # 創建 gRPC channel (HTTP/2 持久連接)
        channel = grpc.insecure_channel('localhost:50051')

        # 生成 session ID (UUID)
        session_id = str(uuid.uuid4())

        # 獲取 user ID
        user_id = os.getenv('USER') or getpass.getuser()

        # 創建 SparkSession 對象（此時還沒發送任何請求！）
        return SparkSession(
            client=SparkConnectClient(
                channel=channel,
                session_id=session_id,
                user_id=user_id
            )
        )
```

🔸 **關鍵點**

- **延遲初始化（Lazy Initialization）**：`.getOrCreate()` 不會立即向服務器發送請求
- **Session ID 生成**：客戶端生成 UUID（例如：`"abc-123-def-456"`）
- **User Context**：從環境變量獲取當前用戶名

---

### 步驟 5：第一個 gRPC 請求（ConfigRequest）

當你執行 `spark.sql("SELECT ...")` 時，PySpark 會發送第一個 gRPC 請求：

```protobuf
ConfigRequest {
  session_id: "abc-123-def-456",
  user_context: UserContext {
    user_id: "stanhsu",
  },
  operation: GetAll {
    prefix: ""  // 獲取所有配置
  }
}
```

---

### 步驟 6：Sail 服務器接收 gRPC 請求

🔸 **檔案位置：`crates/sail-spark-connect/src/entrypoint.rs:13-36`**

這是 Sail 服務器的入口點，負責啟動 gRPC 服務器：

```rust
pub async fn serve<F>(
    listener: TcpListener,         // TCP 監聽器（綁定到 0.0.0.0:50051）
    signal: F,                      // 優雅關閉信號（Ctrl-C）
    options: SessionManagerOptions, // 配置選項
) -> Result<(), Box<dyn std::error::Error>>
where
    F: Future<Output = ()>,
{
    // 創建 SessionManager（管理所有 session）
    let session_manager = SessionManager::new(options);

    // 創建 SparkConnectServer（實作 gRPC service trait）
    let server = SparkConnectServer::new(session_manager);

    // 包裝成 Tonic gRPC service
    let service = SparkConnectServiceServer::new(server)
        .max_decoding_message_size(GRPC_MAX_MESSAGE_LENGTH_DEFAULT)  // 128MB
        .accept_compressed(CompressionEncoding::Gzip)                 // 支援 Gzip 壓縮
        .accept_compressed(CompressionEncoding::Zstd)                 // 支援 Zstd 壓縮
        .send_compressed(CompressionEncoding::Gzip)
        .send_compressed(CompressionEncoding::Zstd);

    // 使用 ServerBuilder 啟動服務器
    ServerBuilder::new("sail_spark_connect", Default::default())
        .add_service(service, Some(crate::spark::connect::FILE_DESCRIPTOR_SET))
        .await
        .serve(listener, signal)    // 開始監聽 TCP 連接
        .await
}
```

🔸 **Tonic 框架的魔法**

- `SparkConnectServiceServer::new(server)` 會自動將你的 `SparkConnectServer` 包裝成 gRPC service
- Tonic 會根據 `.proto` 文件生成的代碼，自動路由請求到對應的方法（如 `config()`, `execute_plan()`）

🔸 **ServerBuilder 做了什麼**

檔案位置：`crates/sail-server/src/builder.rs:101-124`

```rust
pub async fn serve<F>(
    self,
    listener: TcpListener,  // 從 tokio::net::TcpListener
    signal: F,
) -> Result<(), Box<dyn std::error::Error>>
{
    // 添加 gRPC reflection（讓 grpcurl 等工具可以探索 API）
    let reflection_server = self.reflection_server_builder.build_v1()?;
    let router = self.router.add_service(reflection_server);

    // 配置 TCP 選項
    let incoming = TcpIncoming::from(listener)
        .with_nodelay(Some(true))              // 禁用 Nagle 算法（低延遲）
        .with_keepalive(Some(Duration::from_secs(60)));  // TCP keepalive

    // 啟動 HTTP/2 服務器，並支援優雅關閉
    router.serve_with_incoming_shutdown(incoming, signal).await?;

    Ok(())
}
```

---

### 步驟 7：路由到 config() 方法

🔸 **檔案位置：`crates/sail-spark-connect/src/server.rs:247-289`**

Tonic 自動將 `ConfigRequest` 路由到這個方法：

```rust
#[tonic::async_trait]
impl SparkConnectService for SparkConnectServer {
    async fn config(
        &self,
        request: Request<ConfigRequest>,
    ) -> Result<Response<ConfigResponse>, Status> {
        let request = request.into_inner();  // 取出 ConfigRequest

        // 構建 SessionKey（user_id + session_id）
        let session_key = SessionKey {
            user_id: request.user_context.map(|u| u.user_id).unwrap_or_default(),
            session_id: request.session_id.clone(),
        };

        // 【關鍵】獲取或創建 SessionContext
        // 第一次請求時，會觸發 SessionContext 創建
        let ctx = self.session_manager.get_or_create_session_context(session_key).await?;

        // 處理配置請求（省略具體邏輯）
        let response = match request.operation.op_type {
            OpType::GetAll(GetAll { prefix }) => {
                service::handle_config_get_all(&ctx, prefix)?
            }
            OpType::Set(Set { pairs, .. }) => {
                service::handle_config_set(&ctx, pairs)?
            }
            // ... 其他配置操作
        };

        Ok(Response::new(response))
    }

    // 其他方法：execute_plan(), analyze_plan(), ...
}
```

---

### 步驟 8：SessionManager::get_or_create_session_context()

🔸 **檔案位置：`crates/sail-spark-connect/src/session_manager.rs:70-83`**

這個方法是 `SessionManager` 的外觀（Facade），實際工作由 `SessionManagerActor` 完成。

```rust
pub async fn get_or_create_session_context(
    &self,
    key: SessionKey,
) -> SparkResult<SessionContext> {
    // 創建 oneshot channel（用於接收結果）
    let (tx, rx) = oneshot::channel();

    // 構建 Actor 事件
    let event = SessionManagerEvent::GetOrCreateSession {
        key,
        system: self.system.clone(),  // Arc<Mutex<ActorSystem>>
        result: tx,                   // 將發送端交給 Actor
    };

    // 將事件發送給 SessionManagerActor（非阻塞）
    self.handle.send(event).await?;

    // 等待 Actor 處理完成並返回結果（阻塞）
    rx.await.map_err(|e| SparkError::internal(format!("failed to get session: {e}")))?
}
```

🔸 **為什麼使用 Actor 模型？**

1. **並發安全**：多個客戶端同時請求時，Actor 確保 session 創建是序列化的（一次一個）
2. **狀態封裝**：`self.sessions: HashMap<SessionKey, SessionContext>` 是 Actor 的私有狀態，外部無法直接訪問
3. **異步消息傳遞**：請求方不會阻塞 Actor，Actor 可以按自己的節奏處理消息

---

### 步驟 9：SessionManagerActor 處理事件

🔸 **檔案位置：`crates/sail-spark-connect/src/session_manager.rs:348-386`**

`SessionManagerActor` 收到 `GetOrCreateSession` 事件後：

```rust
fn handle_get_or_create_session(
    &mut self,
    ctx: &mut ActorContext<Self>,
    key: SessionKey,
    system: Arc<Mutex<ActorSystem>>,
    result: oneshot::Sender<SparkResult<SessionContext>>,
) -> ActorAction {
    // 檢查 session 是否已存在
    let context = if let Some(context) = self.sessions.get(&key) {
        Ok(context.clone())  // 直接返回已存在的 session
    } else {
        // Session 不存在，創建新的
        info!("creating session {key}");
        match self.create_session_context(system, key.clone()) {
            Ok(context) => {
                // 將新創建的 session 存儲到 HashMap
                self.sessions.insert(key.clone(), context.clone());
                Ok(context)
            }
            Err(e) => Err(e),
        }
    };

    // 設置閒置超時檢測（步驟 13）
    if let Ok(context) = &context {
        if let Ok(active_at) = context
            .extension::<SparkSession>()
            .map_err(|e| e.into())
            .and_then(|spark| spark.track_activity())
        {
            // 在 N 秒後發送 ProbeIdleSession 事件給自己
            ctx.send_with_delay(
                SessionManagerEvent::ProbeIdleSession {
                    key,
                    instant: active_at,
                },
                Duration::from_secs(self.options.config.spark.session_timeout_secs),
            );
        }
    }

    // 將結果發送回等待的調用者
    let _ = result.send(context);
    ActorAction::Continue  // 繼續處理下一個消息
}
```

---

### 步驟 10：create_session_context() - 7 個子步驟

🔸 **檔案位置：`crates/sail-spark-connect/src/session_manager.rs:87-288`**

這是整個連接建立過程中最複雜的部分，包含 7 個子步驟。

#### 步驟 10.1：創建 JobRunner

```rust
// session_manager.rs:93-100
let job_runner: Box<dyn JobRunner> = match options.config.mode {
    ExecutionMode::Local => {
        // Local Mode：單進程，使用 DataFusion 多線程執行
        Box::new(LocalJobRunner::new())
    }
    ExecutionMode::LocalCluster | ExecutionMode::KubernetesCluster => {
        // Cluster Mode：多進程，使用 DriverActor 協調 Worker
        let options = DriverOptions::try_new(&options.config, options.runtime.clone())?;
        let mut system = system.lock()?;
        Box::new(ClusterJobRunner::new(system.deref_mut(), options))
    }
};

```

🔸 **JobRunner 是什麼？**

`JobRunner` 是一個 trait，定義了如何執行查詢計畫：

```rust
// crates/sail-execution/src/job/runner.rs:11-22
#[tonic::async_trait]
pub trait JobRunner: Send + Sync + 'static {
    async fn execute(
        &self,
        ctx: &SessionContext,
        plan: Arc<dyn ExecutionPlan>,  // DataFusion 物理計畫
    ) -> ExecutionResult<SendableRecordBatchStream>;  // 返回結果流

    async fn stop(&self);
}
```

- **LocalJobRunner**：直接調用 DataFusion 的 `execute_stream()`，單進程多線程執行
- **ClusterJobRunner**：通過 `DriverActor` 將計畫分發給多個 Worker 執行

#### 步驟 10.2：配置 SessionConfig

```rust
// session_manager.rs:103-120
let mut session_config = SessionConfig::new()
    .with_create_default_catalog_and_schema(false)  // 不使用 DataFusion 默認 catalog
    .with_information_schema(false)                 // 不使用 information_schema
    .with_extension(Arc::new(create_catalog_manager(
        &options.config,
        options.runtime.clone()
    )?))  // Sail 自定義 catalog（支援 Unity、Iceberg REST 等）
    .with_extension(Arc::new(SparkSession::try_new(
        key.user_id,
        key.session_id,
        job_runner,
        SparkSessionOptions {
            execution_heartbeat_interval: Duration::from_secs(
                options.config.spark.execution_heartbeat_interval_secs,  // 默認 10 秒
            ),
        },
    )?));
```

🔸 **SessionConfig 的 Extension 機制**

DataFusion 允許在 `SessionConfig` 中存儲自定義狀態：
- `CatalogManager`：管理數據庫、表、視圖的元數據
- `SparkSession`：存儲 Spark 特有的狀態（如臨時視圖、JobRunner）

#### 步驟 10.3：配置執行選項

```rust
// session_manager.rs:123-133
{
    let execution = &mut session_config.options_mut().execution;
    execution.batch_size = options.config.execution.batch_size;  // 默認 8192
    execution.collect_statistics = options.config.execution.collect_statistics;
    execution.use_row_number_estimates_to_optimize_partitioning =
        options.config.execution.use_row_number_estimates_to_optimize_partitioning;
    execution.listing_table_ignore_subdirectory = false;  // Spark 不忽略子目錄
}
```

#### 步驟 10.4：配置 Parquet 選項

```rust
// session_manager.rs:136-175
{
    let parquet = &mut session_config.options_mut().execution.parquet;
    parquet.created_by = concat!("sail version ", env!("CARGO_PKG_VERSION")).into();
    parquet.enable_page_index = options.config.parquet.enable_page_index;
    parquet.pruning = options.config.parquet.pruning;
    parquet.pushdown_filters = options.config.parquet.pushdown_filters;
    parquet.compression = Some(options.config.parquet.compression.clone());  // 默認 Snappy
    parquet.max_row_group_size = options.config.parquet.max_row_group_size;  // 默認 1M 行
    // ... 更多 Parquet 配置
}
```

#### 步驟 10.5：配置 RuntimeEnv（對象存儲與三層緩存）

🔸 **檔案位置：`crates/sail-spark-connect/src/session_manager.rs:177-259`**

這是最複雜的部分，配置三種緩存和對象存儲：

```rust
let runtime = {
    // 註冊對象存儲（S3/GCS/HDFS/本地文件系統）
    let registry = DynamicObjectStoreRegistry::new(options.runtime.clone());

    // ===== 緩存 1：文件統計緩存（FileStatisticsCache） =====
    // 用途：緩存 Parquet 文件的統計信息（min/max/null_count）
    // 好處：跳過不相關的文件（謂詞下推）
    let file_statistics_cache: Option<FileStatisticsCache> =
        match &options.config.parquet.file_statistics_cache.r#type {
            CacheType::None => None,
            CacheType::Global => Some(
                self.global_file_statistics_cache
                    .get_or_insert_with(|| {
                        Arc::new(MokaFileStatisticsCache::new(ttl, max_entries))
                    })
                    .clone()
            ),
            CacheType::Session => Some(
                Arc::new(MokaFileStatisticsCache::new(ttl, max_entries))
            ),
        };

    // ===== 緩存 2：文件列表緩存（FileListingCache） =====
    // 用途：緩存 S3 LIST 操作的結果（目錄下有哪些文件）
    // 好處：減少 S3 LIST 調用（LIST 很慢且有費用）
    let file_listing_cache: Option<ListFilesCache> =
        match &options.config.execution.file_listing_cache.r#type {
            CacheType::None => None,
            CacheType::Global => Some(
                self.global_file_listing_cache
                    .get_or_insert_with(|| {
                        Arc::new(MokaFileListingCache::new(ttl, max_entries))
                    })
                    .clone()
            ),
            CacheType::Session => Some(
                Arc::new(MokaFileListingCache::new(ttl, max_entries))
            ),
        };

    // ===== 緩存 3：文件元數據緩存（FileMetadataCache） =====
    // 用途：緩存 Parquet footer（包含 schema、row group 元數據）
    // 好處：減少 S3 GET 調用（讀取小文件末尾）
    let file_metadata_cache: Arc<dyn FileMetadataCache> =
        match options.config.parquet.file_metadata_cache.r#type {
            CacheType::None => Arc::new(MokaFileMetadataCache::new(ttl, Some(0))),  // 大小為 0，不緩存
            CacheType::Global =>
                self.global_file_metadata_cache
                    .get_or_insert_with(|| {
                        Arc::new(MokaFileMetadataCache::new(ttl, size_limit))
                    })
                    .clone(),
            CacheType::Session => Arc::new(MokaFileMetadataCache::new(ttl, size_limit)),
        };

    // 組裝緩存配置
    let cache_config = CacheManagerConfig::default()
        .with_files_statistics_cache(file_statistics_cache)
        .with_list_files_cache(file_listing_cache)
        .with_file_metadata_cache(Some(file_metadata_cache));

    // 構建 RuntimeEnv
    let builder = RuntimeEnvBuilder::default()
        .with_object_store_registry(Arc::new(registry))
        .with_cache_manager(cache_config);

    Arc::new(builder.build()?)
};
```

🔸 **三種緩存類型的區別**

| 緩存類型 | 存儲位置 | 生命週期 | 適用場景 |
|---------|---------|---------|---------|
| `CacheType::None` | 無 | 無 | 開發環境、數據頻繁變化 |
| `CacheType::Session` | `SessionContext` | Session 結束時清理 | 單用戶、獨立查詢 |
| `CacheType::Global` | `SessionManagerActor` | 服務器重啟時清理 | 多用戶、共享數據 |

#### 步驟 10.6：構建 SessionState（註冊優化規則）

```rust
// session_manager.rs:261-271
let state = SessionStateBuilder::new()
    .with_config(session_config)
    .with_runtime_env(runtime)
    .with_default_features()  // DataFusion 默認功能（內建函數、聚合函數等）
    .with_analyzer_rules(default_analyzer_rules())  // Sail 自定義分析規則
    .with_optimizer_rules(default_optimizer_rules())  // Sail 自定義優化規則
    .with_physical_optimizer_rules(get_physical_optimizers(PhysicalOptimizerOptions {
        enable_join_reorder: options.config.optimizer.enable_join_reorder,
    }))
    .with_query_planner(new_query_planner())  // Sail 自定義查詢規劃器
    .build();

let context = SessionContext::new_with_state(state);
```

🔸 **優化規則的作用**

- **分析規則（Analyzer Rules）**：處理未解析的引用（表名、列名）
- **邏輯優化規則（Optimizer Rules）**：優化邏輯計畫（謂詞下推、投影裁剪、常量折疊）
- **物理優化規則（Physical Optimizer Rules）**：優化物理計畫（Join 重排序、管道化）
- **查詢規劃器（Query Planner）**：將邏輯計畫轉換為物理計畫

#### 步驟 10.7：反註冊內建函數

```rust
// session_manager.rs:277-285
for (&name, _function) in BUILT_IN_SCALAR_FUNCTIONS.iter() {
    context.deregister_udf(name);  // 移除 DataFusion 的內建函數
}
for (&name, _function) in BUILT_IN_GENERATOR_FUNCTIONS.iter() {
    context.deregister_udf(name);  // 如 explode()
}
for (&name, _function) in BUILT_IN_TABLE_FUNCTIONS.iter() {
    context.deregister_udtf(name);  // 如 range()
}
```

🔸 **為什麼要反註冊？**

Sail 實作了自己的 Spark 函數（語義與 DataFusion 不同），需要移除 DataFusion 的默認實作，避免衝突。

例如：
- Spark 的 `concat()` 函數遇到 NULL 會返回 NULL
- DataFusion 的 `concat()` 函數會跳過 NULL

---

### 步驟 11：存儲 SessionContext

🔸 **檔案位置：`crates/sail-spark-connect/src/session_manager.rs:364`**

```rust
self.sessions.insert(key, context.clone());
```

將新創建的 `SessionContext` 存儲到 `SessionManagerActor` 的 `HashMap` 中。

🔸 **SessionContext 的內容總結**

| 組件 | 作用 | 配置來源 |
|------|------|----------|
| `JobRunner` | 決定查詢執行方式（Local/Cluster） | `AppConfig.mode` |
| `CatalogManager` | 管理數據庫、表、視圖的元數據 | `AppConfig.catalog` |
| `SparkSession` | 存儲 session 級別的狀態（如臨時視圖） | 用戶 ID + Session ID |
| `DynamicObjectStoreRegistry` | 註冊對象存儲（S3/GCS/HDFS） | `AppConfig.runtime` |
| `FileStatisticsCache` | 緩存 Parquet 文件統計信息 | `AppConfig.parquet.file_statistics_cache` |
| `FileListingCache` | 緩存目錄列表（減少 S3 LIST 調用） | `AppConfig.execution.file_listing_cache` |
| `FileMetadataCache` | 緩存 Parquet footer（減少 S3 GET 調用） | `AppConfig.parquet.file_metadata_cache` |
| 分析規則（Analyzer Rules） | 處理未解析的引用（表名、列名） | `sail-logical-optimizer` |
| 優化規則（Optimizer Rules） | 優化邏輯計畫（謂詞下推、投影裁剪） | `sail-logical-optimizer` |
| 物理優化規則 | 優化物理計畫（Join 重排序） | `sail-physical-optimizer` |
| 查詢規劃器（Query Planner） | 將邏輯計畫轉換為物理計畫 | `sail-plan/planner` |

從現在開始，這個 `SessionContext` 可以用來執行 SQL 查詢了！

---

### 步驟 12：返回 ConfigResponse

🔸 **檔案位置：`crates/sail-spark-connect/src/server.rs:250-260`**

當 `SessionContext` 創建完成後，`config()` 方法繼續處理配置請求：

```rust
// 處理配置請求（省略具體邏輯）
let response = match request.operation.op_type {
    OpType::GetAll(GetAll { prefix }) => {
        service::handle_config_get_all(&ctx, prefix)?
    }
    OpType::Set(Set { pairs, .. }) => {
        service::handle_config_set(&ctx, pairs)?
    }
    // ... 其他配置操作
};

Ok(Response::new(response))
```

🔸 **ConfigResponse 的內容**

```protobuf
ConfigResponse {
  session_id: "abc-123-def-456",
  pairs: [
    {key: "spark.sql.shuffle.partitions", value: "200"},
    {key: "spark.executor.memory", value: "1g"},
    // ... 更多配置
  ]
}
```

這些配置會返回給 PySpark 客戶端，但大多數時候客戶端不會使用這些配置（Spark Connect 模式下，配置由服務器管理）。

---

### 步驟 13：設置閒置超時檢測

🔸 **檔案位置：`crates/sail-spark-connect/src/session_manager.rs:376-382`**

為了避免閒置的 session 佔用資源，Sail 會設置超時檢測：

```rust
ctx.send_with_delay(
    SessionManagerEvent::ProbeIdleSession {
        key,
        instant: active_at,  // 記錄當前的活躍時間
    },
    Duration::from_secs(self.options.config.spark.session_timeout_secs),  // 默認 3600 秒（1 小時）
);
```

🔸 **send_with_delay() 是什麼？**

這是 Actor 系統的特性，可以向自己發送延遲消息：

```rust
// 1 小時後，SessionManagerActor 會收到 ProbeIdleSession 事件
// 檢查這個 session 是否還活躍：
//   - 如果活躍（active_at 時間更新了），不做任何事
//   - 如果閒置（active_at 時間沒變），清理 session
```

🔸 **handle_probe_idle_session() 實作**

檔案位置：`crates/sail-spark-connect/src/session_manager.rs:389-406`

```rust
fn handle_probe_idle_session(
    &mut self,
    ctx: &mut ActorContext<Self>,
    key: SessionKey,
    instant: Instant,  // 上次檢查時的活躍時間
) -> ActorAction {
    let context = self.sessions.get(&key);
    if let Some(context) = context {
        if let Ok(spark) = context.extension::<SparkSession>() {
            // 檢查 session 是否還活躍
            if spark.active_at().is_ok_and(|x| x <= instant) {
                // active_at 沒有更新，說明 session 閒置了
                info!("removing idle session {key}");

                // 停止 JobRunner（關閉 Driver/Worker）
                ctx.spawn(async move {
                    spark.job_runner().stop().await
                });

                // 從 HashMap 中移除 session
                self.sessions.remove(&key);
            }
            // 如果 active_at 更新了，說明 session 還在使用，不做任何事
        }
    }
    ActorAction::Continue
}
```

🔸 **活躍時間如何更新？**

每次客戶端發送請求時，`SessionManager::get_or_create_session_context()` 會調用 `spark.track_activity()`：

```rust
// crates/sail-spark-connect/src/session.rs
impl SparkSession {
    pub fn track_activity(&self) -> SparkResult<Instant> {
        let now = Instant::now();
        *self.active_at.lock()? = now;  // 更新活躍時間
        Ok(now)
    }
}
```

---

## 第零階段總結

現在，我們完整地走過了從 `SparkSession.builder.remote().getOrCreate()` 到 SessionContext 創建的整個流程：

```
1. PySpark 客戶端初始化（本地操作）
   - 解析連接字串
   - 創建 gRPC channel (HTTP/2)
   - 生成 session_id 和 user_id
   - 創建 SparkSession 對象（延遲初始化）

2. 第一個 gRPC 請求（ConfigRequest）
   - 觸發實際的網絡通訊

3. Sail 服務器接收請求
   - entrypoint.rs::serve() 啟動 gRPC 服務器
   - Tonic 框架接收 TCP 連接
   - HTTP/2 協議處理

4. 路由到 config() 方法
   - SparkConnectServer 實作 SparkConnectService trait

5. SessionManager::get_or_create_session_context()
   - 使用 oneshot channel 與 Actor 通訊

6. SessionManagerActor 處理事件
   - 檢查 session 是否存在
   - 如果不存在，調用 create_session_context()

7. create_session_context() - 7 個子步驟
   7.1. 創建 JobRunner（決定執行模式）
   7.2. 配置 SessionConfig（Extension 機制）
   7.3. 配置執行選項（batch_size 等）
   7.4. 配置 Parquet 選項（壓縮、統計信息等）
   7.5. 配置 RuntimeEnv（對象存儲 + 三層緩存）
   7.6. 構建 SessionState（優化規則、查詢規劃器）
   7.7. 反註冊內建函數（使用 Sail 自定義實作）

8. 存儲 SessionContext
   - 將創建的 SessionContext 存儲到 HashMap

9. 返回 ConfigResponse
   - 返回服務器配置給客戶端

10. 設置閒置超時檢測
    - 1 小時後自動檢查並清理閒置 session
```

現在，SessionContext 準備就緒，可以接收真正的 SQL 查詢了！讓我們進入第一階段：SQL 查詢執行。

---
| `CatalogManager` | 管理數據庫、表、視圖的元數據 | `AppConfig.catalog` |
| `SparkSession` | 存儲 session 級別的狀態（如臨時視圖） | 用戶 ID + Session ID |
| `DynamicObjectStoreRegistry` | 註冊對象存儲（S3/GCS/HDFS） | `AppConfig.runtime` |
| `FileStatisticsCache` | 緩存 Parquet 文件統計信息 | `AppConfig.parquet.file_statistics_cache` |
| `FileListingCache` | 緩存目錄列表（減少 S3 LIST 調用） | `AppConfig.execution.file_listing_cache` |
| `FileMetadataCache` | 緩存 Parquet footer（減少 S3 GET 調用） | `AppConfig.parquet.file_metadata_cache` |
| 分析規則（Analyzer Rules） | 處理未解析的引用（表名、列名） | `sail-logical-optimizer` |
| 優化規則（Optimizer Rules） | 優化邏輯計畫（謂詞下推、投影裁剪） | `sail-logical-optimizer` |
| 物理優化規則 | 優化物理計畫（Join 重排序） | `sail-physical-optimizer` |
| 查詢規劃器（Query Planner） | 將邏輯計畫轉換為物理計畫 | `sail-plan/planner` |

## 三種緩存類型的區別

Sail 支援三種緩存類型，影響多個 session 之間的數據共享：

**CacheType::None**
- 不使用緩存
- 每次都重新讀取元數據
- 適合開發環境或數據變化頻繁的場景

**CacheType::Session**
- 每個 session 獨立緩存
- session 結束時緩存被清理
- 適合單用戶場景

**CacheType::Global**
- 所有 session 共享同一個緩存
- 在 `SessionManagerActor` 中存儲（`self.global_file_statistics_cache`）
- 適合多用戶生產環境（節省內存和 S3 調用）

## 時序圖：從 PySpark 連接到 SessionContext 創建

```
PySpark Client                  Sail gRPC Server                SessionManagerActor
      |                                |                                |
      | SparkSession.builder           |                                |
      |   .remote("sc://...")          |                                |
      |   .getOrCreate()               |                                |
      |--------------------------------|                                |
      |   創建 gRPC channel             |                                |
      |   生成 session_id & user_id     |                                |
      |                                |                                |
      | spark.sql("SELECT ...")        |                                |
      |--------------------------------|                                |
      | ConfigRequest (可能先發送)       |                                |
      |------------------------------->|                                |
      |                                | SparkConnectService::config()  |
      |                                |------------------------------->|
      |                                |  GetOrCreateSession event      |
      |                                |                                |
      |                                |  檢查 self.sessions            |
      |                                |  session 不存在，創建新的       |
      |                                |                                |
      |                                |  create_session_context()      |
      |                                |  ├─ 創建 JobRunner             |
      |                                |  ├─ 配置 SessionConfig         |
      |                                |  ├─ 設置執行選項               |
      |                                |  ├─ 配置 Parquet 選項          |
      |                                |  ├─ 創建 RuntimeEnv            |
      |                                |  │  ├─ 註冊對象存儲            |
      |                                |  │  └─ 配置緩存管理器          |
      |                                |  ├─ 構建 SessionState          |
      |                                |  └─ 反註冊內建函數             |
      |                                |                                |
      |                                |  self.sessions.insert(key, ctx)|
      |                                |                                |
      |                                |  設置閒置超時檢測               |
      |                                |  ctx.send_with_delay(          |
      |                                |    ProbeIdleSession,           |
      |                                |    timeout_secs                |
      |                                |  )                             |
      |                                |                                |
      |                                |<-------------------------------|
      |                                |  返回 SessionContext           |
      |<-------------------------------|                                |
      | ConfigResponse                 |                                |
      |                                |                                |
      | ExecutePlanRequest (SQL 查詢)  |                                |
      |------------------------------->|                                |
      |                                | 使用已創建的 SessionContext    |
      |                                | 執行查詢...                    |
```

## PySpark 端的實際代碼（參考）

雖然這不是 Sail 的代碼，但了解 PySpark 客戶端的運作有助於理解整個流程：

```python
# pyspark/sql/connect/session.py（簡化版）
class SparkSession:
    @classmethod
    def builder(cls):
        return Builder()

class Builder:
    def remote(self, url: str):
        # 解析 "sc://localhost:50051"
        self._connection_string = url
        return self

    def getOrCreate(self):
        # 創建 gRPC channel
        channel = grpc.insecure_channel('localhost:50051')

        # 生成 session ID（UUID）
        session_id = str(uuid.uuid4())

        # 獲取 user ID
        user_id = os.getenv('USER') or getpass.getuser()

        # 創建 SparkConnectClient
        client = SparkConnectClient(
            channel=channel,
            session_id=session_id,
            user_id=user_id
        )

        # 創建 SparkSession（此時還沒發送任何請求）
        return SparkSession(client)

    def sql(self, query: str):
        # 第一次執行操作時，才會發送 gRPC 請求
        plan = Plan(
            root=Relation(
                sql=SQL(query=query)
            )
        )

        # 發送 ExecutePlanRequest
        request = ExecutePlanRequest(
            session_id=self._client.session_id,
            user_context=UserContext(user_id=self._client.user_id),
            plan=plan
        )

        # 通過 gRPC 發送請求
        response_stream = self._client.stub.ExecutePlan(request)

        # 返回 DataFrame
        return DataFrame(response_stream, self)
```

## 總結：連接建立的三個階段

**階段 0.1：客戶端初始化（本地操作）**
- 解析連接字串
- 創建 gRPC channel
- 生成 session_id 和 user_id
- 創建 SparkSession 對象（但不發送請求）

**階段 0.2：第一個 gRPC 請求（ConfigRequest 或 ExecutePlanRequest）**
- PySpark 發送第一個請求到 Sail 服務器
- Sail 的 `SessionManager` 收到請求，發現 session 不存在

**階段 0.3：SessionContext 創建（服務器端）**
- `SessionManagerActor` 創建新的 SessionContext
- 配置執行引擎（JobRunner）
- 設置 catalog、緩存、優化規則
- 將 SessionContext 存儲到 `self.sessions` HashMap
- 設置閒置超時檢測（默認 1 小時後自動清理）

現在，SessionContext 已經準備好處理 SQL 查詢了！接下來讓我們看看真正的 SQL 執行流程...

---

## 完整調用鏈概覽

```
PySpark Client
    |
    | (gRPC: Spark Connect Protocol)
    v
Sail Connect Server (crates/sail-spark-connect)
    |
    ├─> 1. 接收 gRPC 請求 (server.rs)
    ├─> 2. 獲取/創建 Session (session_manager.rs)
    ├─> 3. 路由到處理函數 (plan_executor.rs)
    ├─> 4. 解析與計畫轉換 (sail-plan)
    |      ├─> SQL 字串 → Spark Spec
    |      ├─> Spark Spec → DataFusion LogicalPlan
    |      └─> LogicalPlan → PhysicalPlan
    ├─> 5. 執行計畫 (JobRunner)
    |      ├─> Local Mode: DataFusion 直接執行
    |      └─> Cluster Mode: Driver → Workers (Actor 通訊)
    ├─> 6. 結果串流化 (executor.rs)
    |      └─> RecordBatch → Arrow IPC → gRPC Response
    v
PySpark Client 接收結果
```

---

# 第一階段：gRPC 請求進入

## 入口點：SparkConnectService::execute_plan()

🔸 **檔案位置**
`crates/sail-spark-connect/src/server.rs:54-161`

🔸 **Rust 基礎知識**
- `#[tonic::async_trait]`: 這是 Rust 的屬性宏（attribute macro），用來標記這個 trait 實作支援異步方法
- `async fn`: 異步函數，返回 `Future<Output = Result<...>>`
- `Request<T>`: Tonic gRPC 框架的請求包裝器
- `into_inner()`: 消費（consume）Request 包裝器，取出內部的 `ExecutePlanRequest` 結構

```rust
// server.rs:54
async fn execute_plan(
    &self,
    request: Request<ExecutePlanRequest>,
) -> Result<Response<Self::ExecutePlanStream>, Status>
```

🔸 **這個函數做什麼**

當 PySpark 客戶端發送 `spark.sql("SELECT 1 + 1")` 時，會通過 gRPC 傳送一個 `ExecutePlanRequest`。這個請求包含：

```protobuf
message ExecutePlanRequest {
  string session_id = 1;           // 會話 ID
  UserContext user_context = 2;    // 用戶資訊
  optional string operation_id = 6; // 操作 ID（用於重連）
  Plan plan = 3;                   // 查詢計畫（可能是 SQL 字串或已序列化的計畫）
}
```

🔸 **源碼解析**

```rust
// server.rs:60-76
let request = request.into_inner();  // 取出 ExecutePlanRequest
debug!("{request:?}");               // 記錄請求內容

// 構建 SessionKey（用於識別不同用戶的不同會話）
let session_key = SessionKey {
    user_id: request.user_context.map(|u| u.user_id).unwrap_or_default(),
    session_id: request.session_id,
};

// 構建執行器元數據
let metadata = ExecutorMetadata {
    operation_id: request.operation_id.unwrap_or_else(|| Uuid::new_v4().to_string()),
    tags: request.tags,
    reattachable: is_reattachable(&request.request_options),
};

// 獲取或創建 SessionContext（這裡會觸發 Actor 通訊）
let ctx = self.session_manager.get_or_create_session_context(session_key).await?;
```

🔸 **request.plan 的兩種形式**

```rust
// server.rs:77-79
let Plan { op_type: op } = request.plan.required("plan")?;
let op = op.required("plan op")?;
match op {
    plan::OpType::Root(relation) => { ... }    // DataFrame API 呼叫
    plan::OpType::Command(Command { ... }) => { ... }  // SQL 或命令
}
```

對於我們的 `SELECT 1 + 1` 範例，會走 `Command` 分支，具體是 `CommandType::SqlCommand`。

---

# 第二階段：SessionManager 與 Actor 系統

## SessionManager::get_or_create_session_context()

🔸 **檔案位置**
`crates/sail-spark-connect/src/session_manager.rs:70-83`

🔸 **Rust 基礎知識**
- `oneshot::channel()`: 創建一個單次使用的通道（channel），用於異步任務之間傳遞結果
- `tx` (transmitter): 發送端
- `rx` (receiver): 接收端
- `await?`: 等待異步操作完成，如果發生錯誤則早期返回（類似 `try-catch` 的語法糖）

```rust
// session_manager.rs:70-83
pub async fn get_or_create_session_context(
    &self,
    key: SessionKey,
) -> SparkResult<SessionContext> {
    let (tx, rx) = oneshot::channel();  // 創建單次通道

    // 構建 Actor 事件
    let event = SessionManagerEvent::GetOrCreateSession {
        key,
        system: self.system.clone(),  // Arc<Mutex<ActorSystem>>
        result: tx,                   // 將發送端交給 Actor
    };

    // 將事件發送給 SessionManagerActor
    self.handle.send(event).await?;

    // 等待 Actor 處理完成並返回結果
    rx.await.map_err(|e| SparkError::internal(format!("failed to get session: {e}")))?
}
```

🔸 **Actor 處理邏輯**

`SessionManagerActor` 收到 `GetOrCreateSession` 事件後：

```rust
// session_manager.rs:348-386
fn handle_get_or_create_session(
    &mut self,
    ctx: &mut ActorContext<Self>,
    key: SessionKey,
    system: Arc<Mutex<ActorSystem>>,
    result: oneshot::Sender<SparkResult<SessionContext>>,
) -> ActorAction {
    // 如果 session 已存在，直接返回
    let context = if let Some(context) = self.sessions.get(&key) {
        Ok(context.clone())
    } else {
        // 創建新的 SessionContext
        info!("creating session {key}");
        match self.create_session_context(system, key.clone()) {
            Ok(context) => {
                self.sessions.insert(key, context.clone());
                Ok(context)
            }
            Err(e) => Err(e),
        }
    };

    // 設置閒置超時檢測
    if let Ok(context) = &context {
        if let Ok(active_at) = context.extension::<SparkSession>().map_err(|e| e.into()).and_then(|spark| spark.track_activity()) {
            ctx.send_with_delay(
                SessionManagerEvent::ProbeIdleSession { key, instant: active_at },
                Duration::from_secs(self.options.config.spark.session_timeout_secs),
            );
        }
    }

    let _ = result.send(context);  // 將結果發送回等待的調用者
    ActorAction::Continue
}
```

🔸 **為什麼要用 Actor 模型**

1. **並發安全**：多個客戶端同時請求時，Actor 確保 session 創建是線程安全的
2. **狀態管理**：`self.sessions: HashMap<SessionKey, SessionContext>` 是 Actor 的私有狀態
3. **超時管理**：Actor 可以向自己發送延遲消息（`send_with_delay`），實現閒置 session 自動清理

---

# 第三階段：SQL 命令處理

## 路由到 handle_execute_sql_command()

🔸 **檔案位置**
`crates/sail-spark-connect/src/service/plan_executor.rs:198-231`

```rust
// server.rs:100-101
CommandType::SqlCommand(sql) => {
    service::handle_execute_sql_command(&ctx, sql, metadata).await?
}
```

🔸 **handle_execute_sql_command 源碼**

```rust
// plan_executor.rs:198-231
pub(crate) async fn handle_execute_sql_command(
    ctx: &SessionContext,
    sql: SqlCommand,
    metadata: ExecutorMetadata,
) -> SparkResult<ExecutePlanResponseStream> {
    let spark = ctx.extension::<SparkSession>()?;  // 取得 SparkSession

    // 將 SQL 命令包裝成 Relation
    let relation = if let Some(input) = sql.input {
        input  // 如果有 input DataFrame，使用它
    } else {
        // 否則創建一個 SQL Relation
        Relation {
            common: None,
            #[expect(deprecated)]
            rel_type: Some(relation::RelType::Sql(crate::spark::connect::Sql {
                query: sql.sql,           // "SELECT 1 + 1 AS sum"
                args: sql.args,           // SQL 參數（如果有）
                pos_args: sql.pos_args,
                named_arguments: sql.named_arguments,
                pos_arguments: sql.pos_arguments,
            })),
        }
    };

    // 將 Relation 轉換為 Sail 內部的 spec::Plan
    let plan = relation.try_into()?;

    // 進入統一的計畫執行流程
    handle_execute_plan(ctx, plan, metadata, ExecutePlanMode::Lazy).await
}
```

🔸 **關鍵轉換：Protobuf Relation → spec::Plan**

`relation.try_into()?` 這行會調用 `impl TryFrom<Relation> for spec::Plan`，將 gRPC 的 protobuf 消息轉換為 Sail 內部的計畫表示。

對於 SQL 查詢，會生成：
```rust
spec::Plan::Query(spec::QueryPlan {
    plan_id: ...,
    node: spec::QueryNode::Read {
        read_type: spec::ReadType::Sql { query: "SELECT 1 + 1 AS sum", ... },
        is_streaming: false,
    },
})
```

---

# 第四階段：計畫解析與優化

## handle_execute_plan() → resolve_and_execute_plan()

🔸 **檔案位置**
`crates/sail-spark-connect/src/service/plan_executor.rs:109-144`

```rust
// plan_executor.rs:109-144
async fn handle_execute_plan(
    ctx: &SessionContext,
    plan: spec::Plan,
    metadata: ExecutorMetadata,
    mode: ExecutePlanMode,
) -> SparkResult<ExecutePlanResponseStream> {
    let spark = ctx.extension::<SparkSession>()?;
    let operation_id = metadata.operation_id.clone();

    // 核心：解析並執行計畫（這裡會經過多層轉換）
    let (plan, _) = resolve_and_execute_plan(ctx, spark.plan_config()?, plan).await?;

    // 調用 JobRunner 執行物理計畫
    let stream = spark.job_runner().execute(ctx, plan).await?;

    // 根據模式創建不同的執行器
    let rx = match mode {
        ExecutePlanMode::Lazy => {
            // 懶執行：客戶端讀取時才真正計算
            let executor = Executor::new(metadata, stream, spark.options().execution_heartbeat_interval);
            let rx = executor.start()?;
            spark.add_executor(executor)?;  // 註冊 executor 以便中斷/重連
            rx
        }
        ExecutePlanMode::EagerSilent => {
            // 急切執行：立即執行但不返回數據（用於 DDL）
            let _ = read_stream(stream).await?;
            let (tx, rx) = tokio::sync::mpsc::channel(1);
            if metadata.reattachable {
                tx.send(ExecutorOutput::complete()).await?;
            }
            ReceiverStream::new(rx)
        }
    };

    Ok(ExecutePlanResponseStream::new(spark.session_id().to_string(), operation_id, Box::pin(rx)))
}
```

## resolve_and_execute_plan() - 三層轉換

🔸 **檔案位置**
`crates/sail-plan/src/lib.rs:55-87`

🔸 **Rust 基礎知識**
- `Arc<dyn ExecutionPlan>`: Arc 是原子引用計數智能指針，`dyn` 表示動態分發的 trait 對象
- `StringifiedPlan`: 用於 EXPLAIN 的計畫字串表示
- `LogicalPlan`: DataFusion 的邏輯計畫（表達「做什麼」）
- `ExecutionPlan`: DataFusion 的物理計畫（表達「如何做」）

```rust
// sail-plan/src/lib.rs:55-87
pub async fn resolve_and_execute_plan(
    ctx: &SessionContext,
    config: Arc<PlanConfig>,
    plan: spec::Plan,
) -> PlanResult<(Arc<dyn ExecutionPlan>, Vec<StringifiedPlan>)> {
    let mut info = vec![];
    let resolver = PlanResolver::new(ctx, config);

    // ========== 第一步：Spark Spec → DataFusion LogicalPlan ==========
    let NamedPlan { plan, fields } = resolver.resolve_named_plan(plan).await?;
    info.push(plan.to_stringified(PlanType::InitialLogicalPlan));

    // ========== 第二步：執行 DDL 並取得 DataFrame ==========
    // 這裡會處理 Extension 節點（如 CatalogCommand）
    let df = execute_logical_plan(ctx, plan).await?;
    let (session_state, plan) = df.into_parts();

    // ========== 第三步：邏輯計畫優化 ==========
    let plan = session_state.optimize(&plan)?;

    // 處理 streaming 計畫（如果是流式查詢）
    let plan = if is_streaming_plan(&plan)? {
        rewrite_streaming_plan(plan)?
    } else {
        plan
    };
    info.push(plan.to_stringified(PlanType::FinalLogicalPlan));

    // ========== 第四步：LogicalPlan → PhysicalPlan ==========
    let plan = session_state.query_planner().create_physical_plan(&plan, &session_state).await?;

    // ========== 第五步：重命名字段（如果需要） ==========
    let plan = if let Some(fields) = fields {
        rename_physical_plan(plan, &fields)?
    } else {
        plan
    };

    info.push(StringifiedPlan::new(PlanType::FinalPhysicalPlan, displayable(plan.as_ref()).indent(true).to_string()));

    Ok((plan, info))
}
```

🔸 **第一步詳解：resolve_named_plan()**

對於 SQL 查詢 `SELECT 1 + 1 AS sum`：

```rust
// sail-plan/src/resolver/plan.rs:16-29
pub async fn resolve_named_plan(&self, plan: spec::Plan) -> PlanResult<NamedPlan> {
    let mut state = PlanResolverState::new();
    match plan {
        spec::Plan::Query(query) => {
            // 遞迴解析查詢計畫
            let plan = self.resolve_query_plan(query, &mut state).await?;
            let fields = Some(Self::get_field_names(plan.schema(), &state)?);
            Ok(NamedPlan { plan, fields })
        }
        spec::Plan::Command(command) => {
            // 解析命令（如 DDL、DML）
            let plan = self.resolve_command_plan(command, &mut state).await?;
            Ok(NamedPlan { plan, fields: None })
        }
    }
}
```

SQL 查詢會經過：
1. **SQL 字串解析**（在 `spec::Plan` 創建時已完成，使用 `sail-sql-parser`）
2. **表引用解析**（如果有 FROM 子句）
3. **表達式解析**（`1 + 1` 會被解析為 `Add(Literal(1), Literal(1))`）
4. **別名處理**（`AS sum`）

生成的 DataFusion `LogicalPlan` 大致如下：

```
Projection: 1 + 1 AS sum
  EmptyRelation
```

🔸 **第四步詳解：create_physical_plan()**

使用 Sail 的自定義 `QueryPlanner`（在 `sail-plan/src/planner.rs` 中定義），將邏輯計畫轉換為物理計畫：

```
ProjectionExec: expr=[1 + 1 AS sum]
  EmptyExec: produce_one_row=true
```

---

# 第五階段：計畫執行（JobRunner）

## JobRunner Trait 與兩種實作

🔸 **檔案位置**
`crates/sail-execution/src/job/runner.rs:11-93`

```rust
// runner.rs:11-22
#[tonic::async_trait]
pub trait JobRunner: Send + Sync + 'static {
    async fn execute(
        &self,
        ctx: &SessionContext,
        plan: Arc<dyn ExecutionPlan>,
    ) -> ExecutionResult<SendableRecordBatchStream>;

    async fn stop(&self);
}
```

🔸 **Local Mode: LocalJobRunner**

```rust
// runner.rs:44-60
async fn execute(
    &self,
    ctx: &SessionContext,
    plan: Arc<dyn ExecutionPlan>,
) -> ExecutionResult<SendableRecordBatchStream> {
    if self.stopped.load(Ordering::Relaxed) {
        return Err(ExecutionError::InternalError("job runner is stopped".to_string()));
    }
    // 直接使用 DataFusion 執行
    Ok(execute_stream(plan, ctx.task_ctx())?)
}
```

`execute_stream()` 是 DataFusion 的函數，會：
1. 呼叫 `plan.execute()` 取得 `SendableRecordBatchStream`
2. 這是一個異步流（Stream），每次 poll 會產生一個 `RecordBatch`

對於 `SELECT 1 + 1`：
- `EmptyExec` 產生一行空行
- `ProjectionExec` 計算 `1 + 1 = 2`，產生 `RecordBatch { schema: [sum: Int32], rows: [[2]] }`

🔸 **Cluster Mode: ClusterJobRunner**

```rust
// runner.rs:75-93
async fn execute(
    &self,
    _ctx: &SessionContext,
    plan: Arc<dyn ExecutionPlan>,
) -> ExecutionResult<SendableRecordBatchStream> {
    let (tx, rx) = oneshot::channel();

    // 向 DriverActor 發送執行任務
    self.driver.send(DriverEvent::ExecuteJob { plan, result: tx }).await?;

    // 等待 Driver 返回結果流
    rx.await.map_err(|e| ExecutionError::InternalError(format!("failed to create job stream: {e}")))?
}
```

在 Cluster Mode 下，`DriverActor` 會：
1. 將物理計畫分割成多個 Stage（基於 shuffle 邊界）
2. 為每個 Stage 創建 Task
3. 將 Task 分配給 Worker（通過 gRPC 調用 `WorkerService::RunTask`）
4. Worker 執行 Task，將結果寫入 shuffle 存儲
5. Driver 收集最終結果，返回流給客戶端

詳細的 Cluster 執行流程請參考 `SAIL_ARCHITECTURE.md` 的「Cluster Mode 查詢執行流程（8 階段）」章節。

---

# 第六階段：結果串流化

## Executor：將 RecordBatch 流轉換為 gRPC 響應流

🔸 **檔案位置**
`crates/sail-spark-connect/src/executor.rs:96-149`

```rust
// executor.rs:96-122
pub(crate) struct Executor {
    pub(crate) metadata: ExecutorMetadata,
    state: Mutex<ExecutorState>,
}

enum ExecutorState {
    Idle,
    Pending(ExecutorTaskContext),  // 等待開始執行
    Running(ExecutorTask),          // 正在執行
    Pausing,
    Failed(SparkError),
}

struct ExecutorTaskContext {
    stream: SendableRecordBatchStream,  // DataFusion 的結果流
    heartbeat_interval: Duration,       // 心跳間隔（防止客戶端超時）
    buffer: Arc<Mutex<ExecutorBuffer>>, // 緩衝區（用於重連）
}
```

🔸 **Executor::start() 啟動異步任務**

```rust
impl Executor {
    pub fn start(&self) -> SparkResult<ReceiverStream<ExecutorOutput>> {
        let mut state = self.state.lock()?;
        match mem::replace(state.deref_mut(), ExecutorState::Idle) {
            ExecutorState::Pending(mut context) => {
                let (tx, rx) = mpsc::channel(8);  // 創建 channel
                let (notifier, notified) = oneshot::channel();

                // 在背景 spawn 一個異步任務
                let handle = tokio::spawn(async move {
                    // 主迴圈：持續從 stream 讀取 RecordBatch
                    loop {
                        tokio::select! {
                            // 等待 notifier 關閉信號
                            _ = &mut notified => {
                                break Ok(());
                            }
                            // 從 DataFusion stream 讀取下一批數據
                            batch = context.next() => {
                                let batch = batch?;
                                match batch {
                                    Some(batch) => {
                                        // 將 RecordBatch 序列化為 Arrow IPC 格式
                                        let output = ExecutorOutput::new(ExecutorBatch::ArrowBatch(to_arrow_batch(batch)?));
                                        context.save_output(&output)?;  // 保存到緩衝區
                                        if tx.send(output).await.is_err() {
                                            break Ok(());  // 客戶端斷開連接
                                        }
                                    }
                                    None => {
                                        // 流結束，發送完成信號
                                        let output = ExecutorOutput::complete();
                                        context.save_output(&output)?;
                                        let _ = tx.send(output).await;
                                        break Ok(());
                                    }
                                }
                            }
                        }
                    }
                });

                *state = ExecutorState::Running(ExecutorTask { notifier, handle, buffer: context.buffer });
                Ok(ReceiverStream::new(rx))
            }
            _ => Err(SparkError::internal("executor is not pending")),
        }
    }
}
```

🔸 **to_arrow_batch() - 序列化為 Arrow IPC**

```rust
pub fn to_arrow_batch(batch: RecordBatch) -> SparkResult<ArrowBatch> {
    let mut cursor = Cursor::new(Vec::new());
    {
        let mut writer = StreamWriter::try_new(&mut cursor, &batch.schema())?;
        writer.write(&batch)?;
        writer.finish()?;
    }
    let data = cursor.into_inner();
    let row_count = batch.num_rows() as i64;
    Ok(ArrowBatch { row_count, data })
}
```

這裡使用 Apache Arrow 的 IPC（Inter-Process Communication）格式，這是一種零拷貝的列式數據格式：
- 保留 Arrow 的內存佈局
- 可以直接在 PySpark 端解析，無需反序列化

---

# 第七階段：gRPC 響應流返回

## ExecutePlanResponseStream

🔸 **檔案位置**
`crates/sail-spark-connect/src/service/plan_executor.rs:33-99`

```rust
// plan_executor.rs:33-47
pub struct ExecutePlanResponseStream {
    session_id: String,
    operation_id: String,
    inner: ExecutorOutputStream,  // Pin<Box<dyn Stream<Item = ExecutorOutput> + Send>>
}

impl Stream for ExecutePlanResponseStream {
    type Item = Result<ExecutePlanResponse, Status>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<ExecutePlanResponse, Status>>> {
        // 從 inner 流中 poll 下一個 ExecutorOutput
        self.inner.as_mut().poll_next(cx).map(|poll| {
            poll.map(|item| {
                // 構建 gRPC response
                let mut response = ExecutePlanResponse::default();
                response.session_id.clone_from(&self.session_id);
                response.server_side_session_id.clone_from(&self.session_id);
                response.operation_id.clone_from(&self.operation_id);
                response.response_id = item.id;

                // 根據 batch 類型設置 response_type
                match item.batch {
                    ExecutorBatch::ArrowBatch(batch) => {
                        response.response_type = Some(ResponseType::ArrowBatch(batch));
                    }
                    ExecutorBatch::Complete => {
                        response.response_type = Some(ResponseType::ResultComplete(ResultComplete::default()));
                    }
                    // ... 其他類型
                }

                Ok(response)
            })
        })
    }
}
```

🔸 **gRPC Response 的內容**

對於 `SELECT 1 + 1`，會返回兩個 `ExecutePlanResponse`：

**Response 1 (數據批次)**
```protobuf
ExecutePlanResponse {
  session_id: "...",
  operation_id: "...",
  response_id: "uuid-1",
  response_type: ArrowBatch {
    row_count: 1,
    data: <Arrow IPC bytes containing [Row(sum=2)]>
  }
}
```

**Response 2 (完成信號)**
```protobuf
ExecutePlanResponse {
  session_id: "...",
  operation_id: "...",
  response_id: "uuid-2",
  response_type: ResultComplete {}
}
```

---

# 第八階段：PySpark 客戶端接收結果

PySpark 的 Spark Connect 客戶端會：
1. 從 gRPC 流中讀取多個 `ExecutePlanResponse`
2. 將 `ArrowBatch.data` 反序列化為 Arrow RecordBatch
3. 將多個 RecordBatch 組合成 DataFrame
4. 當收到 `ResultComplete` 時結束

最終用戶收到：
```python
[Row(sum=2)]
```

---

# 關鍵 Crate 功能總結

| Crate                  | 職責                                      | 主要類型/函數                                              |
|------------------------|-------------------------------------------|-----------------------------------------------------------|
| `sail-spark-connect`   | Spark Connect gRPC 服務實作               | `SparkConnectServer`, `SessionManager`, `Executor`       |
| `sail-plan`            | Spark 計畫 → DataFusion 計畫轉換           | `PlanResolver`, `resolve_and_execute_plan()`             |
| `sail-sql-parser`      | Spark SQL 解析器（基於 chumsky）           | `parser::parse()`, AST 定義                               |
| `sail-execution`       | 分散式執行協調（Driver/Worker）             | `JobRunner`, `DriverActor`, `WorkerActor`                |
| `sail-session`         | Session 與 Catalog 管理                   | `CatalogManager`, `SparkSession`                         |
| `sail-logical-optimizer` | 邏輯計畫優化規則                         | `default_optimizer_rules()`                              |
| `sail-physical-optimizer`| 物理計畫優化規則                         | `get_physical_optimizers()`                              |
| `sail-common`          | 共享的 spec 定義與配置                    | `spec::Plan`, `spec::Expr`, `AppConfig`                  |

---

# 建議的閱讀順序

如果你想深入學習 Sail 的源碼，建議按以下順序閱讀：

## 第一階段：入口與架構（1-2 天）

1. `crates/sail-cli/README.md` - 了解啟動流程
2. `SAIL_ARCHITECTURE.md` - 理解整體架構
3. `SAIL_ACTOR_MODEL.md` - 理解 Actor 並發模型
4. `crates/sail-cli/src/main.rs` - 看 CLI 如何解析命令
5. `crates/sail-cli/src/spark/server.rs` - 看服務器如何啟動

## 第二階段：gRPC 層（2-3 天）

6. `crates/sail-spark-connect/src/server.rs` - gRPC service 實作
7. `crates/sail-spark-connect/src/session_manager.rs` - Session 管理
8. `crates/sail-spark-connect/src/service/plan_executor.rs` - 計畫執行入口
9. `crates/sail-spark-connect/src/executor.rs` - 結果串流化

## 第三階段：計畫解析與轉換（3-5 天）

10. `crates/sail-common/src/spec/mod.rs` - Sail 內部計畫表示
11. `crates/sail-plan/src/lib.rs` - 計畫解析主入口
12. `crates/sail-plan/src/resolver/plan.rs` - 計畫解析器框架
13. `crates/sail-plan/src/resolver/query/mod.rs` - 查詢計畫解析
14. `crates/sail-plan/src/resolver/query/read.rs` - 表讀取解析
15. `crates/sail-plan/src/resolver/query/project.rs` - 投影解析
16. `crates/sail-plan/src/resolver/expression/mod.rs` - 表達式解析
17. `crates/sail-plan/src/planner.rs` - 自定義 QueryPlanner

## 第四階段：SQL 解析器（2-3 天）

18. `crates/sail-sql-parser/src/parser/mod.rs` - SQL 解析器入口
19. `crates/sail-sql-parser/src/parser/statement.rs` - SQL 語句解析
20. `crates/sail-sql-parser/src/parser/query.rs` - SELECT 查詢解析
21. `crates/sail-sql-parser/src/parser/expression.rs` - 表達式解析
22. `crates/sail-sql-parser/src/ast/mod.rs` - AST 定義

## 第五階段：執行層（3-4 天）

23. `crates/sail-execution/src/job/runner.rs` - JobRunner trait
24. `crates/sail-execution/src/driver/mod.rs` - Driver 架構
25. `crates/sail-execution/src/driver/server.rs` - Driver gRPC 服務
26. `crates/sail-execution/src/driver/event.rs` - Driver 事件定義
27. `crates/sail-execution/src/driver/planner.rs` - 分散式計畫器
28. `crates/sail-execution/src/worker/mod.rs` - Worker 架構
29. `crates/sail-execution/src/worker/server.rs` - Worker gRPC 服務

## 第六階段：Catalog 與數據源（2-3 天）

30. `crates/sail-session/src/catalog/mod.rs` - Catalog 管理
31. `crates/sail-catalog/src/provider/mod.rs` - Catalog provider trait
32. `crates/sail-data-source/src/lib.rs` - 數據源註冊表
33. `crates/sail-delta-lake/src/lib.rs` - Delta Lake 支援
34. `crates/sail-iceberg/src/lib.rs` - Iceberg 支援

## 第七階段：高級功能（選讀）

35. `crates/sail-python-udf/src/lib.rs` - Python UDF 支援
36. `crates/sail-logical-optimizer/src/lib.rs` - 邏輯優化規則
37. `crates/sail-physical-optimizer/src/lib.rs` - 物理優化規則
38. `crates/sail-cache/src/lib.rs` - 緩存實作

---

# 每一層的數據流轉換總結

```
PySpark SQL String
  |
  | gRPC Protobuf (Spark Connect Protocol)
  v
ExecutePlanRequest {
  plan: Plan {
    op_type: Command(SqlCommand { sql: "SELECT 1 + 1 AS sum" })
  }
}
  |
  | TryFrom<Relation> for spec::Plan
  v
spec::Plan::Query(spec::QueryPlan {
  node: QueryNode::Read { read_type: Sql { query: "SELECT 1 + 1 AS sum" } }
})
  |
  | PlanResolver::resolve_named_plan()
  v
DataFusion LogicalPlan
  Projection: 1 + 1 AS sum
    EmptyRelation
  |
  | SessionState::optimize()
  v
Optimized LogicalPlan (相同，因為這個查詢太簡單了)
  |
  | QueryPlanner::create_physical_plan()
  v
DataFusion PhysicalPlan (Arc<dyn ExecutionPlan>)
  ProjectionExec: expr=[1 + 1 AS sum]
    EmptyExec: produce_one_row=true
  |
  | JobRunner::execute()
  v
SendableRecordBatchStream (實作了 Stream<Item = Result<RecordBatch>>)
  |
  | Executor 異步任務 poll stream
  v
RecordBatch {
  schema: Schema([Field { name: "sum", data_type: Int32, ... }]),
  columns: [Int32Array([2])],
  num_rows: 1
}
  |
  | to_arrow_batch() - Arrow IPC 序列化
  v
ArrowBatch {
  row_count: 1,
  data: Vec<u8> (Arrow IPC format)
}
  |
  | ExecutePlanResponseStream
  v
ExecutePlanResponse {
  response_type: Some(ResponseType::ArrowBatch(ArrowBatch { ... }))
}
  |
  | gRPC Stream
  v
PySpark Client 接收 Arrow bytes
  |
  | PyArrow 反序列化
  v
PySpark DataFrame / Row objects
  [Row(sum=2)]
```

---

# 常見問題

## Q1: 為什麼要從 Spark Plan 轉換到 DataFusion Plan？

A: Spark 的計畫表示與 DataFusion 的不同：
- Spark 的 `Relation` 是面向 API 的（RDD/DataFrame 語義）
- DataFusion 的 `LogicalPlan` 是面向查詢優化的（關係代數）
- 轉換層（`sail-plan`）負責語義映射和類型轉換

## Q2: Local Mode 和 Cluster Mode 的主要區別是什麼？

A:
- **Local Mode**: `LocalJobRunner` 直接調用 `execute_stream()`，DataFusion 在本地多線程執行
- **Cluster Mode**: `ClusterJobRunner` 通過 `DriverActor` 將計畫分割成 Stages，分發給 Workers 執行，通過 gRPC 通訊

## Q3: Executor 的作用是什麼？

A: Executor 負責：
1. 將 DataFusion 的 `RecordBatch` 流轉換為 gRPC 響應流
2. 定期發送心跳（空 RecordBatch）防止客戶端超時
3. 緩衝結果以支援客戶端重連（reattachable execution）
4. 處理執行中斷和錯誤

## Q4: Actor 模型在 Sail 中的應用場景？

A:
1. **SessionManager**: 管理多用戶的多個 session，確保並發安全
2. **DriverActor**: 管理分散式查詢的狀態（Worker 註冊、Task 調度、結果收集）
3. **WorkerActor**: 處理 Task 執行請求，管理 shuffle 數據

詳見 `SAIL_ACTOR_MODEL.md`。

## Q5: SQL 解析發生在哪裡？

A:
- SQL 字串在 `spec::Plan` 創建時就已經解析完成（可能在 protobuf 轉換層）
- `sail-sql-parser` 使用 chumsky parser combinator 框架解析 Spark SQL 語法
- 解析結果是 `sail-sql-parser::ast::Statement`，之後轉換為 `spec::Plan`

---

# 總結

這篇文章追蹤了一個 SQL 查詢從客戶端到服務器、從字串到結果的完整旅程。關鍵步驟包括：

1. gRPC 請求接收與路由
2. Session 管理（Actor 模型）
3. 計畫解析與轉換（Spark → DataFusion）
4. 計畫優化與物理計畫生成
5. 計畫執行（Local 或 Cluster）
6. 結果串流化與 Arrow IPC 序列化
7. gRPC 響應流返回

通過理解這個流程，你將掌握 Sail 的核心架構，並能夠深入研究任何感興趣的模塊。

建議結合源碼閱讀，使用 `cargo doc --open` 生成並查看 Rust 文檔，並嘗試在關鍵位置添加日誌以觀察實際執行流程。
