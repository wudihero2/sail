# Sail Streaming 機制研究

這篇文章從零開始講解 Sail 如何實作 Spark Structured Streaming。
用一個最簡單的範例：readStream → filter → writeStream console 來追蹤整個調用鏈。


## 範例程式

```python
df = spark.readStream.format("rate").load()
query = df.writeStream.format("console").queryName("q1").start()
query.awaitTermination(5)
query.stop()
```

這四行 Python 觸發的完整流程：

```
PySpark Client
  │
  ├─ readStream.format("rate").load()    → gRPC: Plan(Read { is_streaming=true, DataSource("rate") })
  │                                        → 回傳 DataFrame（lazy，尚未執行）
  │
  ├─ writeStream.format("console").start() → gRPC: Command(WriteStreamOperationStart)
  │                                          → 伺服器端開始執行 streaming query
  │                                          → 回傳 StreamingQueryId
  │
  ├─ awaitTermination(5)                   → gRPC: StreamingQueryCommand(AwaitTermination)
  │
  └─ stop()                                → gRPC: StreamingQueryCommand(Stop)
```


## Crate 一覽

```
+-------------------------------+--------------------------------------------------+
| Crate                         | 在 streaming 裡的角色                            |
+-------------------------------+--------------------------------------------------+
| sail-spark-connect            | 接收 gRPC，管理 StreamingQuery 生命週期          |
+-------------------------------+--------------------------------------------------+
| sail-plan                     | 解析 spec → LogicalPlan，偵測 + 改寫 streaming  |
+-------------------------------+--------------------------------------------------+
| sail-logical-plan             | 自定義 streaming logical node                    |
|                               | （Wrapper / Adapter / Collector / Filter / Limit）|
+-------------------------------+--------------------------------------------------+
| sail-physical-plan            | 自定義 streaming physical exec                   |
|                               | （SourceAdapter / Collector / Filter / Limit）   |
+-------------------------------+--------------------------------------------------+
| sail-common-datafusion        | FlowEvent 定義、編碼解碼、StreamSource trait     |
+-------------------------------+--------------------------------------------------+
| sail-data-source              | 具體 source/sink：rate、socket、console          |
+-------------------------------+--------------------------------------------------+
```


## 調用鏈總覽

```
handle_execute_write_stream_operation_start()      ← gRPC 入口
  │
  ├→ resolve_and_execute_plan()
  │    ├→ PlanResolver.resolve_named_plan()         ← spec → LogicalPlan
  │    │    └→ resolve_command_write_stream()        ← WriteStream 命令解析
  │    │         └→ resolve_write_input()            ← 解析輸入的 Read relation
  │    │              └→ resolve_query_read_data_source()
  │    │                   └→ RateTableFormat::create_provider()
  │    │                        └→ StreamSourceTableProvider::new(RateStreamSource)
  │    │
  │    ├→ session_state.optimize()                   ← DataFusion optimizer
  │    │
  │    ├→ is_streaming_plan()?                       ← 偵測有沒有 StreamSourceTableProvider
  │    │
  │    ├→ rewrite_streaming_plan()                   ← 改寫 LogicalPlan
  │    │    └→ StreamingRewriter.f_up()              ← 自底向上改寫每個 node
  │    │         ├→ TableScan(rate) → StreamSourceWrapperNode
  │    │         ├→ Projection → 插入 _marker, _retracted
  │    │         ├→ Filter → StreamFilterNode
  │    │         └→ FileWriteNode → 保持不變
  │    │
  │    └→ create_physical_plan()                     ← LogicalPlan → PhysicalPlan
  │         ├→ StreamSourceWrapperNode → RateSourceExec
  │         ├→ StreamFilterNode → StreamFilterExec
  │         └→ FileWriteNode(console) → ConsoleSinkExec
  │
  ├→ service.runner().execute()                      ← 取得 SendableRecordBatchStream
  │
  └→ spark.start_streaming_query()                   ← 建立 StreamingQuery，tokio::spawn 背景執行
       └→ StreamingQuery::run()                      ← 消費 stream 直到 signal 或結束
```


## 第 1 步：gRPC 入口

PySpark 的 `writeStream.start()` 發送 `WriteStreamOperationStart` proto 訊息。

```rust
// crates/sail-spark-connect/src/service/plan_executor.rs:278-309
pub(crate) async fn handle_execute_write_stream_operation_start(
    ctx: &SessionContext,
    start: WriteStreamOperationStart,
    metadata: ExecutorMetadata,
) -> SparkResult<ExecutePlanResponseStream> {
    let spark = ctx.extension::<SparkSession>()?;
    let service = ctx.extension::<JobService>()?;
    let operation_id = metadata.operation_id.clone();
    let reattachable = metadata.reattachable;
    let query_name = start.query_name.clone();

    // 1. 把 proto 轉成 spec::Plan
    let plan = spec::Plan::Command(spec::CommandPlan::new(start.try_into()?));

    // 2. 解析 + 優化 + 改寫 + 生成 physical plan
    let (plan, info) = resolve_and_execute_plan(ctx, spark.plan_config()?, plan).await?;

    // 3. 執行 physical plan，拿到 record batch stream
    let stream = service.runner().execute(ctx, plan).await?;

    // 4. 註冊 streaming query，背景消費 stream
    let id = spark.start_streaming_query(query_name.clone(), info, stream)?;

    // 5. 回傳 query ID 給 PySpark client
    let result = WriteStreamOperationStartResult {
        query_id: Some(id.into()),
        name: query_name,
        query_started_event_json: None,
    };
    // ... 包裝成 response stream 回傳
}
```

這是整個 streaming 的起點。五個步驟：proto → resolve → execute → spawn → respond。


## 第 2 步：resolve_and_execute_plan

這個函數是 batch 和 streaming 共用的入口，streaming 的特殊處理在中間。

```rust
// crates/sail-plan/src/lib.rs:34-66
pub async fn resolve_and_execute_plan(
    ctx: &SessionContext,
    config: Arc<PlanConfig>,
    plan: spec::Plan,
) -> PlanResult<(Arc<dyn ExecutionPlan>, Vec<StringifiedPlan>)> {
    let mut info = vec![];

    // Phase 1: Resolve — spec → LogicalPlan
    let resolver = PlanResolver::new(ctx, config);
    let NamedPlan { plan, fields } = resolver.resolve_named_plan(plan).await?;
    info.push(plan.to_stringified(PlanType::InitialLogicalPlan));

    // Phase 2: Optimize — DataFusion optimizer
    let df = execute_logical_plan(ctx, plan).await?;
    let (session_state, plan) = df.into_parts();
    let plan = session_state.optimize(&plan)?;

    // Phase 3: Streaming detection + rewrite
    let plan = if is_streaming_plan(&plan)? {
        rewrite_streaming_plan(plan)?     // ← streaming 才會走這裡
    } else {
        plan
    };
    info.push(plan.to_stringified(PlanType::FinalLogicalPlan));

    // Phase 4: Physical planning
    let plan = session_state
        .query_planner()
        .create_physical_plan(&plan, &session_state)
        .await?;
    // ...
    Ok((plan, info))
}
```

重點是 Phase 3：optimize 完之後，檢查 plan 裡有沒有 StreamSourceTableProvider。
有的話就呼叫 `rewrite_streaming_plan()` 把 batch plan 改寫成 streaming plan。

為什麼要先 optimize 再改寫？因為 DataFusion optimizer 只認識標準 node（Projection、Filter 等），不認識自定義的 streaming node。所以先讓 optimizer 做完 predicate pushdown、projection pruning 等優化，再把標準 node 替換成 streaming 版本。


## 第 3 步：建立 StreamSourceTableProvider

`readStream.format("rate").load()` 在 resolve 階段會走到 `RateTableFormat::create_provider()`。

```rust
// crates/sail-data-source/src/formats/rate/mod.rs:30-77
async fn create_provider(
    &self,
    ctx: &dyn Session,
    info: SourceInfo,
) -> Result<Arc<dyn TableProvider>> {
    // ... 驗證 schema 和 options ...

    // 建立預設 schema：(timestamp: Timestamp, value: Int64)
    let schema = Schema::new(vec![
        Arc::new(Field::new("timestamp",
            DataType::Timestamp(TimeUnit::Microsecond, Some(tz)), true)),
        Arc::new(Field::new("value", DataType::Int64, true)),
    ]);

    let options = resolve_rate_read_options(options)?;

    // 建立 RateStreamSource（實作 StreamSource trait）
    let source = RateStreamSource::try_new(options, Arc::new(schema))?;

    // 包裝成 StreamSourceTableProvider（實作 TableProvider trait）
    Ok(Arc::new(StreamSourceTableProvider::new(Arc::new(source))))
}
```

注意最後一行：回傳的是 `StreamSourceTableProvider`，不是普通的 table provider。
這就是 streaming 的標記 — 後面 `is_streaming_plan()` 就是靠它來偵測 streaming。

🔸 StreamSource trait

```rust
// crates/sail-common-datafusion/src/streaming/source.rs:16-45
pub trait StreamSource: Send + Sync + fmt::Debug {
    /// 資料 schema（不含 flow event 欄位）
    fn data_schema(&self) -> SchemaRef;

    /// 建立 physical plan，回傳的 stream 是 encoded flow event 格式
    async fn scan(
        &self,
        state: &dyn Session,
        projection: Option<&Vec<usize>>,
        filters: &[Expr],
        limit: Option<usize>,
    ) -> Result<Arc<dyn ExecutionPlan>>;
}
```

🔸 StreamSourceTableProvider 的 scan() 不能被呼叫

```rust
// crates/sail-common-datafusion/src/streaming/source.rs:83-91
async fn scan(&self, ...) -> Result<Arc<dyn ExecutionPlan>> {
    internal_err!("stream source should be rewritten during logical planning")
}
```

StreamSourceTableProvider 的 `scan()` 會直接報錯。
它的 schema() 回傳的是原始 data schema（沒有 _marker / _retracted），
因為在 streaming rewrite 之前，DataFusion optimizer 需要用原始 schema 做優化。
真正的 scan 是在 rewrite 之後，由 StreamSourceWrapperNode 的 physical planner 呼叫 `StreamSource::scan()`。


## 第 4 步：is_streaming_plan — 偵測 streaming

```rust
// crates/sail-plan/src/streaming/rewriter.rs:233-242
pub fn is_streaming_plan(plan: &LogicalPlan) -> Result<bool> {
    plan.exists(|plan| {
        if let LogicalPlan::TableScan(scan) = plan {
            Ok(source_as_provider(&scan.source)
                .is_ok_and(|p| is_streaming_table_provider(p.as_ref())))
        } else {
            Ok(false)
        }
    })
}
```

用 DataFusion 的 `exists()` 遍歷整棵 logical plan tree。
只要有任何一個 TableScan 的 source 是 StreamSourceTableProvider，就判定為 streaming plan。

`is_streaming_table_provider()` 還會處理 RenameTableProvider 的包裝：

```rust
// crates/sail-plan/src/streaming/rewriter.rs:188-196
fn is_streaming_table_provider(provider: &dyn TableProvider) -> bool {
    if provider.as_any().is::<StreamSourceTableProvider>() {
        true
    } else if let Some(rename) = provider.as_any().downcast_ref::<RenameTableProvider>() {
        is_streaming_table_provider(rename.inner().as_ref())   // 遞迴往下找
    } else {
        false
    }
}
```

有時候 Spark 會幫表格重命名欄位（例如 `SELECT timestamp AS ts`），
這時 TableProvider 會被包在 RenameTableProvider 裡面，需要遞迴找到真正的 provider。


## 第 5 步：rewrite_streaming_plan — 改寫 LogicalPlan

這是 streaming 最核心的步驟。把 DataFusion 的標準 batch plan 改寫成 streaming plan。

```rust
// crates/sail-plan/src/streaming/rewriter.rs:249-264
pub fn rewrite_streaming_plan(plan: LogicalPlan) -> Result<LogicalPlan> {
    let node = plan.rewrite(&mut StreamingRewriter)?;
    let plan = node.data;

    // 如果改寫後的 plan 輸出 flow event schema（沒有 sink），
    // 加一個 StreamCollectorNode 把 streaming 收集成 batch 結果
    if is_flow_event_schema(plan.schema().inner()) {
        Ok(LogicalPlan::Extension(Extension {
            node: Arc::new(StreamCollectorNode::try_new(Arc::new(plan))?),
        }))
    } else {
        Ok(plan)
    }
}
```

`plan.rewrite(&mut StreamingRewriter)` 使用 DataFusion 的 `TreeNodeRewriter` trait，自底向上遍歷 plan tree，對每個 node 呼叫 `f_up()`。

🔸 StreamingRewriter::f_up() — 每種 node 的改寫規則

```rust
// crates/sail-plan/src/streaming/rewriter.rs:63-185（簡化）
impl TreeNodeRewriter for StreamingRewriter {
    type Node = LogicalPlan;

    fn f_up(&mut self, plan: LogicalPlan) -> Result<Transformed<LogicalPlan>> {
        match plan {
            // 1. TableScan：是 streaming source → StreamSourceWrapperNode
            //              不是 streaming source → StreamSourceAdapterNode
            LogicalPlan::TableScan(ref scan) => {
                if let Ok(provider) = source_as_provider(&scan.source) {
                    if let Some(source) = get_stream_source_opt(provider.as_ref()) {
                        // 原生 streaming source（如 rate）
                        Ok(Transformed::yes(LogicalPlan::Extension(Extension {
                            node: Arc::new(StreamSourceWrapperNode::try_new(
                                table_name, source, names, projection, filters, fetch,
                            )?),
                        })))
                    } else {
                        // batch source（如 Parquet）→ 用 adapter 包成 streaming
                        Ok(Transformed::yes(LogicalPlan::Extension(Extension {
                            node: Arc::new(StreamSourceAdapterNode::try_new(Arc::new(plan))?),
                        })))
                    }
                }
            }

            // 2. Projection：插入 _marker 和 _retracted 欄位
            LogicalPlan::Projection(projection) => {
                let Projection { mut expr, input, .. } = projection;
                expr.insert(0, col(MARKER_FIELD_NAME));      // _marker
                expr.insert(1, col(RETRACTED_FIELD_NAME));    // _retracted
                Ok(Transformed::yes(LogicalPlan::Projection(
                    Projection::try_new(expr, input)?,
                )))
            }

            // 3. Filter：改成 StreamFilterNode，保證 marker 不被過濾掉
            LogicalPlan::Filter(filter) => {
                let predicate = or(predicate, col(MARKER_FIELD_NAME).is_not_null());
                Ok(Transformed::yes(LogicalPlan::Extension(Extension {
                    node: Arc::new(StreamFilterNode::new(input, predicate)),
                })))
            }

            // 4. Limit：改成 StreamLimitNode
            LogicalPlan::Limit(ref limit) => {
                let SkipType::Literal(skip) = limit.get_skip_type()?;
                let FetchType::Literal(fetch) = limit.get_fetch_type()?;
                Ok(Transformed::yes(LogicalPlan::Extension(Extension {
                    node: Arc::new(StreamLimitNode::new(Arc::clone(&limit.input), skip, fetch)),
                })))
            }

            // 5. 不支援的操作：直接報錯
            LogicalPlan::Aggregate(_) => not_impl_err!("streaming aggregate"),
            LogicalPlan::Sort(_) => plan_err!("sort is not supported for streaming"),
            LogicalPlan::Join(_) => not_impl_err!("streaming join"),
            LogicalPlan::Window(_) => not_impl_err!("streaming window"),
            LogicalPlan::Repartition(_) => not_impl_err!("streaming repartition"),

            // 6. 其他保持不變或特殊處理
            LogicalPlan::Union(union) => { /* 允許 loose types */ }
            LogicalPlan::SubqueryAlias(alias) => { /* 重新建立 alias */ }
            LogicalPlan::EmptyRelation(_) | LogicalPlan::Values(_) => {
                // 靜態資料 → StreamSourceAdapterNode
            }
            LogicalPlan::Extension(extension) => {
                // FileWriteNode → 保持不變（sink node）
                // ShowStringNode → 加 StreamCollectorNode
                // RangeNode → StreamSourceAdapterNode
            }
        }
    }
}
```

改寫後的 plan tree 範例（rate → console）：

```
改寫前（batch plan）：              改寫後（streaming plan）：
FileWriteNode(console)              FileWriteNode(console)  ← 保持不變
  └─ Projection                       └─ Projection(_marker, _retracted, timestamp, value)
       └─ TableScan(rate)                  └─ StreamSourceWrapperNode(rate)
```


## 第 6 步：FlowEvent — streaming 的核心資料格式

Sail 的 streaming 不是直接傳 RecordBatch，而是用 FlowEvent 包裝。

```rust
// crates/sail-common-datafusion/src/streaming/event/mod.rs:14-38
pub enum FlowEvent {
    /// 一批可撤回的資料
    Data {
        batch: RecordBatch,
        retracted: BooleanArray,  // 每一行是否被撤回
    },
    /// 注入到 data flow 裡的標記
    Marker(FlowMarker),
}

impl FlowEvent {
    /// 建立一批 append-only 資料（所有 retracted = false）
    pub fn append_only_data(batch: RecordBatch) -> Self {
        let len = batch.num_rows();
        let retracted = {
            let mut builder = BooleanBuilder::with_capacity(len);
            builder.append_n(len, false);
            builder.finish()
        };
        Self::Data { batch, retracted }
    }
}
```

🔸 retracted 的用途

streaming 裡某些操作（如 aggregation）會更新之前發出的結果。
例如 `SELECT count(*) FROM stream` 每次收到新資料就要「撤回」上一個 count 再發新的。
retracted = true 表示這行是在撤回之前的結果。（目前 Sail 還沒實作 streaming aggregate，所以都是 append-only。）

🔸 FlowMarker 的種類

```protobuf
// crates/sail-common-datafusion/proto/sail/streaming/marker.proto
message FlowMarker {
  oneof Kind {
    LatencyTracker latency_tracker = 1;
    Watermark watermark = 2;
    Checkpoint checkpoint = 3;
    EndOfData end_of_data = 4;
  }
}

message LatencyTracker {
  string source = 1;           // 來自哪個 source
  uint64 id = 2;               // 追蹤 ID
  int64 timestamp_secs = 3;    // 注入時的時間戳
  uint32 timestamp_nanos = 4;
}

message Watermark {
  string source = 1;           // 來自哪個 source
  int64 timestamp_secs = 2;    // 水位線時間戳
  uint32 timestamp_nanos = 3;
}

message Checkpoint {
  uint64 id = 1;               // checkpoint ID
}

message EndOfData {}
```

四種 marker 的用途：

```
+------------------+--------------------------------------------------------------+----------+
| Marker           | 用途                                                         | 目前狀態 |
+------------------+--------------------------------------------------------------+----------+
| EndOfData        | 告訴下游「我沒有更多資料了」。                               | 已使用   |
|                  | batch source 被 adapter 包成 streaming 時，讀完所有資料後    |          |
|                  | 會發一個 EndOfData，讓下游知道可以結束。                      |          |
|                  | StreamLimitExec 讀夠行數後也會發 EndOfData。                 |          |
+------------------+--------------------------------------------------------------+----------+
| Watermark        | 水位線：「event time < 這個時間戳的資料，不會再來了」。      | 未使用   |
|                  | 用於 event-time window aggregate，告訴下游可以安全地         |          |
|                  | 輸出某個時間窗口的結果。                                     |          |
|                  | 例如 watermark = 10:05 表示 10:05 之前的資料都到齊了，       |          |
|                  | 10:00-10:05 的 window 可以結算。                             |          |
+------------------+--------------------------------------------------------------+----------+
| Checkpoint       | 檢查點：告訴下游「把目前的狀態存起來」。                     | 未使用   |
|                  | 用於 fault tolerance，如果 query 掛了可以從最近的            |          |
|                  | checkpoint 恢復，不需要從頭重跑。                            |          |
+------------------+--------------------------------------------------------------+----------+
| LatencyTracker   | 延遲追蹤：source 注入一個帶時間戳的 marker，                 | 未使用   |
|                  | 當 sink 收到它時，用「現在時間 - 注入時間」算出              |          |
|                  | 資料從 source 到 sink 的端到端延遲。                         |          |
|                  | 用於監控 streaming pipeline 的健康度。                       |          |
+------------------+--------------------------------------------------------------+----------+
```

Marker 跟著資料一起在 pipeline 裡流動，不包含實際資料。
目前只有 EndOfData 被實際使用（在 StreamSourceAdapterExec 和 StreamLimitExec）。
Watermark、Checkpoint、LatencyTracker 是為未來 streaming aggregate 和 fault tolerance 預留的。


## 第 7 步：Flow Event Schema — 把 FlowEvent 編碼成 RecordBatch

DataFusion 的 ExecutionPlan 只認識 RecordBatch，不認識 FlowEvent。
所以 Sail 把 FlowEvent 編碼成特殊 schema 的 RecordBatch：

```rust
// crates/sail-common-datafusion/src/streaming/event/schema.rs:36-45
pub fn to_flow_event_schema(schema: &Schema) -> Schema {
    let mut fields = vec![
        Field::new("_marker", DataType::Binary, true),      // nullable
        Field::new("_retracted", DataType::Boolean, false),  // non-nullable
    ];
    fields.extend(schema.fields().iter().map(|x| x.as_ref().clone()));
    Schema::new(fields)
}
```

範例：rate source 的 data schema 是 `(timestamp, value)`，
flow event schema 就是 `(_marker, _retracted, timestamp, value)`。

```
原始 data schema:
+-------------------+--------+
| timestamp         | value  |
+-------------------+--------+

flow event schema:
+---------+------------+-------------------+--------+
| _marker | _retracted | timestamp         | value  |
+---------+------------+-------------------+--------+
| null    | false      | 2025-01-01 00:00  | 0      |  ← 資料行
| null    | false      | 2025-01-01 00:00  | 1      |  ← 資料行
| [bytes] | false      | [placeholder]     | 0      |  ← marker 行
+---------+------------+-------------------+--------+
```

資料行：_marker = null，其餘欄位是真實資料。
Marker 行：_marker = protobuf 編碼的 FlowMarker，其餘欄位是 placeholder。

🔸 EncodedFlowEventStream — 編碼

```rust
// crates/sail-common-datafusion/src/streaming/event/encoding.rs:42-76
pub fn encode(&self, event: FlowEvent) -> Result<RecordBatch> {
    let columns = match event {
        FlowEvent::Data { batch, retracted } => {
            let mut columns: Vec<ArrayRef> = vec![
                new_null_array(&DataType::Binary, batch.num_rows()),  // _marker = null
                Arc::new(retracted),                                  // _retracted
            ];
            columns.extend(batch.columns().iter().cloned());          // 原始資料
            columns
        }
        FlowEvent::Marker(marker) => {
            let marker = {
                let values = marker.encode()?;             // protobuf → bytes
                // ... 建立 BinaryArray ...
            };
            let retracted = placeholder_boolean_array(1);  // placeholder
            let mut columns: Vec<ArrayRef> = vec![marker, retracted];
            for field in self.inner.schema().fields() {
                // 每個欄位填 placeholder 或 null
                columns.push(placeholder_array(field.data_type(), 1)?);
            }
            columns
        }
    };
    Ok(RecordBatch::try_new(self.schema.clone(), columns)?)
}
```

🔸 DecodedFlowEventStream — 解碼

解碼的邏輯是反過來：掃描 _marker 欄位，null 的是資料行，non-null 的是 marker 行。
連續的資料行會被 slice 成一個 FlowEvent::Data。

```rust
// crates/sail-common-datafusion/src/streaming/event/encoding.rs:141-173
fn decode(&self, batch: RecordBatch) -> Result<Vec<FlowEvent>> {
    let (marker, retracted, data) = self.get_special_columns_and_data(&batch)?;
    let mut events = vec![];
    let mut start_data_index = None;

    for i in 0..batch.num_rows() {
        if marker.is_valid(i) {
            // marker 行 → 先 flush 之前累積的資料行
            if let Some(start) = start_data_index {
                events.push(FlowEvent::Data {
                    batch: data.slice(start, i - start),
                    retracted: retracted.slice(start, i - start),
                });
                start_data_index = None;
            }
            let marker = FlowMarker::decode(marker.value(i))?;
            events.push(FlowEvent::Marker(marker));
        } else if start_data_index.is_none() {
            start_data_index = Some(i);
        }
    }
    // flush 最後的資料行
    if let Some(start) = start_data_index {
        events.push(FlowEvent::Data {
            batch: data.slice(start, batch.num_rows() - start),
            retracted: retracted.slice(start, batch.num_rows() - start),
        });
    }
    Ok(events)
}
```


## 第 8 步：RateSourceExec — 產生 streaming 資料

RateSourceExec 是 rate source 的 physical plan，產生無限的 (timestamp, value) 資料。

```rust
// crates/sail-data-source/src/formats/rate/reader.rs:92-119（建構）
pub fn try_new(options: RateReadOptions, schema: SchemaRef, projection: Vec<usize>) -> Result<Self> {
    let output_schema = Arc::new(to_flow_event_schema(&projected_schema));
    let properties = Arc::new(PlanProperties::new(
        EquivalenceProperties::new(output_schema),
        Partitioning::UnknownPartitioning(options.num_partitions),
        EmissionType::Both,
        Boundedness::Unbounded { requires_infinite_memory: false },  // ← 無界
    ));
    // ...
}
```

`Boundedness::Unbounded` 告訴 DataFusion 這是一個無限 stream。

```rust
// crates/sail-data-source/src/formats/rate/reader.rs:193-243（執行）
fn execute(&self, partition: usize, _context: Arc<TaskContext>) -> Result<SendableRecordBatchStream> {
    let rows_per_second = (self.options.rows_per_second / self.options.num_partitions).max(1);
    let batches_per_second = rows_per_second.min(1_000);
    let batch_size = rows_per_second / batches_per_second;
    let interval = Duration::from_secs(1) / (batches_per_second as u32);

    let generator = BatchGenerator::try_new(/* ... */)?;

    // 用 unfold 建立無限 stream：每隔 interval 產生一批資料
    let output = futures::stream::unfold(generator, move |mut generator| async move {
        tokio::time::sleep(interval).await;
        let result = generator.generate(batch_size);
        Some((result, generator))
    });

    // 每個 RecordBatch → FlowEvent::append_only_data
    let output = output.map(|x| Ok(FlowEvent::append_only_data(x?)));

    // 包裝成 FlowEventStreamAdapter → EncodedFlowEventStream
    let stream = Box::pin(FlowEventStreamAdapter::new(self.projected_schema.clone(), output));
    Ok(Box::pin(EncodedFlowEventStream::new(stream)))
}
```

資料產生的流程：

```
tokio::time::sleep(interval)
  → BatchGenerator::generate(batch_size)
    → RecordBatch(timestamp, value)
      → FlowEvent::append_only_data(batch)
        → FlowEventStreamAdapter
          → EncodedFlowEventStream
            → RecordBatch(_marker=null, _retracted=false, timestamp, value)
```


## 第 9 步：StreamSourceAdapterExec — 把 batch 變成 streaming

如果 streaming plan 裡有 batch source（如 Parquet table），需要用 adapter 包裝。

```rust
// crates/sail-physical-plan/src/streaming/source_adapter.rs:80-95
fn execute(&self, partition: usize, context: Arc<TaskContext>) -> Result<SendableRecordBatchStream> {
    let stream = self
        .input
        .execute(partition, context)?
        .map(|x| Ok(FlowEvent::append_only_data(x?)))   // 每個 batch → FlowEvent
        .chain(stream::once(async {
            Ok(FlowEvent::Marker(FlowMarker::EndOfData))  // 結束時發 EndOfData marker
        }));
    let stream = Box::pin(FlowEventStreamAdapter::new(self.input.schema(), stream));
    Ok(Box::pin(EncodedFlowEventStream::new(stream)))
}
```

跟 RateSourceExec 的差別：batch source 是有限的，讀完所有資料後會發一個 EndOfData marker。


## 第 10 步：ConsoleSinkExec — 輸出到 console

```rust
// crates/sail-data-source/src/formats/console/writer.rs:77-111
fn execute(&self, partition: usize, context: Arc<TaskContext>) -> Result<SendableRecordBatchStream> {
    let stream = self.input.execute(partition, context)?;
    let output = futures::stream::once(async move {
        stream
            .enumerate()
            .for_each(|(i, batch)| async move {
                let text = match batch {
                    Ok(batch) => match pretty_format_batches(&[batch]) {
                        Ok(batch) => format!("{batch}"),
                        Err(e) => format!("error formatting batch: {e}"),
                    },
                    Err(e) => format!("error: {e}"),
                };
                let mut stdout = std::io::stdout().lock();
                let _ = writeln!(stdout, "partition {partition} batch {i}");
                let _ = writeln!(stdout, "{text}");
            })
            .await;
        futures::stream::empty()
    })
    .flatten();
    Ok(Box::pin(RecordBatchStreamAdapter::new(self.schema(), output)))
}
```

ConsoleSinkExec 從上游拿到的是 flow event schema 的 RecordBatch（含 _marker、_retracted），
直接用 `pretty_format_batches` 印出來。印完回傳空 stream（schema = empty）。

注意 properties 裡 `Boundedness::Bounded`：雖然上游是無限的 stream，
但 ConsoleSinkExec 本身的輸出是空的（它只印不回傳資料），所以是 bounded。


## 第 11 步：StreamCollectorExec — 把 streaming 收成 batch

如果 streaming 沒有 sink（例如 `spark.readStream.format("rate").load().limit(10).collect()`），
rewrite_streaming_plan 會自動加一個 StreamCollectorNode 包住整個 plan。

```rust
// crates/sail-physical-plan/src/streaming/collector.rs:28-31
pub fn try_new(input: Arc<dyn ExecutionPlan>) -> Result<Self> {
    if input.properties().boundedness != Boundedness::Bounded {
        return plan_err!("stream collector requires bounded input");
    }
    // ...
}
```

StreamCollectorExec 要求輸入必須是 Bounded。
如果上游是 Unbounded 但有 LIMIT，StreamLimitExec 會在讀夠資料後發 EndOfData marker，
讓整個 pipeline 變成 Bounded。

```rust
// crates/sail-physical-plan/src/streaming/collector.rs:97-125
fn execute(&self, partition: usize, context: Arc<TaskContext>) -> Result<SendableRecordBatchStream> {
    let stream = self.input.execute(partition, context)?;
    // 解碼 flow event，過濾掉 marker，只保留 data batch
    let stream = DecodedFlowEventStream::try_new(stream)?.filter_map(|event| async move {
        match event {
            Ok(FlowEvent::Marker(_)) => None,                         // 丟掉 marker
            Ok(FlowEvent::Data { batch, retracted: _ }) => Some(Ok(batch)),  // 保留 data
            Err(e) => Some(Err(e)),
        }
    });
    Ok(Box::pin(RecordBatchStreamAdapter::new(self.schema(), stream)))
}
```

注意 `retracted: _` — 目前 retraction 還沒被處理（TODO 註解）。
未來需要在這裡根據 retracted flag 做差異計算，把撤回的行移除。


## 第 12 步：StreamingQuery — 背景執行管理

stream 建好之後，最後一步是 `spark.start_streaming_query()`。

```rust
// crates/sail-spark-connect/src/streaming.rs:23-43
impl StreamingQuery {
    pub fn new(
        name: String,
        info: Vec<StringifiedPlan>,
        stream: SendableRecordBatchStream,
    ) -> Self {
        let (signal_tx, signal_rx) = oneshot::channel();    // 停止信號
        let (error_tx, error_rx) = watch::channel(None);    // 錯誤通知
        let (stopped_tx, stopped_rx) = watch::channel(false); // 停止狀態

        // 背景 task 消費 stream
        tokio::spawn(async move {
            Self::run(signal_rx, error_tx, stopped_tx, stream).await;
        });

        Self { name, info, error: error_rx, stopped: stopped_rx, signal: Some(signal_tx), awaitable: true }
    }
}
```

三個 channel：
- `signal`（oneshot）：外部呼叫 stop() 時發送，通知背景 task 停止
- `error`（watch）：背景 task 遇到錯誤時發送，外部可以查詢
- `stopped`（watch）：背景 task 結束時設為 true，外部可以等待

🔸 StreamingQuery::run() — 背景消費邏輯

```rust
// crates/sail-spark-connect/src/streaming.rs:64-86
async fn run(
    signal: oneshot::Receiver<()>,
    error: watch::Sender<Option<SparkThrowable>>,
    stopped: watch::Sender<bool>,
    mut stream: SendableRecordBatchStream,
) {
    let task = async move {
        while let Some(x) = stream.next().await {
            match x {
                Ok(_) => {}         // 成功的 batch → 什麼都不做（資料已被 sink 消費）
                Err(e) => {
                    let cause = CommonErrorCause::new::<PyErrExtractor>(&e);
                    let _ = error.send(Some(cause.into()));  // 發送錯誤
                }
            }
        }
    };
    // 等待 signal 或 task 完成，先到先贏
    tokio::select! {
        _ = signal => {}   // 收到 stop 信號
        _ = task => {}     // stream 自然結束
    }
    let _ = stopped.send(true);  // 標記為已停止
}
```

`tokio::select!` 讓 query 可以被兩種方式終止：
1. 外部呼叫 `query.stop()` → signal channel 收到訊號
2. stream 自然結束（如有 LIMIT 或 EndOfData）

注意 `Ok(_) => {}` — 成功的 batch 直接丟掉。因為資料已經在 pipeline 裡被 sink 消費了（如 ConsoleSinkExec 印到 stdout），這裡只是驅動 stream 繼續執行。

🔸 StreamingQueryManager — 多 query 管理

```rust
// crates/sail-spark-connect/src/streaming.rs:113-247
pub struct StreamingQueryManager {
    queries: HashMap<StreamingQueryId, StreamingQuery>,
}

impl StreamingQueryManager {
    pub fn add_query(&mut self, id: StreamingQueryId, query: StreamingQuery) { /* ... */ }
    pub fn stop_query(&mut self, id: &StreamingQueryId) -> SparkResult<()> {
        // 取出 signal sender，發送停止信號
        if let Some(signal) = query.signal.take() {
            let _ = signal.send(());
        }
    }
    pub fn list_active_queries(&self) -> Vec<(StreamingQueryId, StreamingQueryStatus)> { /* ... */ }
    pub fn await_query(&self, id: &StreamingQueryId) -> SparkResult<Option<StreamingQueryAwaitHandle>> { /* ... */ }
    pub fn reset_stopped_queries(&mut self) { /* ... */ }
}
```


## 第 13 步：resolve_command_write_stream — WriteStream 命令解析

回到 resolve 階段，WriteStream 命令的解析：

```rust
// crates/sail-plan/src/resolver/command/write_stream.rs:12-73
pub(super) async fn resolve_command_write_stream(
    &self,
    write_stream: spec::WriteStream,
    state: &mut PlanResolverState,
) -> PlanResult<LogicalPlan> {
    let spec::WriteStream {
        input, format, options,
        partitioning_column_names, query_name: _,
        foreach_writer, foreach_batch,
        clustering_column_names, sink_destination,
    } = write_stream;

    // foreach 不支援
    if foreach_writer.is_some() {
        return Err(PlanError::todo("foreach sink in write stream"));
    }
    if foreach_batch.is_some() {
        return Err(PlanError::unsupported("foreach batch is not supported"));
    }

    // 解析輸入 relation
    let input = self.resolve_write_input(*input, state).await?;

    // 建立 WritePlanBuilder
    let mut builder = WritePlanBuilder::new()
        .with_partition_by(partition_by)
        .with_format(format)            // "console"
        .with_options(options)
        .with_mode(WriteMode::Append { error_if_absent: false });

    // 根據 sink destination 設定 target
    match sink_destination {
        None => { builder = builder.with_target(WriteTarget::DataSource); }
        Some(WriteStreamSinkDestination::Path { path }) => {
            builder = builder.with_target(WriteTarget::DataSource)
                .with_options(vec![("path".to_string(), path)]);
        }
        Some(WriteStreamSinkDestination::Table { table }) => {
            builder = builder.with_target(WriteTarget::Table { table, column_match: WriteColumnMatch::ByName })
        }
    }

    self.resolve_write_with_builder(input, builder, state).await
}
```

注意 `WriteMode::Append` — streaming write 固定用 append mode。


## 完整 Physical Plan 範例

對於 `spark.readStream.format("rate").load().writeStream.format("console").start()`：

```
ConsoleSinkExec
  └─ ProjectionExec(_marker, _retracted, timestamp, value)
       └─ RateSourceExec(rows_per_second=1, num_partitions=1)
```

對於 `spark.readStream.format("rate").load().limit(10).collect()`：

```
StreamCollectorExec                 ← 收集 flow event 成 batch
  └─ CoalescePartitionsExec         ← DataFusion 自動加的（SinglePartition 要求）
       └─ StreamLimitExec(skip=0, fetch=10)
            └─ RateSourceExec(rows_per_second=1, num_partitions=1)
```


## 目前的限制

```
+--------------------+-------------+
| 功能               | 狀態        |
+--------------------+-------------+
| readStream (rate)  | 支援        |
| readStream (socket)| 支援        |
| writeStream (console)| 支援     |
| writeStream (file) | 支援        |
| Filter             | 支援        |
| Projection         | 支援        |
| Limit              | 支援        |
| Union              | 支援        |
| Aggregate          | 不支援      |
| Join               | 不支援      |
| Window             | 不支援      |
| Sort               | 不支援      |
| Repartition        | 不支援      |
| foreach / foreachBatch | 不支援  |
| Watermark          | 定義了但未用 |
| Checkpoint         | 定義了但未用 |
| Retraction         | 定義了但未處理 |
| Trigger 語義       | 忽略        |
| Output Mode        | 忽略        |
+--------------------+-------------+
```


## 建議閱讀順序

```
1. FlowEvent + schema          → 了解 streaming 的核心資料格式
   sail-common-datafusion/src/streaming/event/mod.rs
   sail-common-datafusion/src/streaming/event/schema.rs

2. StreamSource trait           → 了解 streaming source 介面
   sail-common-datafusion/src/streaming/source.rs

3. RateTableFormat + RateSourceExec → 一個完整的 source 實作
   sail-data-source/src/formats/rate/mod.rs
   sail-data-source/src/formats/rate/reader.rs

4. rewrite_streaming_plan      → 了解 batch → streaming 的 plan 改寫
   sail-plan/src/streaming/rewriter.rs

5. EncodedFlowEventStream      → 了解 FlowEvent ↔ RecordBatch 編碼
   sail-common-datafusion/src/streaming/event/encoding.rs

6. ConsoleSinkExec             → 一個完整的 sink 實作
   sail-data-source/src/formats/console/writer.rs

7. StreamCollectorExec         → streaming → batch 的收集器
   sail-physical-plan/src/streaming/collector.rs

8. StreamingQuery + Manager    → query 生命週期管理
   sail-spark-connect/src/streaming.rs

9. handle_execute_write_stream → gRPC 入口，串聯所有步驟
   sail-spark-connect/src/service/plan_executor.rs:278
```
