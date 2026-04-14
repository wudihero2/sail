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

🔸 目前哪些 source / node 實際有發哪些 marker

搜遍整個 codebase，只有 EndOfData 被實際發送。
LatencyTracker、Watermark、Checkpoint 只存在於 marker.rs 的單元測試（encode/decode round-trip），
沒有任何 source 或 node 在執行時發送。

```
+---------------------+------------------+---------------------------------------------+
| 元件                | 發的 marker      | 源碼位置                                    |
+---------------------+------------------+---------------------------------------------+
| StreamSource        | 無               | RateSourceExec 和 SocketSourceExec 只發     |
| AdapterExec         | EndOfData        | FlowEvent::append_only_data（資料），       |
|                     |                  | 不發任何 marker。                           |
|                     |                  | EndOfData 是 StreamSourceAdapterExec 在     |
|                     |                  | batch source 讀完後用 .chain() 追加的：     |
|                     |                  | source_adapter.rs:89-91                     |
+---------------------+------------------+---------------------------------------------+
| RateSourceExec      | 無               | 只用 unfold + sleep 產生                    |
|                     |                  | FlowEvent::append_only_data，              |
|                     |                  | 無限 stream，不發任何 marker。              |
|                     |                  | rate/reader.rs:237                          |
+---------------------+------------------+---------------------------------------------+
| SocketSourceExec    | 無               | 跟 rate 一樣，只發 append_only_data。       |
+---------------------+------------------+---------------------------------------------+
| StreamLimitExec     | EndOfData        | 讀夠行數後手動發 EndOfData 終止 stream。    |
|                     |                  | limit.rs:228 透傳上游的 marker。            |
+---------------------+------------------+---------------------------------------------+
| StreamFilterExec    | 透傳             | 不產生 marker，上游有什麼 marker 就透傳。   |
+---------------------+------------------+---------------------------------------------+
| StreamCollectorExec | 不產生，只消費   | 收到 Marker → 丟掉（filter_map None）。     |
|                     |                  | 收到 Data → 輸出 batch。                    |
|                     |                  | collector.rs:113                            |
+---------------------+------------------+---------------------------------------------+
| ConsoleSinkExec     | 不區分           | 直接把 encoded RecordBatch 印出來，         |
|                     |                  | 不解碼 FlowEvent，marker 行也會被印。       |
+---------------------+------------------+---------------------------------------------+
| Watermark           | 沒人發           | 只在 marker.rs 測試裡出現。                 |
|                     |                  | 未來 source（如 Kafka）需要根據 event time  |
|                     |                  | 定期發 Watermark，才能支援 streaming        |
|                     |                  | aggregate 和 stream-stream join。           |
+---------------------+------------------+---------------------------------------------+
| Checkpoint          | 沒人發           | 只在 marker.rs 測試裡出現。                 |
|                     |                  | 未來需要 checkpoint coordinator 定期        |
|                     |                  | 注入 Checkpoint marker 到所有 source。      |
+---------------------+------------------+---------------------------------------------+
| LatencyTracker      | 沒人發           | 只在 marker.rs 測試裡出現。                 |
|                     |                  | 未來可以由監控系統定期注入，                |
|                     |                  | 用來量測端到端延遲。                        |
+---------------------+------------------+---------------------------------------------+
```

總結：目前 Sail 的 streaming 只用到 EndOfData，整個 marker 體系的其餘三種都是預留設計。
要支援 streaming aggregate / join / fault tolerance，第一步就是讓 source 開始發 Watermark。

🔸 Watermark — 建議實作與原理

原理：Watermark 告訴下游「event time 早於這個時間戳的資料，不會再來了」。
有了 Watermark，下游的 aggregate / join 才知道什麼時候可以結算一個時間窗口、清掉過期 state。

Spark 的使用方式：

```python
df = spark.readStream.format("kafka").load() \
    .withWatermark("event_time", "10 minutes")
#                   ↑ 哪個欄位        ↑ 允許多晚到達（late threshold）
```

意思是：如果目前看到的最大 event_time 是 10:30，
那 Watermark = 10:30 - 10min = 10:20，
表示 10:20 之前的資料不會再來了（晚超過 10 分鐘的就丟棄）。

建議在 Sail 裡的實作方式：

```
1. 在 RateSourceExec / KafkaSourceExec 的 execute() 裡追蹤 max_event_time
2. 每隔 N 個 batch（或每隔 N 秒）發一個 Watermark marker

改動 rate/reader.rs 的 execute()：

目前（只發資料）：
  unfold(generator, |gen| {
      sleep(interval);
      let batch = gen.generate(batch_size);
      Some((Ok(FlowEvent::append_only_data(batch)), gen))
  })

改成（資料 + 定期 watermark）：
  unfold((generator, batch_count), |(gen, count)| {
      sleep(interval);
      let batch = gen.generate(batch_size);

      if count % WATERMARK_INTERVAL == 0 {
          // 每 100 個 batch 發一次 watermark
          let wm_time = current_max_event_time - late_threshold;
          Some((vec![
              Ok(FlowEvent::append_only_data(batch)),
              Ok(FlowEvent::Marker(FlowMarker::Watermark {
                  source: "rate".into(),
                  timestamp: wm_time,
              })),
          ], (gen, count + 1)))
      } else {
          Some((vec![Ok(FlowEvent::append_only_data(batch))], (gen, count + 1)))
      }
  }).flat_map(|events| stream::iter(events))
```

下游 node 的處理：

```
StreamFilterExec   → 收到 Watermark → 直接透傳（目前已經會透傳所有 marker）
StreamLimitExec    → 收到 Watermark → 直接透傳（目前已經會透傳）
StreamAggregateExec（未來）→ 收到 Watermark → 結算已到期的 window → 輸出結果 → 清 state
StreamStreamJoinExec（未來）→ 收到 Watermark → evict 過期 buffer entry
```

Watermark 在 join 裡的傳播：

```
左 source → Watermark(10:20)
右 source → Watermark(10:25)

JoinExec 內部：
  left_wm = 10:20
  right_wm = 10:25
  output_wm = min(10:20, 10:25) = 10:20  ← 往下游發這個
  // 用 min 是因為：左邊還可能來 10:20-10:25 的資料
  // 只有兩邊都過了 10:20，才能保證 10:20 之前的不會再來
```

🔸 Checkpoint — 建議實作與原理

原理：Checkpoint 告訴所有 stateful node「把你目前的 state 存下來」。
如果 query 掛了，可以從最近一次 checkpoint 的 state 恢復，不需要從頭重跑。

需要 checkpoint 的場景：streaming aggregate 和 stream-stream join 都有 state（如 group 的 partial sum、join 的 buffer），這些 state 只存在記憶體裡。
如果 process 掛了，state 就沒了，重啟後要從 Kafka offset 0 開始重跑所有資料。

建議實作方式：

```
架構：
  CheckpointCoordinator（新元件，在 StreamingQuery 層）
    │
    │ 每隔 N 秒 / 每 N 個 batch
    │
    ├→ 注入 Checkpoint(id=1) 到所有 source
    │    source_1 → [data, data, Checkpoint(1), data, data, ...]
    │    source_2 → [data, Checkpoint(1), data, data, ...]
    │
    ├→ 等所有 sink 收到 Checkpoint(1)
    │    sink 收到 Checkpoint(1) 代表：
    │    「Checkpoint(1) 之前的所有資料都已經被處理完了」
    │
    └→ 通知所有 stateful node 把 state 快照到 disk
         AggregateExec → 存 group state
         JoinExec → 存 left_buffer + right_buffer
         Source → 存 Kafka offset

恢復流程：
  1. 讀取最近的 checkpoint（如 id=5）
  2. 每個 source 從 checkpoint 記錄的 offset 開始讀
  3. 每個 stateful node 從 checkpoint 恢復 state
  4. 繼續處理新資料
```

為什麼要用 marker 注入（而不是直接通知 node）：

```
問題：兩個 source 的速度不同

source_1: [d, d, d, d, d, d, d, d]     ← 快
source_2: [d, d]                        ← 慢

如果在 t=5 時直接通知 JoinExec「存 state」：
  left_buffer 已經收了 source_1 的 8 筆資料
  right_buffer 只收了 source_2 的 2 筆資料
  → 這個 state 快照不一致！

用 marker 注入的做法：
  source_1: [d, d, d, Checkpoint(1), d, d, d, d]
  source_2: [d, Checkpoint(1), d]

  JoinExec 收到左邊的 Checkpoint(1)：先記住，不動作
  JoinExec 收到右邊的 Checkpoint(1)：兩邊都到了 → 現在存 state
  → 這個時間點，兩邊 buffer 裡的資料都是 Checkpoint(1) 之前的 → 一致！
```

這就是 Flink 的 Chandy-Lamport 分散式快照演算法的核心思想：
用 barrier（marker）在 stream 裡切出一致的快照邊界。

那 Checkpoint marker 實際上怎麼「塞進 source 的 stream 裡」？
source 的 stream 是 `futures::stream::unfold` 產生的無限迴圈，
Checkpoint 不是 source 自己決定要發的，是外部的 CheckpointCoordinator 決定的。

做法：用 mpsc channel 從外部注入。

```rust
// Source 啟動時建立一個 channel
let (marker_tx, marker_rx) = mpsc::channel::<FlowMarker>(16);

// Source 的 stream：用 tokio::select! 同時聽「產生資料」和「外部注入 marker」
let output = futures::stream::unfold(
    (generator, marker_rx),
    |(mut gen, mut rx)| async move {
        tokio::select! {
            // 正常產生資料（等 interval 到）
            _ = tokio::time::sleep(interval) => {
                let batch = gen.generate(batch_size);
                Some((Ok(FlowEvent::append_only_data(batch)), (gen, rx)))
            }
            // 外部注入 marker（CheckpointCoordinator 發來的）
            Some(marker) = rx.recv() => {
                Some((Ok(FlowEvent::Marker(marker)), (gen, rx)))
            }
        }
    },
);
```

CheckpointCoordinator 持有每個 source 的 marker_tx：

```
CheckpointCoordinator（住在 StreamingQuery 層）
  │
  │  持有 marker_tx_1, marker_tx_2, ...（每個 source 一個）
  │
  │  每隔 N 秒觸發一次 checkpoint：
  ├→ marker_tx_1.send(Checkpoint(1))  → source_1 的 rx 收到 → 插入 stream
  ├→ marker_tx_2.send(Checkpoint(1))  → source_2 的 rx 收到 → 插入 stream
  │
  │  marker 跟著資料流動...
  │
  └→ 等 sink 回報 Checkpoint(1) 完成 → 記錄「checkpoint 1 已完成」
```

為什麼用 channel + select 而不是輪詢 flag：

```
方法 1（不好）：輪詢 flag
  loop {
      if should_checkpoint { emit marker }  // 誰設 flag？race condition？
      sleep(interval);
      emit data;
  }

方法 2（好）：channel + select
  tokio::select! {
      _ = sleep(interval) => emit data     // 正常節奏
      Some(m) = rx.recv() => emit marker   // 外部注入，立即響應
  }
  // channel 天然 thread-safe，select 天然異步，不需要鎖
```

channel 的好處是 source 完全不需要知道 checkpoint 的邏輯，
它只負責「收到什麼就發什麼」。所有排程決策（多久一次、哪些 source 要注入）
都在 CheckpointCoordinator 裡，source 保持簡單。
Flink 也是這個做法 — CheckpointCoordinator 透過 StreamTask 的 input channel 注入 CheckpointBarrier。

🔸 LatencyTracker — 建議實作與原理

原理：在 source 端注入一個帶時間戳的 marker，
當 sink 收到它時，用「收到時間 - 注入時間」算出端到端延遲。
用於監控 streaming pipeline 是否健康、有沒有 backpressure 導致延遲飆高。

```
使用情境：
  DevOps 想知道「從資料進入 pipeline 到被 sink 處理完，要多久？」

  如果延遲突然從 200ms 跳到 5s → 某個 node 有瓶頸（backpressure）
  如果延遲穩定在 50ms → pipeline 健康
```

建議實作方式：

```
1. Source 端（注入）：

  每隔 N 秒注入一個 LatencyTracker：
  FlowEvent::Marker(FlowMarker::LatencyTracker {
      source: "rate-0".into(),     // source 名稱 + partition
      id: tracker_id,              // 遞增 ID
      timestamp: Utc::now(),       // 注入時的時間
  })

  tracker 跟著資料一起流過整個 pipeline：
  source → filter → projection → join → aggregate → sink

2. 中間 node（透傳）：

  StreamFilterExec、StreamLimitExec 等 → 收到 LatencyTracker → 直接透傳
  （目前的實作已經會透傳所有 marker，不需要改）

3. Sink 端（量測）：

  ConsoleSinkExec / FileSinkExec 收到 LatencyTracker 時：
  let latency = Utc::now() - tracker.timestamp;
  metrics.record("e2e_latency", latency);
  // 不輸出到結果，只記錄到 metrics

4. 監控面板：

  Grafana 之類的工具讀取 metrics，畫出：
  - P50 / P99 延遲
  - 每個 source 的延遲（用 source 欄位區分）
  - 延遲趨勢（用 id 欄位排序）
```

跟其他 marker 的差別：

```
+------------------+----------+----------+---------+
| Marker           | 誰注入   | 誰消費   | 頻率    |
+------------------+----------+----------+---------+
| Watermark        | Source   | 中間 node| 每 N batch |
|                  |          | (agg/join)|         |
+------------------+----------+----------+---------+
| Checkpoint       | Coordinator| 所有 stateful node | 每 N 秒 |
+------------------+----------+----------+---------+
| LatencyTracker   | Source   | Sink     | 每 N 秒 |
+------------------+----------+----------+---------+
| EndOfData        | Source/  | Collector| 一次    |
|                  | Limit    |          |         |
+------------------+----------+----------+---------+
```

LatencyTracker 是四種 marker 裡最容易實作的：不需要任何 state、不影響資料正確性、只需要 source 注入 + sink 量測。適合作為 marker 體系的第一個進階實作練手。

🔸 分散式場景：Marker 必須 broadcast

Marker 被編碼成普通 RecordBatch 的一行（`_marker` 欄位非 null），
所以它會跟著資料一起在 shuffle/exchange 裡流動。
但如果 exchange 用 hash repartition，marker 只會送到一個 partition：

```
source(partition=0): [data, data, Checkpoint(1), data, ...]
                                       │
                          HashRepartition(by user_id)
                          ┌──────┬──────┬──────┐
                          ↓      ↓      ↓      ↓
                       part_0  part_1  part_2  part_3

Checkpoint(1) 這行的 user_id 是 placeholder（0），
hash(0) 可能 = 2 → 只有 part_2 收到
part_0、part_1、part_3 永遠收不到 → checkpoint 永遠完不成
```

不只 Checkpoint，所有 marker 都有這個問題：

```
+------------------+-----------------------------------------------------------+
| Marker           | 如果只有一個 partition 收到會怎樣                         |
+------------------+-----------------------------------------------------------+
| Checkpoint       | 其他 partition 的 stateful node 永遠等不到               |
|                  | → checkpoint 永遠完不成                                  |
+------------------+-----------------------------------------------------------+
| Watermark        | 其他 partition 的 join/aggregate 不知道可以 evict state  |
|                  | → state 無限增長 → OOM                                   |
+------------------+-----------------------------------------------------------+
| EndOfData        | 其他 partition 不知道 stream 結束了                       |
|                  | → StreamCollectorExec 永遠等不到所有 partition 結束      |
+------------------+-----------------------------------------------------------+
| LatencyTracker   | 只量到一個 partition 的延遲，其他未知                     |
|                  | → 不影響正確性，但監控資料不完整                         |
+------------------+-----------------------------------------------------------+
```

所以 exchange/shuffle 層需要對 marker 做特殊處理 — broadcast 到所有下游 partition：

```
source(partition=0): [data, data, Checkpoint(1), data, ...]
                                       │
                          ┌─── _marker IS NOT NULL？───┐
                          │ YES                        │ NO
                          ↓                            ↓
                     Broadcast 到                HashRepartition
                     所有 partition              按 user_id 分
                    ┌────┬────┬────┐
                    ↓    ↓    ↓    ↓
                 part_0 part_1 part_2 part_3  ← 每個都收到 Checkpoint(1)
```

Flink 也是這個做法：CheckpointBarrier 走 broadcast channel，不走 hash partition。

目前 Sail 還沒碰到這個問題，因為只有 EndOfData 被使用，
而且目前沒有跨 worker 的 streaming shuffle。
但未來做 distributed streaming 時，這是 exchange 層必須處理的。


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


## 未來：Streaming Join 設計思路

目前 rewriter 裡 Join 會回傳 `not_impl_err`（未來要做），Sort 回傳 `plan_err`（本質不支援）。
以下是 streaming join 的設計思路。

🔸 Phase 1：Stream-Batch Join（先做，簡單）

一邊是 streaming source，另一邊是靜態 batch table（如 Parquet 維度表）。

```
場景：streaming 訂單 JOIN 靜態商品表

  orders (streaming)              products (batch)
  +-----------+------------+      +------------+-------+
  | order_id  | product_id |      | product_id | name  |
  +-----------+------------+      +------------+-------+
  | 1         | A          |      | A          | 蘋果  |
  | 2         | B          |      | B          | 香蕉  |

  SELECT o.order_id, p.name
  FROM orders o JOIN products p ON o.product_id = p.product_id
```

做法：batch 側啟動時全部讀進 hash table（build side），streaming 側每來一個 batch 就去 probe。

```
  streaming side (probe)          batch side (build)
  ┌──────────────────┐            ┌──────────────────┐
  │ RateSourceExec   │            │ ParquetExec      │
  │ (Unbounded)      │            │ (Bounded)        │
  └────────┬─────────┘            └────────┬─────────┘
           │                               │
           │    ┌─────────────────────┐    │
           └───→│ StreamHashJoinExec  │←───┘
                │ build: batch side   │
                │ probe: stream side  │
                └─────────┬───────────┘
                          │
                   flow event output
```

不需要 Watermark 和 State Store，因為 batch 側是靜態的、有限的。
streaming 側的 marker 直接透傳，batch 側沒有 marker。

🔸 Phase 2：Stream-Stream Join（後做，複雜）

兩邊都是 streaming source，核心是 StreamStreamJoinExec。

```
場景：streaming 點擊事件 JOIN streaming 曝光事件

  SELECT c.user_id, i.ad_id
  FROM clicks c JOIN impressions i
    ON c.user_id = i.user_id
    AND c.time BETWEEN i.time AND i.time + INTERVAL 10 MINUTES
```

🔸 StreamStreamJoinExec 結構

```rust
// 假想的設計
pub struct StreamStreamJoinExec {
    left: Arc<dyn ExecutionPlan>,       // 左邊 streaming source
    right: Arc<dyn ExecutionPlan>,      // 右邊 streaming source
    on: Vec<(Column, Column)>,          // JOIN key（如 user_id = user_id）
    join_type: JoinType,                // Inner / Left / Right / Full
    time_condition: Option<TimeRange>,  // 時間窗口（如 BETWEEN i.time AND i.time + 10min）
    properties: Arc<PlanProperties>,
}
```

🔸 execute() — 同時消費兩邊 stream

兩邊是獨立的 stream，用 tokio::select! 交替處理哪邊先來就處理哪邊：

```rust
fn execute(&self, partition: usize, context: Arc<TaskContext>) -> Result<SendableRecordBatchStream> {
    let left_stream = DecodedFlowEventStream::try_new(
        self.left.execute(partition, context.clone())?
    )?;
    let right_stream = DecodedFlowEventStream::try_new(
        self.right.execute(partition, context)?
    )?;

    let state = JoinState::new(self.on.clone(), self.time_condition.clone());

    let output = futures::stream::unfold(
        (left_stream, right_stream, state),
        |(mut left, mut right, mut state)| async move {
            tokio::select! {
                Some(event) = left.next() => {
                    let output = state.process_left(event?);
                    Some((output, (left, right, state)))
                }
                Some(event) = right.next() => {
                    let output = state.process_right(event?);
                    Some((output, (left, right, state)))
                }
                else => None  // 兩邊都結束
            }
        },
    );

    let stream = output.flat_map(|events| futures::stream::iter(events.into_iter().map(Ok)));
    let stream = Box::pin(FlowEventStreamAdapter::new(self.output_schema(), stream));
    Ok(Box::pin(EncodedFlowEventStream::new(stream)))
}
```

🔸 JoinState — 雙邊 buffer + watermark evict

```rust
struct JoinState {
    left_buffer: HashMap<JoinKey, Vec<BufferedRow>>,
    right_buffer: HashMap<JoinKey, Vec<BufferedRow>>,
    left_watermark: Option<SystemTime>,
    right_watermark: Option<SystemTime>,
    on: Vec<(Column, Column)>,
    time_condition: Option<TimeRange>,
}

struct BufferedRow {
    batch: RecordBatch,     // 這行的資料
    event_time: SystemTime, // event time（用來判斷過期）
}
```

三個核心方法：

```rust
impl JoinState {
    /// 左邊來了一個 event
    fn process_left(&mut self, event: FlowEvent) -> Vec<FlowEvent> {
        match event {
            FlowEvent::Data { batch, retracted } => {
                let mut output = vec![];
                for row in batch.iter_rows() {
                    let key = extract_join_key(&row, &self.on);
                    // 1. 去右邊 buffer 找配對
                    if let Some(matches) = self.right_buffer.get(&key) {
                        for matched in matches {
                            if self.time_in_range(&row, matched) {
                                output.push(combine_rows(&row, matched));
                            }
                        }
                    }
                    // 2. 存進左邊 buffer（等右邊未來的資料來配對）
                    self.left_buffer.entry(key).or_default()
                        .push(BufferedRow::from(row));
                }
                vec![FlowEvent::append_only_data(output)]
            }
            FlowEvent::Marker(FlowMarker::Watermark { timestamp, .. }) => {
                self.left_watermark = Some(timestamp);
                self.try_evict();
                self.emit_watermark()
            }
            FlowEvent::Marker(other) => vec![FlowEvent::Marker(other)],
        }
    }

    /// 右邊來了一個 event（跟 process_left 對稱）
    fn process_right(&mut self, event: FlowEvent) -> Vec<FlowEvent> {
        // 左右互換的相同邏輯
    }

    /// 用 watermark 清掉過期的 buffer
    fn try_evict(&mut self) {
        // safe_wm = min(左 watermark, 右 watermark)
        // 因為兩邊都過了這個時間，才能確定不會再有更早的資料
        let safe_wm = match (self.left_watermark, self.right_watermark) {
            (Some(l), Some(r)) => Some(l.min(r)),
            _ => None,
        };
        if let Some(wm) = safe_wm {
            self.left_buffer.retain(|_, rows| {
                rows.retain(|row| row.event_time >= wm - self.time_range());
                !rows.is_empty()
            });
            self.right_buffer.retain(|_, rows| {
                rows.retain(|row| row.event_time >= wm - self.time_range());
                !rows.is_empty()
            });
        }
    }
}
```

🔸 實際上資料是一批 batch 流進來，不是一行一行

上面 `for row in batch.iter_rows()` 是概念性的寫法。
實際上 DataFusion 是 columnar 的，FlowEvent::Data 的 batch 可能有幾百行。
真正的實作會用 columnar 操作：

```
實際的資料流（rate source, rows_per_second=1000, batches_per_second=10）：

每 100ms 產生一個 batch，每個 batch 有 100 行：

t=0ms    FlowEvent::Data { batch: RecordBatch(100 rows), retracted: [false; 100] }
t=100ms  FlowEvent::Data { batch: RecordBatch(100 rows), retracted: [false; 100] }
t=200ms  FlowEvent::Data { batch: RecordBatch(100 rows), retracted: [false; 100] }
...

偶爾穿插 marker：
t=5000ms FlowEvent::Marker(Watermark { time=t-5s })
```

StreamStreamJoinExec 收到的是整個 RecordBatch（100 行），不是一行一行。
所以 process_left 的真正實作會是 batch 級別的操作：

```rust
fn process_left_batch(&mut self, batch: RecordBatch) -> Vec<RecordBatch> {
    // 1. 從 batch 裡抽出 join key column
    let keys = batch.column(self.left_key_index);

    // 2. 對整個 batch 做 hash probe（不是逐行）
    //    類似 DataFusion HashJoinExec 的 probe 邏輯
    let (matched_left_indices, matched_right_indices) =
        self.probe_right_buffer(keys);

    // 3. 用 indices 做 take 操作，拼出配對結果
    let left_matched = take_batch(&batch, &matched_left_indices);
    let right_matched = take_buffered(&self.right_buffer, &matched_right_indices);
    let output = concat_columns(left_matched, right_matched);

    // 4. 把整個 batch 存進左邊 buffer
    //    按 join key group 存，方便右邊來的時候 probe
    self.insert_to_left_buffer(batch);

    vec![output]
}
```

用時間線走一遍具體的 batch 流動：

```
左邊(clicks)                 StreamStreamJoinExec                 右邊(impressions)
                             left_buf    right_buf
                             (empty)     (empty)

t=0   ─── batch_R1 ─────────────────────────────────── batch_R1 來了
      100 rows               left_buf    right_buf     (user=A..Z, time=10:00-10:01)
      (impressions)          (empty)     {A:[..],      → 沒左邊配對 → 存進 right_buf
                                          B:[..],      → 輸出：空
                                          ...}

t=100ms                                                batch_L1 來了
      batch_L1 ─────────────────────────────────────   (user=A,B,C, time=10:00)
      50 rows                left_buf    right_buf     → probe right_buf
      (clicks)               {A:[..],   {A:[..],      → user=A 配對到！time 在範圍內
                              B:[..],    B:[..],       → user=B 配對到！
                              C:[..]}    ...}          → 輸出：matched_batch(30 rows)
                                                       → 同時存進 left_buf

t=200ms                                                batch_L2 來了
      batch_L2 ─────────────────────────────────────   (user=D,E, time=10:01)
      50 rows                left_buf    right_buf     → probe right_buf
      (clicks)               {A,B,C,    {A,B,...}      → user=D 配對到！
                              D,E}                     → 輸出：matched_batch(20 rows)

t=5000ms                                               Watermark 來了
      Watermark(10:05) ─────────────────────────────   右邊的 watermark
                             right_wm = 10:05
                             left_wm = None
                             → 只有一邊有 wm，不能 evict

t=5100ms
      ── Watermark(10:03) ──────────────────────────   左邊的 watermark
                             right_wm = 10:05
                             left_wm = 10:03
                             safe_wm = min(10:03, 10:05) = 10:03
                             → 清掉 time < 10:03 - 10min = 9:53 的 entry
                             → 這次沒啥好清的（資料都在 10:00 之後）
                             → 往下游發 Watermark(10:03)

t=很久以後
      ── Watermark(10:30) ──────────────────────────
                             safe_wm = min(10:30, 10:25) = 10:25
                             → 清掉 time < 10:25 - 10min = 10:15 的 entry
                             → left_buf 和 right_buf 裡 10:15 之前的全清掉
                             → buffer 不會無限增長
```

🔸 為什麼是 batch 級別而不是逐行

```
+----------------+-----------------------+------------------------------+
| 處理方式       | 逐行                  | 逐 batch（columnar）         |
+----------------+-----------------------+------------------------------+
| 100 行的處理   | for row in 0..100     | 一次 hash probe 100 個 key   |
|                | hash_lookup(row.key)  | take(batch, matched_indices) |
|                | combine(row, match)   | concat_columns(left, right)  |
+----------------+-----------------------+------------------------------+
| CPU 效率       | 差（逐元素、cache miss）| 好（向量化、SIMD friendly）  |
+----------------+-----------------------+------------------------------+
| Arrow 相容性   | 需要拆 Array → 逐元素 | 直接操作 Array（零拷貝 slice）|
+----------------+-----------------------+------------------------------+
```

DataFusion 的 HashJoinExec 也是 batch 級別的 probe，
StreamStreamJoinExec 可以直接複用 DataFusion 的 hash table 和 take/concat 工具函數。

🔸 需要的基礎設施

```
+-------------------+------------------------------------------+----------+
| 元件              | 用途                                     | 現狀     |
+-------------------+------------------------------------------+----------+
| Watermark 產生    | source 定期發 Watermark marker           | 未實作   |
+-------------------+------------------------------------------+----------+
| Watermark 傳播    | join node 取 min(左, 右) 往下游傳        | 未實作   |
+-------------------+------------------------------------------+----------+
| State Store       | 存 join buffer                           | 未實作   |
|                   | 小資料 in-memory HashMap                 |          |
|                   | 大資料 RocksDB / Foyer 溢出到 disk       |          |
+-------------------+------------------------------------------+----------+
| Checkpoint        | 定期把 state store 快照存下來            | 未實作   |
+-------------------+------------------------------------------+----------+
| 時間條件解析      | 從 JOIN ON 裡抽出 time range 條件        | 未實作   |
+-------------------+------------------------------------------+----------+
```

🔸 建議實作順序

```
1. Stream-Batch Join          ← 不需要 Watermark / State Store，實用性最高
2. Watermark 產生 + 傳播      ← Phase 2 和 Aggregate 的共同前置
3. State Store (in-memory)    ← 先做記憶體版本
4. Streaming Aggregate        ← 比 stream-stream join 簡單（只需單邊 state）
5. Stream-Stream Join         ← 最複雜，需要上面全部
6. Checkpoint + Recovery      ← production 才需要
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
