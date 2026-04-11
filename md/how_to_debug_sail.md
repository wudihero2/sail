# 如何 Debug Sail：追蹤一個查詢的完整流程

本文教你如何追蹤 `SELECT * FROM users WHERE age > 18` 在 Sail 中每個階段的變化。

---

## 準備工作

### 1. 啟動 Sail Server

```bash
# 在 Sail 專案目錄
cargo build --release

# 啟動 server（帶 debug log）
RUST_LOG=debug ./target/release/sail spark server --port 50051
```

### 2. 準備測試資料

```python
# create_test_data.py
from pyspark.sql import SparkSession

spark = SparkSession.builder.remote("sc://localhost:50051").getOrCreate()

# 創建測試資料
data = [
    ("Alice", 25, "IT"),
    ("Bob", 17, "HR"),
    ("Charlie", 30, "IT"),
    ("David", 16, "Ops"),
    ("Eve", 28, "HR"),
]

df = spark.createDataFrame(data, ["name", "age", "dept"])
df.write.mode("overwrite").parquet("/tmp/users.parquet")
```

### 3. 設定 Log Level

在專案根目錄創建 `.env` 或設定環境變數：

```bash
# 啟用詳細日誌
export RUST_LOG=debug
export RUST_LOG=sail_spark_connect=debug,sail_plan=debug,sail_execution=debug

# 或者只看特定模組
export RUST_LOG=sail_plan::resolver=trace
```

---

## 階段 1：gRPC Request → Spark Plan

### 如何觀察

**方法 1：啟用 gRPC logging**

在 `crates/sail-spark-connect/src/service.rs` 中加入：

```rust
pub async fn execute_plan(&self, request: ExecutePlanRequest) -> Result<...> {
    // 加入這行
    println!("=== 收到 gRPC Request ===");
    println!("{:#?}", request);

    // 原本的程式碼...
}
```

**方法 2：使用 Rust log**

```rust
use tracing::debug;

debug!("收到 ExecutePlanRequest: {:?}", request);
```

**看到什麼**：

```
=== 收到 gRPC Request ===
ExecutePlanRequest {
    session_id: "...",
    plan: Some(Plan {
        op_type: Some(Root(Relation {
            rel_type: Some(Sql(Sql {
                query: "SELECT * FROM users WHERE age > 18",
            }))
        }))
    })
}
```

---

## 階段 2：Spark Plan 解析

### 如何觀察

在 `crates/sail-spark-connect/src/proto/plan.rs` 中：

```rust
pub fn to_logical_plan(&self, ...) -> Result<LogicalPlan> {
    // 加入這行
    println!("=== 解析 Spark Plan ===");
    println!("Plan type: {:?}", self);

    // 原本的程式碼...
}
```

**或使用 DataFusion 的 Display**：

```rust
use datafusion::logical_expr::LogicalPlan;

println!("=== Spark Logical Plan ===");
println!("{}", plan.display_indent());
```

**看到什麼**：

```
=== Spark Logical Plan ===
Sql {
    query: "SELECT * FROM users WHERE age > 18"
}
```

---

## 階段 3：Spark Plan → Sail Plan

### 如何觀察

在 `crates/sail-plan/src/resolver/plan.rs` 中：

```rust
pub fn resolve_relation(&mut self, relation: Relation) -> PlanResult<LogicalPlan> {
    println!("=== 轉換 Spark Relation ===");
    println!("Relation: {:?}", relation);

    let result = // ... 轉換邏輯

    println!("=== Sail Logical Plan ===");
    println!("{}", result.display_indent());

    result
}
```

**看到什麼**：

```
=== 轉換 Spark Relation ===
Relation: Sql { query: "SELECT * FROM users WHERE age > 18" }

=== Sail Logical Plan ===
Projection: name, age, dept
  Filter: age > 18
    TableScan: users
```

---

## 階段 4：SQL 解析（如果是 SQL 查詢）

### 如何觀察

在 `crates/sail-sql-parser/src/parser.rs` 中：

```rust
pub fn parse_sql(&self, sql: &str) -> Result<Statement> {
    println!("=== 解析 SQL ===");
    println!("SQL: {}", sql);

    let stmt = // ... 解析邏輯

    println!("=== AST ===");
    println!("{:#?}", stmt);

    stmt
}
```

**看到什麼**：

```
=== 解析 SQL ===
SQL: SELECT * FROM users WHERE age > 18

=== AST ===
Query {
    body: Select {
        projection: [Wildcard],
        from: [Table { name: "users" }],
        selection: Some(BinaryOp {
            left: Column("age"),
            op: Gt,
            right: Literal(18)
        })
    }
}
```

---

## 階段 5：Sail Plan → DataFusion Logical Plan

### 如何觀察

在 `crates/sail-plan/src/resolver/plan.rs` 的最後：

```rust
pub fn resolve_plan(&mut self, plan: Plan) -> PlanResult<LogicalPlan> {
    let logical_plan = // ... 轉換邏輯

    println!("=== DataFusion Logical Plan ===");
    println!("{}", logical_plan.display_indent());
    println!("\n=== Schema ===");
    println!("{:#?}", logical_plan.schema());

    logical_plan
}
```

**看到什麼**：

```
=== DataFusion Logical Plan ===
Projection: name, age, dept
  Filter: age > Int64(18)
    TableScan: users projection=[name, age, dept]

=== Schema ===
Schema {
    fields: [
        Field { name: "name", data_type: Utf8, nullable: true },
        Field { name: "age", data_type: Int64, nullable: true },
        Field { name: "dept", data_type: Utf8, nullable: true },
    ]
}
```

---

## 階段 6：Logical Plan → Physical Plan

### 如何觀察

在 `crates/sail-execution/src/job/runner.rs` 中：

```rust
pub async fn execute(&self, plan: LogicalPlan) -> Result<RecordBatchStream> {
    println!("=== 開始優化 ===");
    println!("{}", plan.display_indent());

    // DataFusion optimizer
    let optimized = self.optimize(plan)?;

    println!("=== 優化後的 Logical Plan ===");
    println!("{}", optimized.display_indent());

    // 轉換成 Physical Plan
    let physical_plan = self.create_physical_plan(optimized).await?;

    println!("=== Physical Plan ===");
    println!("{}", DisplayableExecutionPlan::new(physical_plan.as_ref()).indent(true));

    // ...
}
```

**看到什麼**：

```
=== 開始優化 ===
Projection: name, age, dept
  Filter: age > Int64(18)
    TableScan: users projection=[name, age, dept]

=== 優化後的 Logical Plan ===
Projection: name, age, dept
  Filter: age > Int64(18)
    TableScan: users projection=[name, age, dept]  ← predicate pushdown 已完成

=== Physical Plan ===
ProjectionExec: expr=[name@0 as name, age@1 as age, dept@2 as dept]
  FilterExec: age@1 > 18
    ParquetExec: file=/tmp/users.parquet, projection=[name, age, dept]
```

---

## 階段 6.5：JobGraph 建構（Cluster Mode）

### 如何觀察

在 `crates/sail-execution/src/driver/planner.rs` 中：

```rust
impl JobGraph {
    pub fn try_new(plan: Arc<dyn ExecutionPlan>) -> ExecutionResult<Self> {
        println!("=== 建構 JobGraph ===");
        println!("=== 原始 Physical Plan ===");
        println!("{}", DisplayableExecutionPlan::new(plan.as_ref()).indent(true));

        let mut graph = Self { stages: vec![] };
        let last = build_job_graph(plan, &mut graph)?;
        graph.stages.push(last);

        println!("=== JobGraph 結果 ===");
        println!("{}", graph);  // 使用 Display trait

        Ok(graph)
    }
}
```

**看到什麼**（如果有 GROUP BY）：

```
=== 建構 JobGraph ===
=== 原始 Physical Plan ===
AggregateExec: mode=Final, gby=[dept@0]
  CoalescePartitionsExec
    AggregateExec: mode=Partial, gby=[dept@2]
      FilterExec: age@1 > 18
        ParquetExec: file=/tmp/users.parquet

=== JobGraph 結果 ===
=== stage 0 ===
ShuffleWriteExec: stage=0, partitioning=Hash([dept], 4)
  AggregateExec: mode=Partial, gby=[dept@2]
    FilterExec: age@1 > 18
      ParquetExec: file=/tmp/users.parquet

=== stage 1 ===
AggregateExec: mode=Final, gby=[dept@0]
  ShuffleReadExec: stage=0, partitioning=Hash([dept], 4)
```

---

## 階段 7：執行 Physical Plan

### 如何觀察（Local Mode）

在 `crates/sail-execution/src/local/runner.rs` 中：

```rust
pub async fn execute(&self, plan: Arc<dyn ExecutionPlan>) -> Result<RecordBatchStream> {
    println!("=== 開始執行 (Local Mode) ===");
    println!("Partition count: {}", plan.output_partitioning().partition_count());

    let stream = plan.execute(0, task_context)?;

    // 包裝 stream 來觀察資料
    let debug_stream = stream.map(|batch_result| {
        if let Ok(batch) = &batch_result {
            println!("=== RecordBatch ===");
            println!("Rows: {}", batch.num_rows());
            println!("Columns: {}", batch.num_columns());
            println!("{:?}", batch);
        }
        batch_result
    });

    Ok(debug_stream)
}
```

**看到什麼**：

```
=== 開始執行 (Local Mode) ===
Partition count: 1

=== RecordBatch ===
Rows: 3
Columns: 3
+--------+-----+------+
| name   | age | dept |
+--------+-----+------+
| Alice  | 25  | IT   |
| Charlie| 30  | IT   |
| Eve    | 28  | HR   |
+--------+-----+------+
```

### 如何觀察（Cluster Mode）

在 `crates/sail-execution/src/driver/actor/handler.rs` 中：

```rust
pub(super) fn handle_execute_job(...) -> ActorAction {
    println!("=== DriverActor 接收 Job ===");

    match self.accept_job(ctx, plan) {
        Ok(job_id) => {
            println!("=== Job ID: {} ===", job_id);
            println!("=== 開始調度 Tasks ===");

            self.schedule_tasks(ctx);
        }
        // ...
    }
}
```

在 `crates/sail-execution/src/driver/actor/handler.rs` 的 `accept_job` 中：

```rust
fn accept_job(...) -> ExecutionResult<JobId> {
    let graph = JobGraph::try_new(plan)?;

    println!("=== 創建 Tasks ===");
    for (s, stage) in graph.stages().iter().enumerate() {
        println!("Stage {}: {} partitions", s, stage.output_partitioning().partition_count());

        for p in 0..stage.output_partitioning().partition_count() {
            let task_id = self.state.next_task_id()?;
            println!("  Task {}: stage={}, partition={}", task_id, s, p);
            // ...
        }
    }

    Ok(job_id)
}
```

**看到什麼**：

```
=== DriverActor 接收 Job ===
=== Job ID: 1 ===
=== 創建 Tasks ===
Stage 0: 4 partitions
  Task 1: stage=0, partition=0
  Task 2: stage=0, partition=1
  Task 3: stage=0, partition=2
  Task 4: stage=0, partition=3
Stage 1: 1 partitions
  Task 5: stage=1, partition=0
=== 開始調度 Tasks ===
```

---

## 階段 8：結果收集

### 如何觀察

在 `crates/sail-spark-connect/src/service.rs` 中：

```rust
pub async fn execute_plan(&self, request: ExecutePlanRequest) -> Result<Stream> {
    // ... 執行 plan

    let mut result_stream = execute_result?;

    let mut batch_count = 0;
    let mut total_rows = 0;

    while let Some(batch_result) = result_stream.next().await {
        let batch = batch_result?;
        batch_count += 1;
        total_rows += batch.num_rows();

        println!("=== Batch {} ===", batch_count);
        println!("Rows: {}", batch.num_rows());
        println!("{:?}", batch);

        // 轉換成 protobuf 並回傳
        let response = convert_to_response(batch)?;
        yield response;
    }

    println!("=== 完成 ===");
    println!("Total batches: {}", batch_count);
    println!("Total rows: {}", total_rows);
}
```

**看到什麼**：

```
=== Batch 1 ===
Rows: 3
+--------+-----+------+
| name   | age | dept |
+--------+-----+------+
| Alice  | 25  | IT   |
| Charlie| 30  | IT   |
| Eve    | 28  | HR   |
+--------+-----+------+

=== 完成 ===
Total batches: 1
Total rows: 3
```

---

## 完整的 Debug 腳本

創建 `debug_query.py`：

```python
from pyspark.sql import SparkSession

# 啟用 Spark 的 explain
spark = SparkSession.builder.remote("sc://localhost:50051").getOrCreate()
spark.conf.set("spark.sql.adaptive.enabled", "false")  # 關閉 AQE 以便觀察

# 讀取資料
df = spark.read.parquet("/tmp/users.parquet")

# 執行查詢
result = df.filter("age > 18")

# 顯示執行計畫
print("=== Spark Logical Plan ===")
result.explain(extended=False)

print("\n=== Spark Physical Plan ===")
result.explain(mode="formatted")

# 執行並顯示結果
print("\n=== Results ===")
result.show()

# 更複雜的查詢
print("\n=== Complex Query ===")
grouped = df.filter("age > 18").groupBy("dept").count()
grouped.explain(mode="formatted")
grouped.show()
```

---

## 使用 Rust Debugger

### 設定 VSCode

創建 `.vscode/launch.json`：

```json
{
    "version": "0.2.0",
    "configurations": [
        {
            "type": "lldb",
            "request": "launch",
            "name": "Debug Sail Server",
            "cargo": {
                "args": [
                    "build",
                    "--bin=sail",
                    "--package=sail-cli"
                ],
                "filter": {
                    "name": "sail",
                    "kind": "bin"
                }
            },
            "args": ["spark", "server", "--port", "50051"],
            "cwd": "${workspaceFolder}",
            "env": {
                "RUST_LOG": "debug"
            }
        }
    ]
}
```

### 設定 Breakpoints

在關鍵位置設定中斷點：

1. `crates/sail-spark-connect/src/service.rs` - `execute_plan` 方法
2. `crates/sail-plan/src/resolver/plan.rs` - `resolve_relation` 方法
3. `crates/sail-execution/src/driver/planner.rs` - `JobGraph::try_new`
4. `crates/sail-execution/src/driver/actor/handler.rs` - `handle_execute_job`

---

## 使用 tracing 框架

### 設定 tracing

在 `Cargo.toml` 中（通常已包含）：

```toml
[dependencies]
tracing = "0.1"
tracing-subscriber = "0.3"
```

### 在程式碼中加入 spans

```rust
use tracing::{info, debug, span, Level};

pub fn resolve_plan(&mut self, plan: Plan) -> PlanResult<LogicalPlan> {
    let span = span!(Level::INFO, "resolve_plan");
    let _guard = span.enter();

    info!("開始解析 plan");
    debug!("Plan: {:?}", plan);

    let result = // ... 邏輯

    info!("解析完成");
    debug!("Result: {}", result.display_indent());

    result
}
```

### 啟動時設定

```rust
fn main() {
    // 設定 tracing subscriber
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .with_target(true)
        .with_thread_ids(true)
        .init();

    // ... 啟動 server
}
```

---

## 實用的 Debug 技巧

### 1. 觀察特定模組

```bash
# 只看 plan resolver 的 log
RUST_LOG=sail_plan::resolver=trace cargo run

# 看多個模組
RUST_LOG=sail_plan=debug,sail_execution=debug cargo run
```

### 2. 儲存 Plans 到檔案

```rust
use std::fs::File;
use std::io::Write;

pub fn debug_save_plan(plan: &LogicalPlan, filename: &str) {
    let mut file = File::create(filename).unwrap();
    writeln!(file, "{}", plan.display_indent()).unwrap();
}

// 使用
debug_save_plan(&logical_plan, "/tmp/logical_plan.txt");
debug_save_plan(&physical_plan, "/tmp/physical_plan.txt");
```

### 3. 比較 Plans

```rust
pub fn compare_plans(before: &LogicalPlan, after: &LogicalPlan) {
    println!("=== Before Optimization ===");
    println!("{}", before.display_indent());
    println!("\n=== After Optimization ===");
    println!("{}", after.display_indent());

    // 比較 node 數量
    println!("\nNodes before: {}", count_nodes(before));
    println!("Nodes after: {}", count_nodes(after));
}
```

### 4. 追蹤執行時間

```rust
use std::time::Instant;

pub fn timed_execution<F, R>(name: &str, f: F) -> R
where
    F: FnOnce() -> R,
{
    let start = Instant::now();
    let result = f();
    let elapsed = start.elapsed();
    println!("{} took: {:?}", name, elapsed);
    result
}

// 使用
let physical_plan = timed_execution("Create Physical Plan", || {
    create_physical_plan(logical_plan)
});
```

---

## 總結

**完整 Debug 流程**：

1. **啟動 Server**：`RUST_LOG=debug cargo run`
2. **加入 println!**：在關鍵位置印出中間結果
3. **使用 tracing**：結構化的日誌
4. **設定 Breakpoints**：使用 debugger 單步執行
5. **執行查詢**：從 PySpark 發送查詢
6. **觀察輸出**：追蹤每個階段的轉換

**關鍵觀察點**：

- gRPC Request 的內容
- Spark Plan 的結構
- Logical Plan 的轉換
- Physical Plan 的優化
- JobGraph 的 Stage 切分
- Tasks 的創建和調度
- RecordBatch 的內容

**最佳實踐**：

- 使用 `display_indent()` 來美化 plan 的輸出
- 用 tracing spans 來追蹤執行路徑
- 儲存中間結果到檔案方便比較
- 使用 conditional compilation 來避免影響 production 效能

```rust
#[cfg(debug_assertions)]
println!("Debug info: {:?}", data);
```
