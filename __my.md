# Sail 執行流程：從 PySpark 到結果

我的簡單理解，從 PySpark client 發送請求到拿到結果的完整流程。

---

## 1. gRPC Requests

**發生什麼**：PySpark client 透過 Spark Connect protocol 發送請求

**範例**：
```python
# PySpark code
spark = SparkSession.builder.remote("sc://localhost:50051").getOrCreate()
df = spark.sql("SELECT * FROM users WHERE age > 18")
df.show()
```

**傳輸格式**：
- Protocol: gRPC (Spark Connect Protocol)
- Request 包含：Spark Logical Plan (protobuf 格式)

### 🔸 ExecutePlanRequest 結構

完整的 gRPC request 內容（定義在 `crates/sail-spark-connect/proto/spark/connect/base.proto:293`）：

```rust
ExecutePlanRequest {
    session_id: "abc123",           // UUID 格式的 session 標識符
    user_context: Some(UserContext {
        user_id: "user1",            // 用戶 ID
    }),
    operation_id: Some("op456"),     // (可選) 操作 ID
    plan: Some(Plan {                // 要執行的邏輯計劃
        op_type: Some(Root(Relation {
            rel_type: Some(Sql(Sql {
                query: "SELECT * FROM users WHERE age > 18",
            }))
        }))
    }),
    tags: [],                        // (可選) 標籤
    request_options: [],             // (可選) 請求選項
}
```

**結構解析**：

1. **Plan** (`base.proto:38`)
```protobuf
message Plan {
  oneof op_type {
    Relation root = 1;      // 查詢計劃
    Command command = 2;    // 命令（如 createTable, dropTable）
  }
}
```

2. **Relation** (`relations.proto:37`)
```protobuf
message Relation {
  RelationCommon common = 1;
  oneof rel_type {
    Read read = 2;          // 讀取操作：spark.read.parquet()
    Project project = 3;    // 投影：select(col1, col2)
    Filter filter = 4;      // 過濾：filter(col > 10)
    Join join = 5;          // 連接：df1.join(df2)
    SetOperation set_op = 6;  // 集合操作：union, intersect
    Sort sort = 7;          // 排序：orderBy(col)
    Limit limit = 8;        // 限制：limit(10)
    Aggregate aggregate = 9;  // 聚合：groupBy().agg()
    SQL sql = 10;           // SQL 查詢
    // ... 還有 40+ 種 relation 類型
  }
}
```

3. **SQL** (當 rel_type 是 SQL 時)
```protobuf
message SQL {
  string query = 1;         // SQL 查詢字串
  optional Relation input = 2;  // (可選) 輸入關係
}
```

**範例場景對應**：

| PySpark 代碼 | rel_type 值 |
|-------------|------------|
| `spark.sql("SELECT ...")` | Sql |
| `df.filter(col("age") > 18)` | Filter |
| `df.select("name", "age")` | Project |
| `df.groupBy("city").count()` | Aggregate |
| `df.orderBy("age")` | Sort |
| `df1.join(df2, "id")` | Join |

**位置**：客戶端 → Sail Server

---

## 2. Spark Connect Server

**發生什麼**：Sail 的 gRPC server 接收並解析請求

**對應程式碼**：
- `crates/sail-spark-connect/src/server.rs:54` - `SparkConnectServer::execute_plan`
- 實作 Spark Connect 的 gRPC service

### 🔸 完整調用鏈：execute_plan

**入口點**：`SparkConnectServer::execute_plan`

```rust
#[tonic::async_trait]
impl SparkConnectService for SparkConnectServer {
    async fn execute_plan(
        &self,
        request: Request<ExecutePlanRequest>,
    ) -> Result<Response<Self::ExecutePlanStream>, Status> {
        // 1. 提取 request
        let request = request.into_inner();

        // 2. 提取 session 資訊
        let session_key = SessionKey {
            user_id: request.user_context.map(|u| u.user_id).unwrap_or_default(),
            session_id: request.session_id,
        };

        // 3. 提取 metadata（operation_id, tags, reattachable）
        let metadata = ExecutorMetadata { ... };

        // 4. 取得或創建 SessionContext
        let ctx = self
            .session_manager
            .get_or_create_session_context(session_key)
            .await?;

        // 5. 提取 Plan
        let Plan { op_type: op } = request.plan.required("plan")?;
        let op = op.required("plan op")?;

        // 6. 根據 Plan 類型分派
        let stream = match op {
            plan::OpType::Root(relation) => {
                // 查詢：SELECT, FROM, WHERE 等
                service::handle_execute_relation(&ctx, relation, metadata).await?
            }
            plan::OpType::Command(Command { command_type }) => {
                // 命令：RegisterFunction, WriteOperation 等
                // ...
            }
        };

        Ok(Response::new(stream))
    }
}
```

### 🔸 輸入範例

對於查詢 `SELECT * FROM users WHERE age > 18`，接收到的 request：

```rust
ExecutePlanRequest {
    session_id: "abc123",
    user_context: Some(UserContext {
        user_id: "user1",
    }),
    operation_id: Some("op456"),
    plan: Some(Plan {
        op_type: Some(Root(Relation {
            rel_type: Some(Sql(Sql {
                query: "SELECT * FROM users WHERE age > 18",
            }))
        }))
    }),
    tags: [],
    request_options: [],
}
```

### 🔸 調用鏈

```
SparkConnectServer::execute_plan [server.rs:54]
  ↓
service::handle_execute_relation [plan_executor.rs:147]
  ↓
Relation::try_into() → spec::Plan [proto/plan.rs]
  ↓
handle_execute_plan [plan_executor.rs:91]
  ↓
resolve_and_execute_plan [plan_executor.rs:41]
```

### 🔸 Debug 輸出範例

```rust
// 在 execute_plan 開頭加入
println!("=== 1. 接收 gRPC Request ===");
println!("Session ID: {}", request.session_id);
println!("Plan: {:#?}", request.plan);
```

輸出：
```
=== 1. 接收 gRPC Request ===
Session ID: abc123
Plan: Plan {
    op_type: Some(Root(Relation {
        rel_type: Some(Sql(Sql {
            query: "SELECT * FROM users WHERE age > 18"
        }))
    }))
}
```

**輸入**：gRPC ExecutePlanRequest (protobuf)
**輸出**：開始處理 Relation

---

## 3. gRPC -> Spark Plan

**發生什麼**：將 protobuf 格式的 Spark plan 解析成 Sail 內部的 Spark Plan 結構

**對應程式碼**：
- `crates/sail-spark-connect/src/service/plan_executor.rs:147` - `handle_execute_relation`
- `crates/sail-spark-connect/src/proto/plan.rs` - `impl TryFrom<Relation> for spec::Plan`

### 🔸 步驟 1：處理 Relation

```rust
pub(crate) async fn handle_execute_relation(
    ctx: &SessionContext,
    relation: Relation,
    metadata: ExecutorMetadata,
) -> SparkResult<ExecutePlanResponseStream> {
    // 將 protobuf Relation 轉換成 Sail 的 spec::Plan
    let plan = relation.try_into()?;

    // 執行 plan
    handle_execute_plan(ctx, plan, metadata, ExecutePlanMode::Lazy).await
}
```

**輸入 Relation**：
```rust
Relation {
    common: None,
    rel_type: Some(Sql(Sql {
        query: "SELECT * FROM users WHERE age > 18",
        args: {},
        pos_args: [],
    }))
}
```

### 🔸 步驟 2：Protobuf → Sail Plan 轉換

**位置**：`crates/sail-spark-connect/src/proto/plan.rs`

```rust
impl TryFrom<Relation> for spec::Plan {
    type Error = SparkError;

    fn try_from(relation: Relation) -> Result<Self, Self::Error> {
        let rel_type = relation.rel_type.required("relation type")?;

        match rel_type {
            RelType::Sql(sql) => {
                // 轉換 SQL 查詢
                Ok(spec::Plan::Query(spec::QueryPlan::new(
                    spec::QueryNode::Sql(spec::SqlNode {
                        query: sql.query,
                        args: sql.args,
                        pos_args: sql.pos_args,
                    })
                )))
            }
            RelType::Read(read) => {
                // 轉換 Read (table scan)
                Ok(spec::Plan::Query(spec::QueryPlan::new(
                    spec::QueryNode::Read(read.try_into()?)
                )))
            }
            RelType::Project(project) => {
                // 轉換 Projection
                // ...
            }
            RelType::Filter(filter) => {
                // 轉換 Filter
                // ...
            }
            RelType::Join(join) => {
                // 轉換 Join
                // ...
            }
            RelType::Aggregate(agg) => {
                // 轉換 Aggregate
                // ...
            }
            // ... 更多類型
        }
    }
}
```

### 🔸 輸出：spec::Plan

對於我們的查詢，轉換結果：

```rust
spec::Plan::Query(QueryPlan {
    node: QueryNode::Sql(SqlNode {
        query: "SELECT * FROM users WHERE age > 18",
        args: {},
        pos_args: [],
    })
})
```

### 🔸 Relation 的各種類型

| RelType | 用途 | 範例 |
|---------|------|------|
| `Sql` | SQL 查詢 | `SELECT * FROM users` |
| `Read` | 讀取 table | `df = spark.read.parquet("file.parquet")` |
| `Project` | 選擇欄位 | `df.select("name", "age")` |
| `Filter` | 過濾 | `df.filter("age > 18")` |
| `Join` | Join 操作 | `df1.join(df2, "id")` |
| `Aggregate` | 聚合 | `df.groupBy("dept").count()` |
| `Sort` | 排序 | `df.orderBy("age")` |
| `Limit` | 限制行數 | `df.limit(10)` |

### 🔸 Debug 輸出範例

```rust
// 在 try_into() 內加入
println!("=== 2. Relation → spec::Plan ===");
println!("Input relation type: {:?}", relation.rel_type);
let plan = // ... 轉換邏輯
println!("Output plan: {:#?}", plan);
```

輸出：
```
=== 2. Relation → spec::Plan ===
Input relation type: Some(Sql(Sql {
    query: "SELECT * FROM users WHERE age > 18"
}))
Output plan: Query(QueryPlan {
    node: Sql(SqlNode {
        query: "SELECT * FROM users WHERE age > 18"
    })
})
```

**輸入**：protobuf Relation
**輸出**：spec::Plan (Sail 內部格式)

---

## 4. Spark Plan -> Sail Plan

**發生什麼**：將 Spark 的 logical plan 轉換成 Sail 自己的 logical plan

**對應程式碼**：
- `crates/sail-plan/src/resolver/` - Plan 解析和轉換
- `crates/sail-plan/src/resolver/plan.rs` - 主要的 plan resolver

**主要工作**：
- **Relation 轉換**：Spark Relation → Sail Relation
  - Scan → 資料來源讀取
  - Join → join 邏輯
  - Aggregate → 聚合邏輯
  - Filter → 過濾條件

- **Expression 轉換**：Spark Expression → Sail Expression
  - Column references
  - Functions (Spark functions → DataFusion functions)
  - Literals
  - Operators

- **函數對應**：
  - Spark 函數名稱 → DataFusion 函數
  - 例如：`array_insert` → 自訂實作
  - 位置：`crates/sail-plan/src/function/scalar/`

**輸入**：Spark Plan
**輸出**：Sail Plan (仍是 logical plan，但已經是 Sail 的格式)

---

## 5. Sail Plan -> DataFusion Logical Plan

**發生什麼**：將 Sail 的 logical plan 轉換成 DataFusion 的 LogicalPlan

**對應程式碼**：
- `crates/sail-plan/src/` - Plan 轉換邏輯
- DataFusion 的 `LogicalPlan` 類型

**主要工作**：
- 轉換成 DataFusion 的標準 LogicalPlan
- 映射資料來源 (Parquet, Delta Lake, Iceberg)
- 映射所有 operators (Projection, Filter, Join, Aggregate)
- 確保函數都有對應的實作

**關鍵概念**：
```
Spark SQL: SELECT dept, COUNT(*) FROM users GROUP BY dept
    ↓
Sail Logical Plan: 處理 Spark 語義
    ↓
DataFusion Logical Plan:
  - Aggregate [dept], [COUNT(*)]
    - TableScan [users]
```

**輸入**：Sail Plan
**輸出**：DataFusion LogicalPlan

---

## 6. DataFusion Logical Plan -> Physical Plan

**發生什麼**：DataFusion 的 query optimizer 將 logical plan 轉換成 physical plan

**對應程式碼**：
- DataFusion 內建的 optimizer 和 planner
- `crates/sail-execution/` - Sail 的執行層

**主要工作**：

🔸 **Optimization (優化)**
- Predicate pushdown：將 filter 往下推
- Projection pushdown：只讀取需要的欄位
- Join reordering：調整 join 順序
- Constant folding：計算常數表達式

🔸 **Partitioning (分區)**
- 決定資料如何分區
- 決定平行度 (parallelism)

🔸 **Physical Operators**
- LogicalPlan operators → PhysicalPlan operators
- 例如：
  - `Filter` → `FilterExec`
  - `Aggregate` → `AggregateExec`
  - `Join` → `HashJoinExec` 或 `SortMergeJoinExec`
  - `Scan` → `ParquetExec` / `DeltaScanExec`

**範例轉換**：
```
Logical Plan:
  Aggregate [dept], [COUNT(*)]
    Filter [age > 18]
      TableScan [users]

Physical Plan:
  AggregateExec (final)
    CoalescePartitionsExec
      AggregateExec (partial) [4 partitions]
        FilterExec [age > 18]
          ParquetExec [users.parquet] [4 partitions]
```

**輸入**：DataFusion LogicalPlan
**輸出**：DataFusion ExecutionPlan (Physical Plan)

---

## 6.5 Sail 的額外處理：JobGraph 建構（分散式執行專用）

**發生什麼**：Sail 在 Physical Plan 生成後，插入 Shuffle 機制來支援分散式執行

**對應程式碼**：
- `crates/sail-execution/src/driver/planner.rs` - JobGraph 建構
- `crates/sail-execution/src/plan/shuffle_write.rs` - ShuffleWriteExec
- `crates/sail-execution/src/plan/shuffle_read.rs` - ShuffleReadExec

**時機**：
- 只在 **Cluster Mode** 才需要
- Local Mode 直接執行 Physical Plan
- Physical Plan 生成後、執行前

### 🔸 核心工作：切分 Stages

**JobGraph 做什麼**：
```rust
pub struct JobGraph {
    stages: Vec<Arc<dyn ExecutionPlan>>,  // 將 Physical Plan 切分成多個 Stage
}
```

1. **遍歷 Physical Plan**
2. **識別 Shuffle 邊界**：
   - `RepartitionExec` (hash/range 重新分區)
   - `CoalescePartitionsExec` (合併 partitions)
3. **插入 Shuffle Operators**：
   - `ShuffleWriteExec` - 寫入 shuffle 資料
   - `ShuffleReadExec` - 讀取 shuffle 資料

### 🔸 轉換範例

**DataFusion 原始 Physical Plan**：
```
AggregateExec (final)
  CoalescePartitionsExec        ← Shuffle 邊界
    AggregateExec (partial) [4 partitions]
      FilterExec [age > 18]
        ParquetExec [4 partitions]
```

**Sail 的 JobGraph 切分後**：
```
Stage 1: (最終 Stage)
  AggregateExec (final)
    ShuffleReadExec (stage=0, partitions=4)
      ↑
      | 從 Stage 0 讀取 shuffle 資料
      |

─────── Shuffle 邊界 ───────

Stage 0:
  ShuffleWriteExec (stage=0, partitions=4)
    ↓ 寫入 shuffle storage
    AggregateExec (partial) [4 partitions]
      FilterExec [age > 18]
        ParquetExec [4 partitions]
```

### 🔸 ShuffleWriteExec

**功能**：
- 執行 child plan (例如 `AggregateExec (partial)`)
- 根據 hash(dept) 重新分區資料
- 將資料寫入 shuffle storage
- 供下一個 Stage 讀取

**關鍵欄位**：
```rust
pub struct ShuffleWriteExec {
    stage: usize,                           // 屬於哪個 Stage
    plan: Arc<dyn ExecutionPlan>,           // 要執行的 child plan
    shuffle_partitioning: Partitioning,     // 如何分區 (例如 Hash(dept, 4))
    locations: Vec<Vec<TaskWriteLocation>>, // 每個 partition 寫入哪裡
}
```

### 🔸 ShuffleReadExec

**功能**：
- 從 shuffle storage 讀取資料
- 從前一個 Stage 的多個 Tasks 讀取
- 合併成一個 RecordBatchStream

**關鍵欄位**：
```rust
pub struct ShuffleReadExec {
    stage: usize,                          // 從哪個 Stage 讀取
    locations: Vec<Vec<TaskReadLocation>>, // 從哪些位置讀取
}
```

### 🔸 執行流程

**Stage 0 (在 4 個 Workers 上並行執行)**：
```
Task 0: ParquetExec [partition 0]
        → FilterExec
        → AggregateExec (partial)
        → ShuffleWriteExec
          → hash(dept) → 寫入 4 個輸出 partitions

Task 1: ParquetExec [partition 1] → ... → ShuffleWriteExec
Task 2: ParquetExec [partition 2] → ... → ShuffleWriteExec
Task 3: ParquetExec [partition 3] → ... → ShuffleWriteExec
```

**Shuffle Storage 組織**：
```
Output Partition 0: {dept="IT" 的所有資料}
Output Partition 1: {dept="HR" 的所有資料}
Output Partition 2: {dept="Ops" 的所有資料}
Output Partition 3: {其他 dept 的資料}
```

**Stage 1 (讀取並完成聚合)**：
```
Task 0: ShuffleReadExec [讀取 partition 0 from all Stage 0 tasks]
        → AggregateExec (final)
        → 返回結果
```

### 🔸 為什麼需要 Shuffle？

**問題**：分散式環境中的 GROUP BY

```
Worker 1 有: [("IT", 50), ("HR", 20)]
Worker 2 有: [("IT", 50), ("HR", 30)]  ← 相同 dept 的資料分散在不同機器
```

**解決**：Shuffle 重新分區

```
Shuffle 後:
Worker 1 負責: 所有 "IT" 的資料   → COUNT = 100
Worker 2 負責: 所有 "HR" 的資料   → COUNT = 50
```

### 🔸 對比：Local Mode vs Cluster Mode

| 項目 | Local Mode | Cluster Mode (JobGraph) |
|------|-----------|------------------------|
| Physical Plan | 直接執行 | 插入 ShuffleWrite/ReadExec |
| Partitions | 記憶體內傳遞 | 透過 shuffle storage |
| 執行單位 | Thread | Task (分散在多個 Workers) |
| 協調 | 不需要 | DriverActor 協調 |
| Shuffle | CoalescePartitionsExec | ShuffleWrite + ShuffleRead |

### 🔸 總結

**Sail 的創新之處**：
1. **不修改 DataFusion optimizer**：保留所有優化能力
2. **在 Physical Plan 後加入 Shuffle**：支援分散式執行
3. **透明切分**：自動識別 Shuffle 邊界，切分成 Stages
4. **統一介面**：Local 和 Cluster 用相同的 API

**關鍵概念**：
- **Stage**：可以獨立執行的一組 operators
- **Shuffle**：Stage 之間的資料重新分區和傳輸
- **Task**：Stage 中每個 partition 的執行單元

**輸入**：DataFusion ExecutionPlan
**輸出**：JobGraph (多個 Stages，包含 Shuffle operators)

---

## 7. Physical Plan -> Execution

**發生什麼**：實際執行 physical plan，讀取資料並計算結果

**執行模式**：

### 🔸 Local Mode (本地執行)

**對應程式碼**：
- `crates/sail-execution/src/local/` - Local execution

**流程**：
```
ExecutionPlan.execute(partition)
  ↓
返回 RecordBatchStream
  ↓
逐批次 (batch) 處理資料
  ↓
每個 batch 是一個 RecordBatch (Arrow 格式)
```

### 🔸 Cluster Mode (分散式執行)

**對應程式碼**：
- `crates/sail-execution/src/driver/` - Driver (協調者)
- `crates/sail-execution/src/worker/` - Worker (執行者)

**流程**：
```
1. ClusterJobRunner 接收 ExecutionPlan
   ↓
2. DriverActor 建構 JobGraph
   - 分析 Shuffle 邊界
   - 切分成 Stages
   ↓
3. DriverActor 創建 Tasks
   - 每個 Stage 的每個 partition = 1 個 Task
   ↓
4. DriverActor 調度 Tasks 到 Workers
   - 透過 gRPC 發送 RunTask
   ↓
5. Workers 執行 Tasks
   - 讀取資料 (Parquet, Delta, Iceberg)
   - 執行計算 (filter, map, aggregate)
   - 寫入 Shuffle data (如果需要)
   ↓
6. DriverActor 收集結果
   - 從最終 Stage 的 Tasks 讀取結果
```

**關鍵機制**：

🔸 **Shuffle**
- 當需要重新分區時發生
- 例如：GROUP BY, JOIN
- Stage 之間的邊界
- 實作：
  - `ShuffleWriteExec` - 寫入 shuffle 資料
  - `ShuffleReadExec` - 讀取 shuffle 資料
  - 位置：`crates/sail-execution/src/shuffle/`

🔸 **Task 狀態**
```
Created → Pending → Scheduled → Running → Succeeded
                                       → Failed (重試)
```

🔸 **資料格式**
- Apache Arrow：記憶體中的列式格式
- RecordBatch：一批資料記錄
- Schema：資料的結構定義

**輸入**：ExecutionPlan
**輸出**：RecordBatchStream (資料流)

---

## 8. Execution -> Results

**發生什麼**：將執行結果返回給 client

**對應程式碼**：
- `crates/sail-spark-connect/src/service.rs` - 回傳結果給 client

**流程**：

### 🔸 結果收集

```
RecordBatchStream
  ↓
逐批次讀取 RecordBatch
  ↓
轉換成 Spark Connect 格式 (protobuf)
  ↓
透過 gRPC stream 回傳給 client
```

### 🔸 資料轉換

```
Arrow RecordBatch (Sail 內部)
  ↓
Arrow IPC format (序列化)
  ↓
Spark Connect ExecutePlanResponse (protobuf)
  ↓
gRPC stream
  ↓
PySpark client 接收並解析
```

### 🔸 串流處理

- 結果是**串流**的，不是一次性全部回傳
- 每個 batch 大約 8192 rows (可配置)
- Client 可以邊接收邊處理

**範例輸出**：
```
+----+-----+
|dept|count|
+----+-----+
|  IT|  100|
|  HR|   50|
| Ops|   75|
+----+-----+
```

**輸入**：RecordBatchStream
**輸出**：gRPC response stream → PySpark DataFrame

---

## 完整流程圖

```
PySpark Client
    |
    | 1. gRPC Request (Spark Connect Protocol)
    v
SparkConnectService (Sail gRPC Server)
    |
    | 2. Parse protobuf
    v
Spark Plan (Spark logical plan 結構)
    |
    | 3. Spark → Sail 轉換
    | (crates/sail-plan/src/resolver/)
    v
Sail Plan (Sail 內部的 logical plan)
    |
    | 4. Sail → DataFusion 轉換
    | (crates/sail-plan/src/)
    v
DataFusion LogicalPlan
    |
    | 5. Optimization + Planning
    | (DataFusion optimizer)
    v
DataFusion ExecutionPlan (Physical Plan)
    |
    | 6. Execution
    |
    +---> Local Mode: 直接執行
    |     (crates/sail-execution/src/local/)
    |
    +---> Cluster Mode: 分散式執行
          (crates/sail-execution/src/driver/ + worker/)
          |
          | a. JobGraph 切分 Stages
          | b. 創建 Tasks
          | c. 調度到 Workers
          | d. Workers 執行
          | e. Shuffle (如果需要)
          | f. 收集結果
          v
RecordBatchStream (Arrow format)
    |
    | 7. Convert to Spark Connect format
    | (crates/sail-spark-connect/src/)
    v
gRPC Response Stream
    |
    | 8. Stream back to client
    v
PySpark Client (DataFrame)
```

---

## 關鍵 Crates 對應

| Crate | 負責流程 | 說明 |
|-------|---------|------|
| `sail-spark-connect` | 步驟 1, 2, 3, 8 | gRPC server, protobuf 解析, 結果回傳 |
| `sail-plan` | 步驟 4, 5 | Spark → Sail → DataFusion 轉換 |
| `sail-sql-parser` | (SQL 情況) | 解析 Spark SQL |
| `sail-execution` | 步驟 7 | Local/Cluster 執行 |
| `sail-delta-lake` | 步驟 7 | Delta Lake 資料來源 |
| `sail-iceberg` | 步驟 7 | Iceberg 資料來源 |
| `sail-python-udf` | 步驟 7 | Python UDF 支援 |

---

## 資料格式轉換

```
PySpark DataFrame
    ↓ (序列化)
Protobuf (Spark Connect Plan)
    ↓ (解析)
Spark Plan (Rust struct)
    ↓ (轉換)
Sail Plan (Rust struct)
    ↓ (轉換)
DataFusion LogicalPlan
    ↓ (優化 + 規劃)
DataFusion ExecutionPlan
    ↓ (執行)
Arrow RecordBatch (columnar data)
    ↓ (序列化)
Arrow IPC format
    ↓ (包裝)
Protobuf (ExecutePlanResponse)
    ↓ (gRPC stream)
PySpark DataFrame
```

---

## 重要概念

### 🔸 Logical Plan vs Physical Plan

**Logical Plan**：
- 描述「做什麼」(WHAT)
- 與具體執行無關
- 可以優化

**Physical Plan**：
- 描述「怎麼做」(HOW)
- 具體的執行策略
- 包含分區、平行度等細節

### 🔸 Spark 語義 vs DataFusion 語義

**為什麼需要 Sail Plan**：
- Spark 和 DataFusion 的函數行為不同
- Spark 的 array 是 1-based，DataFusion 是 0-based
- Spark 的 null 處理邏輯不同
- 需要轉換層來保證相容性

### 🔸 Pull-based vs Push-based Execution

**DataFusion 使用 Pull-based (Volcano model)**：
- Parent operator 向 child operator 拉取資料
- 逐批次處理 (streaming execution)
- 記憶體效率高

### 🔸 Columnar Format (列式儲存)

**Apache Arrow**：
- 記憶體中的列式格式
- CPU cache friendly
- 支援 SIMD 運算
- 零拷貝跨語言傳遞

---

## 範例：一個完整的查詢

**PySpark Code**：
```python
df = spark.sql("SELECT dept, COUNT(*) as cnt FROM users WHERE age > 18 GROUP BY dept")
df.show()
```

**流程追蹤**：

1. **gRPC Request**: 包含 SQL string
2. **Spark Plan**:
   - Filter(age > 18)
   - Aggregate(groupBy=[dept], agg=[COUNT(*)])
3. **Sail Plan**: 轉換 Spark 語義
4. **DataFusion Logical**:
   - Aggregate[dept], [COUNT(*)]
     - Filter[age > 18]
       - TableScan[users]
5. **DataFusion Physical**:
   - AggregateExec(final)
     - CoalescePartitionsExec
       - AggregateExec(partial) [4 partitions]
         - FilterExec[age > 18]
           - ParquetExec[users.parquet] [4 partitions]
6. **Execution**:
   - 4 個 tasks 讀取 Parquet 並做 partial aggregation
   - Shuffle
   - 1 個 task 做 final aggregation
7. **Results**:
   - RecordBatch 1: [("IT", 100), ("HR", 50)]
   - RecordBatch 2: [("Ops", 75)]
8. **gRPC Response**: 串流回傳給 client

---

## 總結

Sail 的核心價值在於：

1. **相容性**：完整支援 Spark Connect protocol
2. **效能**：使用 Rust + DataFusion 獲得更好的效能
3. **透明性**：PySpark code 不需要修改
4. **擴展性**：支援 local 和 cluster mode
5. **整合性**：支援 Delta Lake, Iceberg 等 lakehouse formats

整個流程的關鍵就是**多層轉換**：
- Spark → Sail → DataFusion
- 每一層都負責特定的語義轉換
- 最終由 DataFusion 執行，利用 Rust 的效能優勢
