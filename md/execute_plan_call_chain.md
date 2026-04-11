# execute_plan 完整調用鏈解析

本文詳細追蹤 `SELECT * FROM users WHERE age > 18` 在 Sail 中從 gRPC 請求到 Spark Plan 的完整調用鏈。

---

## 完整調用鏈概覽

```
1. PySpark Client 發送 gRPC request
   ↓
2. SparkConnectServer::execute_plan() [server.rs:54]
   ↓
3. service::handle_execute_relation() [plan_executor.rs:147]
   ↓
4. Relation::try_into() → spec::Plan [proto/plan.rs]
   ↓
5. handle_execute_plan() [plan_executor.rs:91]
   ↓
6. resolve_and_execute_plan() [plan_executor.rs:41]
   ↓
7. PlanResolver::resolve_plan() [sail-plan resolver]
   ↓
8. DataFusion LogicalPlan
```

---

## 階段 1：接收 gRPC Request

### 入口點：`execute_plan`

**位置**：`crates/sail-spark-connect/src/server.rs:54-82`

```rust
#[tonic::async_trait]
impl SparkConnectService for SparkConnectServer {
    type ExecutePlanStream = ExecutePlanResponseStream;

    async fn execute_plan(
        &self,
        request: Request<ExecutePlanRequest>,
    ) -> Result<Response<Self::ExecutePlanStream>, Status> {
        // 1. 提取 request
        let request = request.into_inner();
        debug!("{request:?}");

        // 2. 提取 session 資訊
        let session_key = SessionKey {
            user_id: request.user_context.map(|u| u.user_id).unwrap_or_default(),
            session_id: request.session_id,
        };

        // 3. 提取 metadata
        let metadata = ExecutorMetadata {
            operation_id: request.operation_id.unwrap_or_else(|| Uuid::new_v4().to_string()),
            tags: request.tags,
            reattachable: is_reattachable(&request.request_options),
        };

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
                // 執行查詢
                service::handle_execute_relation(&ctx, relation, metadata).await?
            }
            plan::OpType::Command(Command { command_type }) => {
                // 執行命令 (RegisterFunction, WriteOperation 等)
                // ...
            }
        };

        Ok(Response::new(stream))
    }
}
```

### 輸入範例

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

### Debug 輸出

```rust
// 在 execute_plan 開頭加入
println!("=== 1. 接收 gRPC Request ===");
println!("Session ID: {}", request.session_id);
println!("Operation ID: {:?}", request.operation_id);
println!("Plan: {:#?}", request.plan);
```

**輸出**：
```
=== 1. 接收 gRPC Request ===
Session ID: abc123
Operation ID: Some("op456")
Plan: Plan {
    op_type: Some(Root(Relation {
        rel_type: Some(Sql(Sql {
            query: "SELECT * FROM users WHERE age > 18"
        }))
    }))
}
```

---

## 階段 2：處理 Relation

### 調用：`handle_execute_relation`

**位置**：`crates/sail-spark-connect/src/service/plan_executor.rs:147-154`

```rust
pub(crate) async fn handle_execute_relation(
    ctx: &SessionContext,
    relation: Relation,
    metadata: ExecutorMetadata,
) -> SparkResult<ExecutePlanResponseStream> {
    // 1. 將 protobuf Relation 轉換成 Sail 的 spec::Plan
    let plan = relation.try_into()?;

    // 2. 執行 plan
    handle_execute_plan(ctx, plan, metadata, ExecutePlanMode::Lazy).await
}
```

### 輸入：Relation (protobuf)

```rust
Relation {
    common: None,
    rel_type: Some(Sql(Sql {
        query: "SELECT * FROM users WHERE age > 18",
        args: {},
        pos_args: [],
        named_arguments: {},
        pos_arguments: [],
    }))
}
```

### Debug 輸出

```rust
println!("=== 2. handle_execute_relation ===");
println!("Relation type: {:?}", relation.rel_type);
```

**輸出**：
```
=== 2. handle_execute_relation ===
Relation type: Some(Sql(Sql { query: "SELECT * FROM users WHERE age > 18" }))
```

---

## 階段 3：Protobuf → Sail Plan

### 調用：`Relation::try_into()`

**位置**：`crates/sail-spark-connect/src/proto/plan.rs` (impl TryFrom)

這個轉換會：
1. 解析 Relation 的類型 (Sql, Read, Project, Filter, Join 等)
2. 轉換成 Sail 內部的 `spec::Plan` 結構

```rust
impl TryFrom<Relation> for spec::Plan {
    type Error = SparkError;

    fn try_from(relation: Relation) -> Result<Self, Self::Error> {
        use crate::spark::connect::relation::RelType;

        let rel_type = relation.rel_type.required("relation type")?;

        match rel_type {
            RelType::Sql(sql) => {
                // 轉換 SQL 查詢
                Ok(spec::Plan::Query(spec::QueryPlan::new(
                    spec::QueryNode::Sql(spec::SqlNode {
                        query: sql.query,
                        // ... 其他欄位
                    })
                )))
            }
            RelType::Read(read) => {
                // 轉換 Read (Scan table)
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
            // ... 更多類型
        }
    }
}
```

### 輸出：spec::Plan

```rust
spec::Plan::Query(QueryPlan {
    node: QueryNode::Sql(SqlNode {
        query: "SELECT * FROM users WHERE age > 18",
        args: {},
        pos_args: [],
    })
})
```

### Debug 輸出

```rust
// 在 try_into() 內加入
println!("=== 3. Relation → spec::Plan ===");
println!("Input relation type: {:?}", relation.rel_type);
let plan = // ... 轉換邏輯
println!("Output plan: {:#?}", plan);
```

**輸出**：
```
=== 3. Relation → spec::Plan ===
Input relation type: Some(Sql(...))
Output plan: Query(QueryPlan {
    node: Sql(SqlNode {
        query: "SELECT * FROM users WHERE age > 18"
    })
})
```

---

## 階段 4：執行 Plan

### 調用：`handle_execute_plan`

**位置**：`crates/sail-spark-connect/src/service/plan_executor.rs:91-144`

```rust
async fn handle_execute_plan(
    ctx: &SessionContext,
    plan: spec::Plan,
    metadata: ExecutorMetadata,
    mode: ExecutePlanMode,
) -> SparkResult<ExecutePlanResponseStream> {
    // 1. 取得 SparkSession
    let spark = ctx.extension::<SparkSession>()?;

    // 2. 解析並執行 plan
    let (execution_plan, schema_capture) =
        resolve_and_execute_plan(ctx, spark.plan_config()?, plan.clone()).await?;

    // 3. 執行 Physical Plan
    let stream = spark.job_runner().execute(ctx, execution_plan).await?;

    // 4. 根據模式處理結果
    match mode {
        ExecutePlanMode::Lazy => {
            // 返回 stream (不立即執行)
            // ...
        }
        ExecutePlanMode::Eager => {
            // 立即收集所有資料
            // ...
        }
        ExecutePlanMode::EagerSilent => {
            // 收集資料但不返回
            // ...
        }
    }
}
```

### Debug 輸出

```rust
println!("=== 4. handle_execute_plan ===");
println!("Plan: {:#?}", plan);
println!("Mode: {:?}", mode);
```

**輸出**：
```
=== 4. handle_execute_plan ===
Plan: Query(QueryPlan { node: Sql(...) })
Mode: Lazy
```

---

## 階段 5：解析並執行 Plan

### 調用：`resolve_and_execute_plan`

**位置**：`crates/sail-spark-connect/src/service/plan_executor.rs:41-89`

```rust
async fn resolve_and_execute_plan(
    ctx: &SessionContext,
    plan_config: Arc<PlanConfig>,
    plan: spec::Plan,
) -> SparkResult<(Arc<dyn ExecutionPlan>, Option<Arc<Schema>>)> {
    // 1. 創建 PlanResolver
    let mut resolver = PlanResolver::new(ctx, plan_config);

    // 2. 解析 Sail Plan → DataFusion LogicalPlan
    let logical_plan = resolver.resolve_plan(plan)?;

    // 3. 取得 schema (如果需要)
    let schema_capture = resolver.capture_schema().ok().flatten();

    // 4. 優化 LogicalPlan
    let optimized_plan = ctx.state().optimize(&logical_plan)?;

    // 5. 創建 Physical Plan
    let execution_plan = ctx.state().create_physical_plan(&optimized_plan).await?;

    Ok((execution_plan, schema_capture))
}
```

### Debug 輸出

```rust
println!("=== 5. resolve_and_execute_plan ===");
println!("Input plan: {:#?}", plan);

let logical_plan = resolver.resolve_plan(plan)?;
println!("\n=== Logical Plan ===");
println!("{}", logical_plan.display_indent());

let optimized_plan = ctx.state().optimize(&logical_plan)?;
println!("\n=== Optimized Logical Plan ===");
println!("{}", optimized_plan.display_indent());

let execution_plan = ctx.state().create_physical_plan(&optimized_plan).await?;
println!("\n=== Physical Plan ===");
println!("{}", DisplayableExecutionPlan::new(execution_plan.as_ref()).indent(true));
```

**輸出**：
```
=== 5. resolve_and_execute_plan ===
Input plan: Query(QueryPlan { node: Sql(...) })

=== Logical Plan ===
Projection: name, age, dept
  Filter: age > Int64(18)
    TableScan: users projection=[name, age, dept]

=== Optimized Logical Plan ===
Projection: name, age, dept
  Filter: age > Int64(18)
    TableScan: users projection=[name, age, dept]

=== Physical Plan ===
ProjectionExec: expr=[name@0 as name, age@1 as age, dept@2 as dept]
  FilterExec: age@1 > 18
    ParquetExec: file=/tmp/users.parquet, projection=[name, age, dept]
```

---

## 階段 6：Sail Plan → DataFusion LogicalPlan

### 調用：`PlanResolver::resolve_plan`

**位置**：`crates/sail-plan/src/resolver/plan.rs`

```rust
impl PlanResolver {
    pub fn resolve_plan(&mut self, plan: spec::Plan) -> PlanResult<LogicalPlan> {
        match plan {
            spec::Plan::Query(query) => {
                // 解析 Query
                self.resolve_query_plan(query)
            }
            spec::Plan::Command(command) => {
                // 解析 Command
                self.resolve_command_plan(command)
            }
        }
    }

    fn resolve_query_plan(&mut self, query: QueryPlan) -> PlanResult<LogicalPlan> {
        match query.node {
            QueryNode::Sql(sql) => {
                // 解析 SQL
                self.resolve_sql(sql)
            }
            QueryNode::Read(read) => {
                // 解析 Read (table scan)
                self.resolve_read(read)
            }
            QueryNode::Project(project) => {
                // 解析 Projection
                self.resolve_project(project)
            }
            QueryNode::Filter(filter) => {
                // 解析 Filter
                self.resolve_filter(filter)
            }
            // ... 更多節點類型
        }
    }

    fn resolve_sql(&mut self, sql: SqlNode) -> PlanResult<LogicalPlan> {
        // 1. 使用 SQL parser 解析 SQL
        let statements = self.sql_parser.parse_sql(&sql.query)?;

        // 2. 轉換成 DataFusion LogicalPlan
        let plan = self.statement_to_plan(statements[0].clone())?;

        Ok(plan)
    }
}
```

### Debug 輸出

```rust
println!("=== 6. PlanResolver::resolve_plan ===");
println!("Input: spec::Plan::Query(Sql)");
println!("SQL: {}", sql.query);

// 解析 SQL
let statements = self.sql_parser.parse_sql(&sql.query)?;
println!("\n=== Parsed Statements ===");
println!("{:#?}", statements);

// 轉換成 LogicalPlan
let plan = self.statement_to_plan(statements[0].clone())?;
println!("\n=== DataFusion LogicalPlan ===");
println!("{}", plan.display_indent());
```

**輸出**：
```
=== 6. PlanResolver::resolve_plan ===
Input: spec::Plan::Query(Sql)
SQL: SELECT * FROM users WHERE age > 18

=== Parsed Statements ===
Statement::Query(Query {
    body: Select {
        projection: [Wildcard],
        from: [TableWithJoins {
            relation: Table {
                name: ObjectName(["users"])
            }
        }],
        selection: Some(BinaryOp {
            left: Identifier("age"),
            op: Gt,
            right: Value(Number("18"))
        })
    }
})

=== DataFusion LogicalPlan ===
Projection: name, age, dept
  Filter: age > Int64(18)
    TableScan: users projection=[name, age, dept]
```

---

## 完整範例：追蹤整個流程

### 準備工作

在 `crates/sail-spark-connect/src/server.rs` 中加入：

```rust
async fn execute_plan(
    &self,
    request: Request<ExecutePlanRequest>,
) -> Result<Response<Self::ExecutePlanStream>, Status> {
    let request = request.into_inner();

    println!("╔════════════════════════════════════════════════════════════╗");
    println!("║           Execute Plan 完整調用鏈追蹤                      ║");
    println!("╚════════════════════════════════════════════════════════════╝");

    println!("\n[1] 接收 gRPC Request");
    println!("    Session ID: {}", request.session_id);
    if let Some(ref plan) = request.plan {
        if let Some(ref op) = plan.op_type {
            println!("    Plan Type: {:?}", op);
        }
    }

    let session_key = SessionKey {
        user_id: request.user_context.map(|u| u.user_id).unwrap_or_default(),
        session_id: request.session_id,
    };

    let metadata = ExecutorMetadata {
        operation_id: request.operation_id.unwrap_or_else(|| Uuid::new_v4().to_string()),
        tags: request.tags,
        reattachable: is_reattachable(&request.request_options),
    };

    println!("\n[2] 取得 SessionContext");
    let ctx = self
        .session_manager
        .get_or_create_session_context(session_key)
        .await?;
    println!("    Context 已準備");

    println!("\n[3] 解析 Plan");
    let Plan { op_type: op } = request.plan.required("plan")?;
    let op = op.required("plan op")?;

    let stream = match op {
        plan::OpType::Root(relation) => {
            println!("    Plan 類型: Query (Root Relation)");
            println!("\n[4] 調用 handle_execute_relation");
            service::handle_execute_relation(&ctx, relation, metadata).await?
        }
        plan::OpType::Command(Command { command_type }) => {
            println!("    Plan 類型: Command");
            // ... 其他命令處理
        }
    };

    println!("\n[完成] 返回結果 stream");
    println!("╚════════════════════════════════════════════════════════════╝\n");

    Ok(Response::new(stream))
}
```

### 執行查詢

```python
# test_query.py
from pyspark.sql import SparkSession

spark = SparkSession.builder.remote("sc://localhost:50051").getOrCreate()
df = spark.read.parquet("/tmp/users.parquet")
result = df.filter("age > 18")
result.show()
```

### 完整輸出

```
╔════════════════════════════════════════════════════════════╗
║           Execute Plan 完整調用鏈追蹤                      ║
╚════════════════════════════════════════════════════════════╝

[1] 接收 gRPC Request
    Session ID: abc123
    Plan Type: Root(Relation { rel_type: Some(Sql(...)) })

[2] 取得 SessionContext
    Context 已準備

[3] 解析 Plan
    Plan 類型: Query (Root Relation)

[4] 調用 handle_execute_relation
    Relation type: Sql
    Query: SELECT * FROM users WHERE age > 18

[5] Relation → spec::Plan
    Output: Query(QueryPlan { node: Sql(...) })

[6] handle_execute_plan
    Mode: Lazy
    開始解析 Plan

[7] resolve_and_execute_plan
    創建 PlanResolver

[8] PlanResolver::resolve_plan
    解析 SQL: SELECT * FROM users WHERE age > 18

[9] DataFusion Logical Plan
Projection: name, age, dept
  Filter: age > Int64(18)
    TableScan: users projection=[name, age, dept]

[10] 優化 Logical Plan
    (保持不變，predicate pushdown 已完成)

[11] 創建 Physical Plan
ProjectionExec: expr=[name@0 as name, age@1 as age, dept@2 as dept]
  FilterExec: age@1 > 18
    ParquetExec: file=/tmp/users.parquet, projection=[name, age, dept]

[12] 執行 Physical Plan
    讀取 Parquet 檔案
    應用 filter: age > 18
    返回結果

[完成] 返回結果 stream
╚════════════════════════════════════════════════════════════╝
```

---

## 關鍵資料結構

### ExecutePlanRequest (protobuf)

```rust
pub struct ExecutePlanRequest {
    pub session_id: String,
    pub user_context: Option<UserContext>,
    pub operation_id: Option<String>,
    pub plan: Option<Plan>,
    pub tags: Vec<String>,
    pub request_options: Vec<RequestOption>,
}
```

### Plan (protobuf)

```rust
pub struct Plan {
    pub op_type: Option<OpType>,
}

pub enum OpType {
    Root(Relation),         // 查詢
    Command(Command),       // 命令 (write, register UDF 等)
}
```

### Relation (protobuf)

```rust
pub struct Relation {
    pub common: Option<RelationCommon>,
    pub rel_type: Option<RelType>,
}

pub enum RelType {
    Sql(Sql),              // SQL 查詢
    Read(Read),            // Table scan
    Project(Project),      // Projection
    Filter(Filter),        // Filter
    Join(Join),            // Join
    // ... 更多類型
}
```

### spec::Plan (Sail 內部)

```rust
pub enum Plan {
    Query(QueryPlan),      // 查詢計畫
    Command(CommandPlan),  // 命令計畫
}

pub struct QueryPlan {
    pub node: QueryNode,
}

pub enum QueryNode {
    Sql(SqlNode),          // SQL
    Read(ReadNode),        // Table scan
    Project(ProjectNode),  // Projection
    Filter(FilterNode),    // Filter
    // ... 更多節點
}
```

---

## 總結

**完整調用鏈**：

1. **SparkConnectServer::execute_plan**
   - 接收 gRPC request
   - 提取 session, plan, metadata

2. **service::handle_execute_relation**
   - 將 protobuf Relation 轉換成 spec::Plan

3. **Relation::try_into()**
   - protobuf → Sail 內部格式

4. **handle_execute_plan**
   - 協調整個執行流程

5. **resolve_and_execute_plan**
   - spec::Plan → DataFusion LogicalPlan
   - 優化 LogicalPlan
   - 創建 Physical Plan

6. **PlanResolver::resolve_plan**
   - 解析 Sail Plan
   - 轉換成 DataFusion LogicalPlan

7. **DataFusion execution**
   - 執行 Physical Plan
   - 返回 RecordBatchStream

**關鍵轉換**：
```
gRPC protobuf → Sail spec::Plan → DataFusion LogicalPlan → Physical Plan → Results
```

**每個階段的作用**：
- **Protobuf**：網路傳輸格式
- **spec::Plan**：Sail 內部的中間表示，處理 Spark 語義
- **LogicalPlan**：DataFusion 的邏輯計畫
- **PhysicalPlan**：可執行的計畫
