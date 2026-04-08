# Sail Cache 機制研究

## 目前 Sail 已有的 Cache

Sail 目前有三層 cache，全部基於 Moka（Rust 版 Caffeine，LRU + TTL）。
這些 cache 都屬於 DataFusion 的 RuntimeEnv CacheManager 體系，
在 `RuntimeEnvFactory::create()` 裡面被註冊到 DataFusion。

```
crates/sail-cache/src/
├── file_metadata_cache.rs    MokaFileMetadataCache
├── file_statistics_cache.rs  MokaFileStatisticsCache
├── file_listing_cache.rs     MokaFileListingCache
└── lib.rs                    try_parse_memory_limit()
```

🔸 三個 Cache 一覽

```
+----------------------------+---------------------------+-----------------+
| Cache                      | 快取什麼                  | Key             |
+----------------------------+---------------------------+-----------------+
| MokaFileMetadataCache      | Parquet footer/schema     | object_store    |
|                            | 等檔案元資料              | Path            |
+----------------------------+---------------------------+-----------------+
| MokaFileStatisticsCache    | row count, column stats,  | object_store    |
|                            | byte size, ordering       | Path            |
+----------------------------+---------------------------+-----------------+
| MokaFileListingCache       | 目錄下的檔案清單          | TableScopedPath |
|                            | (Vec<ObjectMeta>)         | (table + path)  |
+----------------------------+---------------------------+-----------------+
```

這三個都實作 DataFusion 的 `CacheAccessor<K, V>` trait（get / put / remove / contains_key / len / clear）。

🔸 註冊位置

```
// crates/sail-session/src/runtime.rs
let cache_config = CacheManagerConfig::default()
    .with_files_statistics_cache(Some(self.create_file_statistics_cache()))
    .with_list_files_cache(Some(self.create_file_listing_cache()))
    .with_file_metadata_cache(Some(self.create_file_metadata_cache()));

let builder = RuntimeEnvBuilder::default()
    .with_cache_manager(cache_config)
    ...
```

DataFusion 在開啟 Parquet 檔案時會先查 cache，避免重複讀取 footer 和統計資訊。

🔸 DeltaTableCache

除了上面三個 DataFusion 內建的 cache 介面之外，Sail 還有一個獨立的 DeltaTableCache：

```
// crates/sail-delta-lake/src/session_extension.rs
pub struct DeltaTableCache {
    cache: FutureCache<TableCacheKey, Arc<CachedTable>>,
}

struct TableCacheKey {
    table_url: String,
    version: i64,
}

struct CachedTable {
    snapshot: Arc<DeltaSnapshot>,
    log_store: LogStoreRef,
}
```

這是用 Moka 的 async FutureCache，快取 Delta Lake 表的 snapshot + log store，
key 是 (table_url, version)，最多 1024 筆。
好處是同一個 Delta 版本不需要重複讀取 transaction log。

🔸 Hugging Face 檔案快取

在 `crates/sail-object-store/src/hugging_face.rs` 有一個特殊的快取行為：
透過 `hf_hub` crate 的 API 會把整個檔案下載到本地磁碟。

```
// TODO: The cache is not ideal for cluster mode. In cluster mode, the driver
//   only needs to access the metadata at the end of the Parquet file, but the
//   entire file is downloaded and cached.
// TODO: The cache is not efficient for wide tables with many columns when
//   only a small set of columns are accessed.
```

這是 hf_hub 自己管理的磁碟快取，Sail 只是使用者。
兩個 TODO 提到的問題正好是 Issue #1015 想解決的方向。


## 尚未實作的 Spark Cache API

Spark 有一套完整的 DataFrame/Table cache 機制（df.cache() / df.persist() / CACHE TABLE 等），
Sail 目前全部是 todo 或 no-op：

```
// crates/sail-plan/src/resolver/command/mod.rs
CommandNode::IsCached { .. }    => Err(PlanError::todo("PlanNode::IsCached")),
CommandNode::CacheTable { .. }  => Err(PlanError::todo("PlanNode::CacheTable")),
CommandNode::UncacheTable { .. }=> Err(PlanError::todo("PlanNode::UncacheTable")),
CommandNode::ClearCache         => Err(PlanError::todo("PlanNode::ClearCache")),

// crates/sail-plan/src/resolver/query/mod.rs
spec::QueryNode::CachedLocalRelation { .. }  => Err(PlanError::todo("cached local relation")),
spec::QueryNode::CachedRemoteRelation { .. } => Err(PlanError::todo("cached remote relation")),

// crates/sail-spark-connect/src/service/plan_analyzer.rs
pub(crate) async fn handle_analyze_persist(...) {
    warn!("Persist operation is not yet supported and is a no-op");
    Ok(PersistResponse {})
}
pub(crate) async fn handle_analyze_unpersist(...) {
    warn!("Unpersist operation is not yet supported and is a no-op");
    Ok(UnpersistResponse {})
}
```

在 Spark 中 `df.cache()` 等於 `df.persist(StorageLevel.MEMORY_AND_DISK)`，
會把 DataFrame 的執行結果以 columnar 格式存在 BlockManager 裡面。
後續查詢如果用到同一個 DataFrame，就直接讀 cache 不重新計算。

Sail 要實作這些 API，需要一套 result cache / materialized view 的機制，
這跟 Issue #1015 的 Object Store Cache 是不同層次的需求。


## Issue #1015: Object Store Cache Using Foyer

🔸 Issue 概要

Issue #1015（[Caching] Object Store Cache Using Foyer）屬於 Epic #1014（[EPIC] Caching）。
Epic 的願景是："disk cache, memory cache, result cache, partial cache, full cache,
whatever cache, the less we touch remote storage the best."

Epic #1014 下有兩個子 issue：
- #1015 Object Store Cache Using Foyer
- #1017 Iceberg and Delta Lake caching

Issue #1015 沒有詳細描述，但從標題可以推斷：
在 Object Store 層加一個 Foyer 驅動的 hybrid cache（memory + disk），
讓從 S3/GCS/ADLS 讀取的資料被快取在本地。

🔸 Foyer 是什麼

Foyer（https://github.com/foyer-rs/foyer）是 Rust 版的 hybrid cache library，
靈感來自 Facebook CacheLib（C++）和 Caffeine（Java）。

```
+---------------------------------------------+
|              Foyer Hybrid Cache              |
+---------------------------------------------+
|                                              |
|  +---------+     miss     +-----------+     |
|  | Memory  | ----------> | Disk      |     |
|  | Cache   | <---------- | Cache     |     |
|  +---------+    promote   +-----------+     |
|       |                        |             |
|       v                        v             |
|    LRU/LFU/              block-based        |
|    FIFO evict            engine + index     |
|                                              |
+---------------------------------------------+
         |  miss
         v
  +----------------+
  | Remote Storage |
  | (S3 / GCS)    |
  +----------------+
```

主要特性：
- memory + disk 兩層自動 tiering
- 可插拔的驅逐策略（LRU, LFU, FIFO, S3-FIFO）
- zero-copy abstraction
- 支援 OpenTelemetry / Prometheus 觀測
- 已被 RisingWave, Chroma, SlateDB 等生產環境使用
- MSRV Rust 1.86.0

API 範例：
```rust
// 純 memory cache
let cache = CacheBuilder::new(16).build();
cache.insert("key", value);

// hybrid (memory + disk)
let hybrid = HybridCacheBuilder::new()
    .memory(64 * 1024 * 1024)       // 64 MB memory
    .storage()
    .with_engine_config(BlockEngineConfig::new(device))
    .build()
    .await?;
```


## 實作方案分析

要在 Sail 加入 Object Store Cache，有幾種可能的切入點：

🔸 方案 A：ObjectStore 層包裝（推薦用於 #1015）

在 object_store crate 的 trait 層面做 cache。
Sail 已經有 `DynamicObjectStoreRegistry`，可以在這裡注入 caching layer。

```
                 DataFusion
                     |
                 ObjectStore trait
                     |
             +-------------------+
             | CachingObjectStore |  <-- 新增
             +-------------------+
             |  Foyer Hybrid     |
             |  Cache            |
             +---+----------+----+
                 |          |
              hit: local  miss: delegate
                           |
                     +-------------+
                     | S3/GCS/ADLS |
                     +-------------+
```

實作重點：
- 新增 `CachingObjectStore` 實作 `ObjectStore` trait
- 內部持有一個 Foyer HybridCache + 一個 inner ObjectStore
- get/get_range 查 cache，miss 時 delegate 到 inner 並回填
- put/delete 同時 invalidate cache
- key 可以用 (path, range) 或 (path, offset, length)

好處：
- 對 DataFusion 完全透明，不需要改任何 query plan 邏輯
- 適用於所有格式（Parquet, CSV, JSON, Delta, Iceberg）
- 類似 AWS S3 cache、GCS FUSE 的概念但更可控

挑戰：
- 需要處理 cache invalidation（ETags, Last-Modified）
- range read 的 cache 粒度需要設計（按 block？按 column chunk？）
- cluster mode 每個 worker 有獨立的 local cache，需要考慮 cache 一致性

🔸 方案 B：物理執行計畫包裝（類似 LiquidCache）

新增一個 `CachedScanExec` 物理計畫節點，包裝原本的 scan operator。

```
                Before                          After
         ParquetExec(s3://...)           CachedScanExec
                                              |
                                    hit: read from cache
                                    miss: ParquetExec(s3://...) + 回填 cache
```

這類似 datafusion-distributed 的 BroadcastExec 做法：

```rust
// datafusion-distributed/src/execution_plans/broadcast.rs
pub struct BroadcastExec {
    input: Arc<dyn ExecutionPlan>,
    partitions: Arc<OnceLock<Vec<Arc<RecordBatchQueue>>>>,
    ...
}
```

BroadcastExec 用 OnceLock 快取 input 的所有 partition 結果，
後續 partition 直接從 queue 讀。

好處：
- 可以做到 query-aware（pushdown filter/projection 到 cache 層）
- 快取的粒度是 RecordBatch，對 DataFusion 更友好

挑戰：
- 需要 optimizer rule 注入 CachedScanExec 到 plan tree
- 不同查詢的 filter/projection 不同，cache key 設計更複雜
- 實作成本比方案 A 高很多

🔸 方案 C：Query Result Cache（類似 datafusion-query-cache）

快取完整查詢或部分聚合的結果。

```
SELECT region, SUM(sales)
FROM orders
WHERE ts BETWEEN '2024-01-01' AND '2024-12-31'
GROUP BY region
```

第一次執行完整計算，快取結果。
第二次如果只有 ts 範圍變化（如 '2024-01-01' ~ '2025-03-01'），
只需要計算新增部分再跟 cache 合併。

datafusion-query-cache 的做法是透過：
- 自訂 QueryPlanner 改寫 logical plan
- 自訂 OptimizerRule 分析哪些部分可以快取
- 自訂 ExecutionPlan（CacheUpdateAggregateExec）在執行後存結果
- 利用 DataFusion 的 partial aggregation 機制做增量合併

好處：
- 對重複查詢效果最好，直接跳過所有計算
- 適合 dashboard / BI 場景（同一個查詢反覆跑）

挑戰：
- cache invalidation 最複雜（底層資料變了怎麼辦？）
- 只適用於聚合查詢，不通用
- 實作成本最高


## 建議實作策略

```
Phase 1 (Issue #1015)     Phase 2               Phase 3
+-----------------+    +------------------+    +-----------------+
| CachingObject   |    | Spark Cache API  |    | Query Result    |
| Store + Foyer   |    | (df.cache()      |    | Cache           |
|                 |    |  CACHE TABLE)    |    | (partial agg    |
| ObjectStore     |    | in-memory Arrow  |    |  reuse)         |
| trait 層包裝    |    | batch cache      |    |                 |
+-----------------+    +------------------+    +-----------------+
  最大效益/最小成本     Spark 相容性必須       進階優化
```

🔸 Phase 1: CachingObjectStore + Foyer（對應 #1015）

建議位置：新增 crate `sail-object-store-cache` 或直接在 `sail-object-store` 內。

核心介面：

```rust
pub struct CachingObjectStore {
    inner: Arc<dyn ObjectStore>,
    cache: HybridCache<CacheKey, Bytes>,
}

#[derive(Hash, Eq, PartialEq)]
struct CacheKey {
    path: Path,
    range: Option<Range<usize>>,
}

impl ObjectStore for CachingObjectStore {
    async fn get_opts(&self, location: &Path, options: GetOptions) -> Result<GetResult> {
        // 1. check cache
        // 2. miss -> delegate to inner
        // 3. fill cache
        // 4. return
    }
}
```

在 `DynamicObjectStoreRegistry` 裡面把原本的 ObjectStore 包一層：

```rust
// sail-object-store/src/lib.rs (概念)
fn get_store(&self, url: &Url) -> Arc<dyn ObjectStore> {
    let inner = self.resolve_inner(url);
    if self.cache_enabled {
        Arc::new(CachingObjectStore::new(inner, self.foyer_cache.clone()))
    } else {
        inner
    }
}
```

🔸 Phase 2: Spark Cache API

實作 `df.cache()` / `df.persist()` / `CACHE TABLE`。
需要一個 session-scope 的 cache store，key 是 plan fingerprint，
value 是 `Vec<RecordBatch>`。

這不是 Issue #1015 的範圍，但是 Spark 相容性的必要功能。

🔸 Phase 3: Query Result Cache

參考 datafusion-query-cache 的做法，針對聚合查詢做增量快取。
優先度最低，等 Phase 1 和 Phase 2 穩定後再考慮。


## 外部方案參考

🔸 LiquidCache（https://github.com/XiangpengHao/liquid-cache）

定位：DataFusion 的 distributed pushdown cache，VLDB 2026 論文。

核心理念：快取「邏輯資料」而不是「物理表示」。
把 S3 資料（Parquet/CSV/JSON）轉碼為自研格式，
更壓縮、更適合 NVMe、更高效的 DataFusion 運算。

```
+-------------------+
| DataFusion Node A |
+--------+----------+
         |  Arrow Flight
         v
+-------------------+
| LiquidCache       |
| Server            |
| (transcoded data) |
+--------+----------+
         |  miss
         v
+-------------------+
| S3 / Object Store |
+-------------------+
```

特性：
- 支援 filter/projection pushdown 到 cache server
- 兩層：memory（只存查詢相關欄位）+ SSD（完整資料）
- 多個 DataFusion node 共享一個 LiquidCache instance
- 透過 Arrow Flight protocol 通訊

跟 Sail 的關係：LiquidCache 解決的問題跟 #1015 類似但更激進。
Sail 如果用 Foyer 做 ObjectStore 層 cache 是比較保守穩定的選擇。
LiquidCache 的 pushdown 概念可以作為未來進階方案的參考。

🔸 datafusion-query-cache（https://github.com/datafusion-contrib/datafusion-query-cache）

定位：DataFusion 的 time-series query result cache。

核心理念：利用 DataFusion 的 partial aggregation 機制。
第一次執行存下 partial aggregate state，
第二次只計算新資料再跟 cached state 合併。

實作方式：
- 自訂 QueryPlanner + OptimizerRule
- UserDefinedLogicalNodeCore 定義新的 logical plan node
- CacheUpdateAggregateExec 在執行時寫入 cache
- UnionExec 合併 cached + new 資料

跟 Sail 的關係：這是 Phase 3 的參考，不是 #1015 的直接需求。

🔸 datafusion-distributed BroadcastExec

定位：分散式 join 時的 batch cache。

```rust
// 用 OnceLock 快取 partition 結果
pub struct BroadcastExec {
    input: Arc<dyn ExecutionPlan>,
    partitions: Arc<OnceLock<Vec<Arc<RecordBatchQueue>>>>,
}
```

第一個 partition 執行 input 並存到 queue，其他 partition 直接讀。
這是 broadcast join 的標準做法，Sail 可以參考做類似的 broadcast cache。
