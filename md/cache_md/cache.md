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

🔸 為什麼 Sail 不用 DataFusion 預設的 cache，要自己用 Moka 實作

DataFusion 本身有提供預設實作（DefaultFilesMetadataCache、DefaultListFilesCache），
但 Sail 全部替換成 Moka 版本。原因有三個：

原因 1：並行效能（Mutex vs lock-free）

```rust
// DataFusion 預設：用 Mutex 包裝
pub struct DefaultFilesMetadataCache {
    state: Mutex<DefaultFilesMetadataCacheState>,  // 每次 get/put 都要搶鎖
}

// Sail 替換：用 Moka（lock-free concurrent cache）
pub struct MokaFileMetadataCache {
    metadata: Cache<Path, CachedFileMetadataEntry>,  // 內部用 crossbeam + 分段鎖
}
```

DataFusion 的 Mutex 在高並行場景（多個 Worker thread 同時讀 Parquet）會成為瓶頸。
Moka 內部用類似 Java Caffeine 的分段鎖 + lock-free read path，
多個 thread 同時 get() 不會互相阻塞。

Sail 的場景特別需要這個：
- LocalCluster 模式 4 個 Worker thread 共用同一個 cache
- 每個 Worker 同時開多個 Parquet 檔案
- Mutex 會讓 get() 變成序列化瓶頸

原因 2：TTL 支援

```rust
// DataFusion 預設：沒有 TTL
// entry 只靠 LRU 驅逐 + is_valid_for() 被動判斷

// Sail 的 Moka：可以設 TTL
builder = builder.time_to_live(Duration::from_secs(ttl));
```

DataFusion 的預設 cache 沒有 TTL，entry 永遠不會自動過期。
它靠每次查詢時 is_valid_for() 比對 ObjectMeta 來被動判斷。
但如果 listing cache 本身沒有 TTL，就永遠發現不了檔案被改了。

Sail 的 Moka 加了 TTL 作為保底機制（預設 60 秒），
確保即使 is_valid_for() 沒被觸發，entry 也會自動過期。

原因 3：Cache mode 彈性

Sail 支援三種 cache mode（application.yaml 可設定）：

```yaml
cache:
  type: "global"    # global / session / none
```

```
+----------+--------------------------------------------+
| Mode     | 行為                                       |
+----------+--------------------------------------------+
| global   | 所有 session 共用一個 cache instance        |
|          | cache 累積效果最好                          |
+----------+--------------------------------------------+
| session  | 每個 session 獨立的 cache instance          |
|          | session 結束 cache 就消失                   |
+----------+--------------------------------------------+
| none     | 不 cache                                   |
+----------+--------------------------------------------+
```

DataFusion 的預設 cache 沒有這個切換機制。
Sail 用 Moka 自己建 cache instance，可以根據 config 決定共不共享。

三個 cache 的共同改進對照：

```
+-----------------------------+----------------------------+-------------------+
|                             | DataFusion 預設            | Sail (Moka)       |
+-----------------------------+----------------------------+-------------------+
| 並行策略                    | Mutex（每次操作搶鎖）      | lock-free read    |
|                             |                            | + 分段鎖 write    |
+-----------------------------+----------------------------+-------------------+
| 驅逐策略                    | LRU                        | LRU               |
+-----------------------------+----------------------------+-------------------+
| TTL                         | 沒有                       | 可設定（秒）      |
+-----------------------------+----------------------------+-------------------+
| 記憶體上限                  | 有（50 MB）                | 有（可設定）      |
+-----------------------------+----------------------------+-------------------+
| 權重計算                    | memory_size()              | memory_size()     |
+-----------------------------+----------------------------+-------------------+
| Cache mode                  | 無（全域唯一）             | global/session    |
|                             |                            | /none             |
+-----------------------------+----------------------------+-------------------+
| 過期判斷                    | is_valid_for()             | is_valid_for()    |
|                             | (被動，靠 ObjectMeta)      | + TTL（主動）     |
+-----------------------------+----------------------------+-------------------+
```

注意：Sail 沒有改 DataFusion 的 cache trait 或 is_valid_for() 邏輯，
只是提供了不同的 trait 實作（MokaFileMetadataCache 取代 DefaultFilesMetadataCache）。
DataFusion 的 CacheAccessor trait 設計得很好，允許外部提供自己的 cache 實作。

🔸 CachingObjectStore 能不能重複使用 DataFusion 的 ObjectMeta 做 invalidation

DataFusion 的 metadata cache 用 is_valid_for() 比對 ObjectMeta 來判斷過期：

```rust
pub fn is_valid_for(&self, current_meta: &ObjectMeta) -> bool {
    self.meta.size == current_meta.size
        && self.meta.last_modified == current_meta.last_modified
}
```

這個 current_meta 是 list directory 時順便拿到的，不需要額外的 S3 請求。
問題是：CachingObjectStore 在 ObjectStore trait 層，拿不到這個 ObjectMeta。

原因是查詢的兩個階段之間沒有傳遞 ObjectMeta：

```
Phase 0: list directory（DataFusion planning 層）
    ObjectStore::list("s3://bucket/orders/")
    → Vec<ObjectMeta> { path, size, last_modified, e_tag }
    → ObjectMeta 留在 ListingTable / ParquetExec 的 file_groups 裡

Phase 1: read metadata / footer（DataFusion planning 層）
    DFParquetMetadata::fetch_metadata()
    → 有 ObjectMeta（從 Phase 0 傳下來）
    → is_valid_for() 可以用

Phase 2: read data / column chunks（ObjectStore trait 層）
    ObjectStore::get_range(path, 13MB..16MB)
    → 只有 Path + byte range
    → 沒有 ObjectMeta
    → CachingObjectStore 在這裡
```

ObjectStore trait 的簽名決定了這個限制：

```rust
// object_store crate
#[async_trait]
pub trait ObjectStore {
    async fn get_opts(
        &self,
        location: &Path,          // 只有 path
        options: GetOptions,       // 只有 range + 條件式 header
    ) -> Result<GetResult>;
    // 沒有 ObjectMeta 參數
}
```

DataFusion 的 planning 層有 ObjectMeta，但它呼叫 ObjectStore::get_range() 時
只傳 path + byte range 下去，不傳 ObjectMeta。
兩層之間沒有直接的資料傳遞通道。

一個直覺的想法是：DataFusion Phase 0 已經呼叫了 ObjectStore::list()，
這個 list 會穿過 CachingObjectStore，能不能直接攔截 list 結果拿到 ObjectMeta？

答案是：有時候可以，有時候不行。
因為 listing cache（MokaFileListingCache）擋在 ObjectStore 前面：

```
// crates/sail-data-source/src/listing.rs (line 198-213)

DataFusion ListingTable 要列出檔案
    |
    v
先查 file_listing_cache（Moka，在 CacheManager 裡，不在 ObjectStore 鏈上）
    |
    +→ HIT: 直接回傳 Vec<ObjectMeta>
    |       ObjectStore::list() 根本不被呼叫
    |       CachingObjectStore 完全看不到
    |
    +→ MISS: 呼叫 store.list()
             → LoggingObjectStore::list()
             → CachingObjectStore::list()    ← 可以攔截！
             → RuntimeAwareObjectStore::list()
             → S3 LIST
             → 結果存進 file_listing_cache
```

listing cache HIT 時，ObjectStore::list() 不會被呼叫，
CachingObjectStore 看���到任何 ObjectMeta。

所以 CachingObjectStore 做 invalidation 有四條路：

```
+------+-------------------------------+-------------------+-------------------+
| 方案 | 做法                          | 額外成本          | 適用場景          |
+------+-------------------------------+-------------------+-------------------+
| A    | 不做 invalidation             | 零                | Data Lake         |
|      | 檔案 immutable，不需要驗證    |                   | (Delta/Iceberg)   |
|      |                               |                   | 檔案永遠不變     |
+------+-------------------------------+-------------------+-------------------+
| B    | TTL 保底                      | 零                | 所有場景          |
|      | Foyer 設 TTL，過期自動重讀    | (只是 TTL 到期    | 簡單有效          |
|      |                               |  後 miss 一次)    |                   |
+------+-------------------------------+-------------------+-------------------+
| C    | 攔截 list() 拿 ObjectMeta     | 零（搭順風車）    | list MISS 時有效  |
|      | 在 CachingObjectStore::list() |                   | list HIT 時       |
|      | 記住每個檔案的 ObjectMeta     |                   | 看不到，要 fallba |
|      | get_range() 時比對驗證        |                   | ck 到 B 或 D      |
+------+-------------------------------+-------------------+-------------------+
| D    | CachingObjectStore 自己       | 每個檔案的 TTL    | raw S3 檔案       |
|      | 維護 ObjectMeta cache         | 週期一次 HEAD     | (會被覆蓋的)      |
|      | 第一次 head(path) 拿到後      | (不是每次 cache   |                   |
|      | cache 住，TTL 到期再刷新      | hit 都發 HEAD)    |                   |
+------+-------------------------------+-------------------+-------------------+
```

方案 C 的細節：

```
CachingObjectStore 內部維護一個 ObjectMeta cache：
  meta_cache: HashMap<Path, ObjectMeta>

impl ObjectStore for CachingObjectStore {
    fn list(&self, prefix: Option<&Path>) -> BoxStream<Result<ObjectMeta>> {
        // 攔截 list 結果，記住每個檔案的 ObjectMeta
        let stream = self.inner.list(prefix);
        stream.inspect(|result| {
            if let Ok(meta) = result {
                self.meta_cache.insert(meta.location.clone(), meta.clone());
            }
        })
    }

    async fn get_range(&self, location: &Path, range: Range<usize>) -> Result<Bytes> {
        // 查 data cache
        if let Some(cached) = self.data_cache.get(location, range) {
            // 有 ObjectMeta 可以比對嗎？
            if let Some(cached_meta) = self.meta_cache.get(location) {
                // 有 → 用 is_valid_for() 驗證
                // （但 cached_meta 可能是舊的，取決於 list 多久前被呼叫）
            }
            // 沒有 → 跳過驗證，靠 TTL 保底
            return cached;
        }
        // cache miss → 讀 S3
    }
}
```

但方案 C 有一個微妙問題：
listing cache HIT 時 CachingObjectStore 看不到 list()，
meta_cache 裡的 ObjectMeta 可能是很久之前存的。
這跟 TTL 保底的效果差不多。

方案 D 比 C 更可靠：

```
CachingObjectStore 內部有 ObjectMeta cache（也用 Foyer / Moka）：

第一次讀 file X：
  → head(path) 拿 ObjectMeta（1 個 HEAD 請求）
  → 存進 meta_cache
  → 從 S3 讀 data，存進 data_cache

第二次讀 file X（同 path，不同 range）：
  → meta_cache HIT → 不需要 HEAD
  → 比對 ObjectMeta → data cache 有效 → 直接用

meta_cache TTL 到期後：
  → 重新 head(path)，1 個 HEAD
  → 比對 → 一致就繼續用 data cache，不一致就 invalidate

成本：每個檔案的 TTL 週期（如 1 小時）一次 HEAD
      不是每次 get_range() 一次 HEAD
```

🔸 方案 D 的一致性限制

方案 D 是「最終一致」，不是「強一致」。
meta_cache TTL 期間內，如果 S3 上的檔案被覆蓋了，CachingObjectStore 不會立即發現。

```
時間線範例（meta_cache TTL = 1 小時）：

t=0    第一次讀 file X → HEAD → ObjectMeta(size=100, modified=t0)
       → 存進 meta_cache → 讀 data → 存進 data_cache

t=20m  有人覆蓋 file X（新 size=150, modified=t20m）

t=30m  第二次讀 file X
       → meta_cache HIT（TTL 還沒到）
       → 拿到舊的 ObjectMeta(size=100, modified=t0)
       → 比對 data_cache → 一致 → 用了過時的 data ❌

t=60m  meta_cache TTL 到期
       → 重新 HEAD → 發現 ObjectMeta 變了
       → invalidate data_cache → 讀新資料 ✅

過期資料的最大延遲 = meta_cache TTL
```

這跟 DataFusion listing cache 的限制一樣（見 datafusion_cache.md）：
listing cache TTL 未到期 → is_valid_for() 拿舊 ObjectMeta 比 → 還是一致 → 過期資料。
兩者本質相同，只是一個在 listing 層，一個在 data 層。

實務上通常可接受：
- Delta Lake / Iceberg 檔案 immutable，不會被覆蓋，TTL 多長都沒差（方案 A 就夠）
- raw S3 Parquet 被覆蓋的頻率通常很低，1 小時 TTL 的過期窗口對 batch analytics 可接受
- 如果需要更快偵測變更，把 meta_cache TTL 調短（如 5 分鐘），代價是更多 HEAD 請求

這跟 OCRA 的做法一樣：OCRA 的 PageCache trait 有 head() 方法，
ReadThroughCache 在 get_range() 時先呼叫 cache.head() 拿 ObjectMeta
（cache 住的就直接用，miss 就發 HEAD），然後才讀 data。

建議的組合策略：

```
+---------------------+-------------------------------------------------+
| 場景                | 策略                                            |
+---------------------+-------------------------------------------------+
| Delta Lake /        | 方案 A：不驗證                                   |
| Iceberg             | 檔案 immutable，metadata 層保證只讀有效檔案     |
+---------------------+-------------------------------------------------+
| raw S3 Parquet      | 方案 B + C：TTL 保底 + 攔截 list                 |
| (偶爾被覆蓋)        | list MISS 時攔截 ObjectMeta，list HIT 時靠 TTL  |
+---------------------+-------------------------------------------------+
| raw S3 Parquet      | 方案 B + D：TTL 保底 + ObjectMeta cache          |
| (頻繁被覆蓋)        | 每個 TTL 週期一次 HEAD 確認                      |
+---------------------+-------------------------------------------------+
```

Phase 1 實作建議先做 A + B（最簡單），
後續如果使用者反映 raw S3 場景有過期問題再加 C 或 D。

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


## Key / Value 設計：為什麼 Cache 不需要懂 Parquet

一個常見的疑問是：S3 上是 Parquet，cache 要怎麼知道裡面有什麼？
value 要存完整的 Parquet 還是切開？

答案是：cache 完全不需要知道「這是 Parquet」。
DataFusion 的 Parquet reader 已經在上層把讀取切好了，
cache 只看到一連串的 byte range GET，只需要快取 raw bytes。

🔸 Parquet 讀取流程

假設 S3 上有一個 Parquet 檔案：

```
s3://bucket/orders.parquet (100 MB)

Parquet 內部 layout:
+================================================================+
| Row Group 0                                                     |
|  Col: order_id  (offset: 0,     length: 5 MB)                 |
|  Col: customer  (offset: 5MB,   length: 8 MB)                 |
|  Col: amount    (offset: 13MB,  length: 3 MB)                 |
|  Col: region    (offset: 16MB,  length: 2 MB)                 |
|  Col: timestamp (offset: 18MB,  length: 4 MB)                 |
+----------------------------------------------------------------+
| Row Group 1                                                     |
|  Col: order_id  (offset: 22MB,  length: 5 MB)                 |
|  ...                                                            |
+----------------------------------------------------------------+
| ...                                                             |
+----------------------------------------------------------------+
| Footer (offset: 99.9MB, length: 100 KB)  <-- schema + stats   |
+================================================================+
```

當你執行 `SELECT order_id, amount FROM orders WHERE region = 'US'` 時，
DataFusion 的 Parquet reader 發出這些 ObjectStore 請求：

```
1. GET orders.parquet  range: 99.9MB-100MB   (讀 footer，拿到 schema + 統計)
2. (看 footer 的 stats，發現 Row Group 0 可能有 region='US')
3. GET orders.parquet  range: 16MB-18MB      (讀 region column chunk 做 filter)
4. (掃描 region column，找到符合的 row index)
5. GET orders.parquet  range: 0-5MB          (讀 order_id column chunk)
6. GET orders.parquet  range: 13MB-16MB      (讀 amount column chunk)
```

DataFusion 從不讀 customer 和 timestamp 欄位。
cache 層完全不需要知道 Parquet 結構，它只看到 byte range GET。

🔸 推薦方案：固定大小 Part（跟 Alluxio / SlateDB 一致）

把每個檔案切成固定大小的 part（例如 1 MB），以 (path, part_id) 為 key：

```
Key:   (path: "s3://bucket/orders.parquet", part_id: 0)
Value: bytes[0 .. 1MB]          <-- raw bytes，不是 Parquet 結構

Key:   (path: "s3://bucket/orders.parquet", part_id: 1)
Value: bytes[1MB .. 2MB]

Key:   (path: "s3://bucket/orders.parquet", part_id: 99)
Value: bytes[99MB .. 100MB]     <-- 包含 footer
```

🔸 GET 流程

```
DataFusion 請求: GET orders.parquet range: 13MB-16MB (amount column)
                          |
                          v
              CachingObjectStore
                          |
              切成 parts:
              part_id=13 (13MB-14MB)  -> cache lookup
              part_id=14 (14MB-15MB)  -> cache lookup
              part_id=15 (15MB-16MB)  -> cache lookup
                          |
              +------+--------+-------+
              |      |        |       |
              v      v        v       |
            HIT    HIT      MISS     |
            (local)(local)    |       |
                              v       |
                    fetch from S3     |
                    save to cache     |
                              |       |
              +------+--------+-------+
              |
              v
          combine 3 parts -> return bytes[13MB-16MB]
```

🔸 為什麼不用 (path, column_name) 當 key

```
方案 X（不推薦）：Parquet-aware key

Key:   (path, row_group=0, column="amount")
Value: 解壓後的 column chunk

問題：
1. cache 層必須理解 Parquet 格式（也要理解 CSV, JSON, ORC, Delta...）
2. ObjectStore trait 的介面是 get(path, byte_range)，沒有 column 概念
3. 不同格式要寫不同的 cache 邏輯
4. 解壓後的資料比 raw bytes 大很多，浪費 cache 空間
```

🔸 為什麼不存完整 Parquet 檔案

```
方案 Y（不推薦）：存完整檔案

Key:   path
Value: 完整 100MB parquet file

問題：
1. 只查 2 個欄位卻 cache 100MB，浪費
2. 大檔案寫入 cache 慢
3. 小查詢也要等整個檔案 cache 好
4. cache 容量被少數大檔案佔滿
```

🔸 完整查詢流程（含 cache warm-up）

```
=== 第一次查詢 ===

SELECT order_id, amount FROM orders WHERE region = 'US'
    |
    v
DataFusion Parquet Reader
    |
    | (1) get_range(orders.parquet, 99.9MB-100MB)  -- footer
    v
CachingObjectStore
    +---> Foyer: lookup part_id=99
    |     MISS -> S3 GET -> save part_id=99 -> return footer bytes
    v
DataFusion reads footer, decides to read Row Group 0
    |
    | (2) get_range(orders.parquet, 16MB-18MB)  -- region column
    v
CachingObjectStore
    +---> Foyer: lookup part_id=16  MISS -> S3 -> save -> return
    +---> Foyer: lookup part_id=17  MISS -> S3 -> save -> return
    v
DataFusion filters region='US', finds matching rows
    |
    | (3) get_range(orders.parquet, 0-5MB)   -- order_id
    | (4) get_range(orders.parquet, 13-16MB) -- amount
    v
CachingObjectStore
    +---> part 0-4:   MISS -> S3 -> save
    +---> part 13-15: MISS -> S3 -> save
    v
Return results to user
(total: 12 parts cached, 12 S3 GETs)


=== 第二次執行同一個查詢 ===

    | (1) get_range footer     -> part_id=99   HIT (cache)
    | (2) get_range region     -> part 16,17   HIT (cache)
    | (3) get_range order_id   -> part 0-4     HIT (cache)
    | (4) get_range amount     -> part 13-15   HIT (cache)
    |
    全部從 local cache 讀，零 S3 請求


=== 第三次：不同查詢但重疊欄位 ===

SELECT order_id, customer FROM orders WHERE region = 'US'
    |
    | footer     -> part_id=99   HIT (之前已 cache)
    | region     -> part 16,17   HIT (之前已 cache)
    | order_id   -> part 0-4     HIT (之前已 cache)
    | customer   -> part 5-12    MISS -> S3 -> save (新欄位)
    |
    只有 customer 欄位需要讀 S3，其他全部 cache hit
```

🔸 建議的 Sail 實作

```rust
pub struct CachingObjectStore {
    inner: Arc<dyn ObjectStore>,
    cache: HybridCache<PartKey, Bytes>,  // Foyer hybrid (memory + disk)
    part_size: usize,                     // 建議 1 MB
}

#[derive(Clone, Hash, Eq, PartialEq)]
struct PartKey {
    path: String,      // "s3://bucket/orders.parquet"
    part_id: u64,      // offset / part_size
}

// Value 就是 raw bytes，不做任何解析
// Foyer 負責 memory vs disk 的 tiering
```

為什麼 part_size 建議 1 MB：
- Parquet column chunk 通常 1-10 MB，1 MB part 能精準對齊
- Alluxio 預設也是 1 MB page
- 太小（如 64 KB）會產生大量 key，增加 metadata 開銷
- 太大（如 16 MB）會浪費空間（只需要 1 MB 卻 cache 16 MB）


## 不需要改 DataFusion：注入點在哪

完全不需要修改 DataFusion 的 Parquet reader 源碼。
DataFusion 透過 `ObjectStore` trait 讀取檔案，
Sail 只需要在注入 ObjectStore 的地方多包一層 CachingObjectStore。

🔸 現有的 ObjectStore 包裝鏈

Sail 已經有多層 ObjectStore 包裝（decorator pattern）：

```
// crates/sail-object-store/src/registry.rs (line 164)
fn get_dynamic_object_store(url: &Url) -> Result<Arc<dyn ObjectStore>> {
    let store = match scheme {
        AmazonS3  => Arc::new(LazyObjectStore::new(|| get_s3_object_store(url))),
        Azure     => Arc::new(LazyObjectStore::new(|| get_azure_object_store(url))),
        GCS       => Arc::new(LazyObjectStore::new(|| get_gcs_object_store(url))),
        ...
    };
    //                 vvvvvvvvvvvvvvvvvvvv  最外層包 LoggingObjectStore
    Ok(Arc::new(LoggingObjectStore::new(store)))
}
```

目前的包裝鏈：

```
DataFusion ParquetExec
    |
    | calls ObjectStore::get_opts(path, range)
    v
LoggingObjectStore          <-- 記 log + tracing span
    |
    v
RuntimeAwareObjectStore     <-- 切換到 IO runtime（選配）
    |
    v
LazyObjectStore             <-- 延遲初始化（第一次用才建連線）
    |
    v
S3 / Azure / GCS client    <-- 真正的 HTTP 請求
```

🔸 CachingObjectStore 要插在哪

插在 LoggingObjectStore 和 inner store 之間：

```
DataFusion ParquetExec
    |
    | calls ObjectStore::get_opts(path, range)
    v
LoggingObjectStore              <-- 記 log（含 cache hit/miss 都會記到）
    |
    v
CachingObjectStore              <-- 新增：查 Foyer cache
    |                                hit -> 直接回傳 local bytes
    |                                miss -> 往下走
    v
RuntimeAwareObjectStore
    |
    v
LazyObjectStore
    |
    v
S3 / Azure / GCS client
```

🔸 需要改的檔案

只需要改 2 個檔案 + 新增 1 個檔案：

```
crates/sail-object-store/
├── src/
│   ├── layers/
│   │   ├── mod.rs              <-- (改) 加 pub mod caching;
│   │   ├── caching.rs          <-- (新增) CachingObjectStore 實作
│   │   ├── logging.rs          <-- 不動
│   │   ├── lazy.rs             <-- 不動
│   │   └── runtime.rs          <-- 不動
│   ├── registry.rs             <-- (改) 包一層 CachingObjectStore
│   └── lib.rs                  <-- 不動
└── Cargo.toml                  <-- (改) 加 foyer dependency
```

🔸 registry.rs 只改一行

```rust
// crates/sail-object-store/src/registry.rs
// 改動前 (line 164):
Ok(Arc::new(LoggingObjectStore::new(store)))

// 改動後:
let store = if cache_enabled {
    Arc::new(CachingObjectStore::new(store, foyer_cache.clone()))
} else {
    store
};
Ok(Arc::new(LoggingObjectStore::new(store)))
```

🔸 caching.rs 的核心結構

參考現有的 `LoggingObjectStore` 的 decorator pattern（同一個 layers 目錄）：

```rust
// crates/sail-object-store/src/layers/caching.rs
use foyer::HybridCache;

pub struct CachingObjectStore {
    inner: Arc<dyn ObjectStore>,
    cache: HybridCache<PartKey, Bytes>,
    part_size: usize,  // 1 MB
}

#[async_trait::async_trait]
impl ObjectStore for CachingObjectStore {
    async fn get_opts(&self, location: &Path, options: GetOptions) -> Result<GetResult> {
        // 1. 把 range 切成 parts
        // 2. 每個 part 查 cache
        // 3. miss 的 part -> self.inner.get_opts() 讀 S3
        // 4. 回填 cache
        // 5. 組合回傳
    }

    async fn put_opts(&self, location: &Path, payload: PutPayload, opts: PutOptions)
        -> Result<PutResult>
    {
        // 直接 delegate 到 inner，不 cache PUT
        self.inner.put_opts(location, payload, opts).await
    }

    async fn delete(&self, location: &Path) -> Result<()> {
        // delegate + invalidate cache
        self.cache.remove(&location_to_prefix(location));
        self.inner.delete(location).await
    }

    // list, copy, rename -> 直接 delegate
}
```

🔸 DataFusion 完全不知道 cache 的存在

```
DataFusion 視角（完全沒變）：

let store = runtime_env.object_store("s3://bucket/")?;  // 拿到 ObjectStore
let data = store.get_range(&path, 13MB..16MB).await?;    // 讀 bytes

DataFusion 不知道這個 store 是：
  LoggingObjectStore(
    CachingObjectStore(          <-- 新加的，DataFusion 看不到
      RuntimeAwareObjectStore(
        LazyObjectStore(
          S3 client
        )
      )
    )
  )

就像 LoggingObjectStore 記 log 不影響 DataFusion，
CachingObjectStore 做 cache 也不影響 DataFusion。
這就是 ObjectStore trait 的 decorator pattern 的優勢。
```

🔸 完整調用鏈

```
PySpark: spark.sql("SELECT order_id, amount FROM orders WHERE region = 'US'")
    |
    v
sail-spark-connect (gRPC)
    |
    v
sail-plan (logical plan -> physical plan)
    |
    v
DataFusion ParquetExec
    |
    | (1) store.get_range(orders.parquet, 99.9MB-100MB)  // footer
    v
ObjectStoreRegistry::get_store("s3://bucket/")
    |
    v
LoggingObjectStore::get_opts()      // log: "get orders.parquet 99.9MB-100MB"
    |
    v
CachingObjectStore::get_opts()      // part_id=99 -> Foyer lookup
    |                                   HIT  -> return cached bytes
    |                                   MISS -> 往下走
    v
RuntimeAwareObjectStore::get_opts() // 切到 IO runtime
    |
    v
LazyObjectStore::get_opts()         // 第一次用才初始化 S3 client
    |
    v
S3 client::get_range()              // HTTP GET with Range header
    |
    v
CachingObjectStore                  // 回填：save part_id=99 to Foyer
    |
    v
LoggingObjectStore                  // log: "get complete, 100KB, 45ms"
    |
    v
DataFusion ParquetExec              // 拿到 footer bytes，解析 schema + stats
    |
    | (2) store.get_range(orders.parquet, 16MB-18MB)  // region column
    | (3) store.get_range(orders.parquet, 0-5MB)      // order_id column
    | (4) store.get_range(orders.parquet, 13MB-16MB)  // amount column
    v
... 同樣的流程，每個 range 獨立走 cache ...
    |
    v
Return RecordBatch to user
```


## 分散式 Cache 一致性 與 有效性判斷

Sail 是分散式引擎，Driver + 多個 Worker 各自有獨立的 RuntimeEnv。
每個 Worker 建立自己的 `WorkerSessionFactory` → `RuntimeEnvFactory` → `ObjectStoreRegistry`，
所以每個 Worker 會有自己獨立的 CachingObjectStore + Foyer cache。

```
+------------------+     +------------------+     +------------------+
| Worker 0         |     | Worker 1         |     | Worker 2         |
| +--------------+ |     | +--------------+ |     | +--------------+ |
| | Foyer Cache  | |     | | Foyer Cache  | |     | | Foyer Cache  | |
| | (local NVMe) | |     | | (local NVMe) | |     | | (local NVMe) | |
| +--------------+ |     | +--------------+ |     | +--------------+ |
+--------+---------+     +--------+---------+     +--------+---------+
         |                        |                        |
         v                        v                        v
+---------------------------------------------------------------+
|                        S3 / GCS / ADLS                        |
+---------------------------------------------------------------+
```

問題一：Worker 之間的 cache 不共享，會不會有一致性問題？
問題二：怎麼知道 cache 裡的資料還是有效的？

🔸 為什麼 Data Lake 場景不需要跨 Worker 一致性

Data Lake 的檔案（Parquet）是 write-once、immutable 的：

```
寫入:  s3://bucket/orders/part-00001.parquet  (一次寫入，永不修改)
       s3://bucket/orders/part-00002.parquet
       s3://bucket/orders/part-00003.parquet

刪除:  透過 Delta Lake / Iceberg metadata 標記為「不再使用」
       實際檔案可能還在 S3 上，但不會被新查詢讀到
```

因為檔案是 immutable 的：
- 同一個 path + 同一個 byte range 永遠回傳同樣的 bytes
- Worker 0 cache 了 part-00001.parquet 的 part_id=5
- Worker 1 也 cache 了 part-00001.parquet 的 part_id=5
- 兩者的內容保證一模一樣，不需要同步

這就是為什麼 Trino + Alluxio 的嵌入式模式（每個 worker 獨立 cache）
在生產環境中完全沒問題。

🔸 Delta Lake / Iceberg 怎麼避免讀到過期資料

```
                   Transaction Log (Delta) / Manifest (Iceberg)
                              |
                              v
                   "目前有效的檔案清單"
                   - part-00001.parquet  (active)
                   - part-00002.parquet  (active)
                   - part-00003.parquet  (removed by compaction)
                              |
                              v
                   DataFusion 只會讀 part-00001 和 part-00002
                   part-00003 的 cache 自然不會被訪問
                   最終被 LRU 驅逐
```

流程：
1. 查詢開始時，Driver 讀 Delta/Iceberg metadata，得到「目前有效的檔案清單」
2. 只有清單上的檔案會被分配給 Worker 執行
3. 被 compaction 刪除的舊檔案不會出現在清單中
4. 這些舊檔案的 cache 不會被讀取，最終被 Foyer 的 LRU 自動驅逐

結論：不需要主動 invalidate，metadata 層已經保證了正確性。

🔸 非 Data Lake 場景（raw Parquet on S3）

如果使用者直接覆蓋 S3 上的 Parquet 檔案（而不是用 Delta/Iceberg），
就會有 cache 過期的風險：

```
時間線：
T1: Worker 讀 s3://bucket/data.parquet -> cache 填入
T2: 外部程式覆蓋 s3://bucket/data.parquet (新資料)
T3: Worker 再讀 -> cache HIT -> 拿到舊資料 (錯誤!)
```

解法有三種，可以依需求組合：

```
+---------------------+-------------------------------------+----------+
| 策略                | 做法                                | 成本     |
+---------------------+-------------------------------------+----------+
| TTL (最簡單)        | 每個 cache entry 設定存活時間        | 零       |
|                     | 過期後下次讀取重新從 S3 拉           |          |
|                     | Foyer 原生支援 TTL                  |          |
+---------------------+-------------------------------------+----------+
| ETag 驗證           | cache entry 存 S3 ETag              | 每次讀   |
|                     | 讀取時先 HEAD 比對 ETag              | 多一個   |
|                     | 不一致就 invalidate                  | HEAD     |
+---------------------+-------------------------------------+----------+
| Last-Modified 比對  | 類似 ETag，用檔案修改時間判斷         | 同上     |
+---------------------+-------------------------------------+----------+
```

🔸 建議的 Sail 實作策略

```rust
#[derive(Clone, Hash, Eq, PartialEq)]
struct PartKey {
    path: String,
    part_id: u64,
}

struct PartValue {
    data: Bytes,
    etag: Option<String>,        // S3 回傳的 ETag
    last_modified: Option<i64>,  // S3 回傳的 Last-Modified
}
```

三級防護：

```
Level 1: Immutable File（Delta / Iceberg）
   -> 檔案永遠不變，cache 永遠有效
   -> 不需要任何額外處理

Level 2: TTL（所有場景）
   -> Foyer 設定 TTL（例如 1 小時）
   -> 過期自動重新讀取
   -> 保底機制，防止極端情況

Level 3: ETag 驗證（選配，raw S3 場景）
   -> GET 前先 HEAD 檢查 ETag
   -> 如果跟 cache 裡的一致，直接用 cache
   -> 不一致就 invalidate 重讀
   -> 多一個 HEAD 但比完整 GET 便宜很多
```

實際查詢流程（帶 ETag 驗證）：

```
CachingObjectStore::get_opts(path, range)
    |
    v
查 Foyer cache for (path, part_id)
    |
    +--> MISS -> fetch from S3 -> save (data + etag) to cache -> return
    |
    +--> HIT -> 檢查 TTL
              |
              +--> TTL 過期 -> HEAD request to S3
              |                |
              |                +--> ETag 一致 -> 更新 TTL -> return cached data
              |                |
              |                +--> ETag 不一致 -> invalidate -> re-fetch from S3
              |
              +--> TTL 未過期 -> return cached data (零 S3 請求)
```

🔸 為什麼不需要跨 Worker cache 同步

```
+--------------------------------------------------------------------+
| 方案                     | 複雜度 | 效益                           |
+--------------------------------------------------------------------+
| 每個 Worker 獨立 cache   | 低     | 簡單、無單點故障、已被          |
| (推薦)                   |        | Trino+Alluxio 驗證             |
+--------------------------------------------------------------------+
| 共享 cache server        | 高     | cache 利用率更高，但多了       |
| (如 Redis / LiquidCache) |        | 一個 network hop + 維運成本    |
+--------------------------------------------------------------------+
| P2P cache                | 極高   | 理論最優，但實作極複雜          |
| (Worker 互相借 cache)    |        | Alluxio standalone 就是這種    |
+--------------------------------------------------------------------+
```

對 Sail 來說，獨立 cache 是最佳選擇：
- Data Lake 檔案 immutable，不需要同步
- 每個 Worker 的 local NVMe 延遲 < 0.1ms，比 network cache 快 100 倍
- Trino + Alluxio embedded 已經在生產環境驗證了這個模式
- 如果同一個檔案被多個 Worker 讀，各自 cache 一份，空間浪費可接受
  （NVMe SSD 容量大且便宜，比 S3 GET 成本低很多）


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


## Trino + Alluxio：Parquet 快取架構

Trino 透過 Alluxio 在 compute 和 remote storage 之間加了一層快取。
有兩種部署模式：

🔸 部署模式一覽

```
模式 A：獨立 Alluxio 叢集（經典）

  Trino Worker  -->  Alluxio Worker  -->  Alluxio Master  -->  S3
  Trino Worker  -->  Alluxio Worker  ------^
  (alluxio:// scheme)

模式 B：嵌入式 Local Cache（推薦，2024-2025 主流）

  Trino Worker [ Alluxio Local Cache Library (embedded) ]
       |                   |
       |              Local NVMe SSD
       |
       +----> S3 (only on cache miss)
```

模式 B 更簡單，不需要維運另一個分散式系統。
每個 Trino worker JVM 內嵌 Alluxio client library，
用本地 NVMe SSD 當 cache storage。

🔸 快取粒度

Alluxio 在 byte-range / page 層級做快取，不理解 Parquet 格式：

```
+----------------------------------------------------------+
| Parquet File on S3                                       |
+----------+----------+----------+----------+--------------+
| Col A    | Col B    | Col C    | Col D    | Footer       |
| Chunk 0  | Chunk 0  | Chunk 0  | Chunk 0  | (schema,    |
| 2 MB     | 3 MB     | 1 MB     | 4 MB     |  stats)     |
+----------+----------+----------+----------+--------------+
     |          |                                  |
     v          v                                  v
+---------+---------+----  ...  ----+---------+---------+
| Page 0  | Page 1  | Page 2  ...   | Page 9  | Page 10 |
| 1 MB    | 1 MB    | 1 MB         | 1 MB    | 1 MB    |
+---------+---------+----  ...  ----+---------+---------+
  ^  cached            not cached      ^  cached
```

page size 預設 1 MB（可設定）。
Trino 的 Parquet reader 做 column pruning，只讀需要的欄位。
Alluxio 只快取被讀取的 byte range 對應的 page，
所以未被查詢的欄位自然不會進 cache。

🔸 Cache Invalidation

```
+-------------------+-----------------------------------------------+
| 機制              | 說明                                          |
+-------------------+-----------------------------------------------+
| TTL 過期          | 每個 page 有 TTL（預設 7d），過期後重新讀 S3   |
| 容量驅逐          | cache 滿了用 LRU 驅逐最冷的 page              |
| 無主動通知        | S3 沒有 push notification，靠 TTL 被動失效    |
| Data Lake 友好    | Parquet 是 write-once，Delta/Iceberg 用       |
|                   | metadata 決定讀哪些檔案，舊檔案的 cache 自然  |
|                   | 不會被用到                                    |
+-------------------+-----------------------------------------------+
```

🔸 Trino 嵌入式 Cache 設定

```properties
hive.cache.enabled=true
hive.cache.location=/mnt/nvme0/cache,/mnt/nvme1/cache
hive.cache.disk-usage-percentage=80
hive.cache.ttl=7d
hive.cache.page-size=1MB
```

🔸 獨立 Alluxio 叢集 vs 嵌入式

```
+----------------------+-----------------------------+-------------------+
| 面向                 | 獨立 Alluxio 叢集           | 嵌入式 Local Cache|
+----------------------+-----------------------------+-------------------+
| 部署複雜度           | 高（Master + Workers）       | 低（只改 config）  |
| Worker 間共享 cache  | 可以（Worker A 讀 B 的 cache）| 不行               |
| Metadata 管理        | Alluxio Master 追蹤          | 各 worker 獨立     |
| 故障影響             | Alluxio 掛了影響 Trino       | miss 直接走 S3     |
| 適用場景             | 多引擎共享、超大規模          | 單 Trino 叢集      |
+----------------------+-----------------------------+-------------------+
```

🔸 跟 Sail #1015 的關係

Trino + Alluxio 的嵌入式模式非常接近 Sail #1015 要做的事：
在 ObjectStore 層包一個 local cache，format-agnostic，page-based。
差別是 Alluxio 用 Java，Sail 打算用 Foyer（Rust）。

Alluxio 的 page-based 設計（固定 1MB page）是一個值得參考的決策：
- 簡單：不需要理解 Parquet 格式
- 高效：Parquet 的 columnar layout 讓 column pruning 自然只 cache 需要的 page
- 通用：CSV、JSON、ORC 都適用


## RisingWave：Foyer 在串流引擎的應用

RisingWave 是基於 S3 的即時串流處理平台。
它的儲存引擎 Hummock 是分散式 LSM-Tree，
用 Foyer 做 SSTable block 的 hybrid cache。

🔸 三層儲存架構

```
+------------------+
|    Memory Cache  |  <-- Foyer memory layer
|    (hottest data)|      eviction: LRU/LFU/w-TinyLFU/S3-FIFO/SIEVE
+--------+---------+
         | miss / evict
         v
+------------------+
|    Disk Cache    |  <-- Foyer disk layer (NVMe SSD)
|    (warm data)   |      engines: block / set-associative / object
|    10-1000x      |      I/O: sync or io_uring
|    larger than   |
|    memory        |
+--------+---------+
         | miss
         v
+------------------+
|    S3            |  <-- Durable storage
|    (cold data)   |      only accessed on cache miss
+------------------+
```

🔸 快取什麼

RisingWave 快取的最小單位是 SSTable 的 block（不是整個 SSTable）：

```
+---------------------------------------------+
| SSTable (on S3)                              |
+-------+-------+-------+-------+-------------+
| Block | Block | Block | Block | Meta/Index   |
|  0    |  1    |  2    |  3    | (bloom       |
| 64KB  | 64KB  | 64KB  | 64KB  |  filter)    |
+-------+-------+-------+-------+-------------+
   ^       ^                          ^
   |       |                          |
   cached  cached                     cached
   (memory)(disk)                     (memory)
```

🔸 Compaction-Aware Cache Refill

LSM-Tree compaction 會產生新的 SSTable，舊的 SSTable 被刪除。
如果不做處理，compaction 後 cache 全部失效，大量 miss 湧向 S3。

RisingWave 的解法：
1. Compaction 完成後，通知 compute node 更新 manifest
2. 更新 manifest 之前，先把新 SSTable 的 block 批次寫入 disk cache
3. 刻意寫入 disk cache 而不是 memory cache，避免汙染 hot data
4. 批次讀取可以一次 S3 GET 拿到數十到數千個連續 block，降低成本

```
                Compaction 完成
                     |
                     v
         +------------------------+
         | Batch fetch new SST    |
         | blocks from S3         |
         | (one large GET request)|
         +------------------------+
                     |
                     v
         +------------------------+
         | Write to Disk Cache    |  <-- 不進 memory cache
         | (Foyer disk layer)     |      避免 evict hot data
         +------------------------+
                     |
                     v
         +------------------------+
         | Update manifest        |  <-- 現在 read 可以找到新 block
         | (switch to new SSTs)   |
         +------------------------+
```

🔸 為什麼用 Foyer 不用 Moka

Moka 是純 memory cache，容量受限於 RAM。
串流 state 可以到 TB 等級，memory 不夠裝。
Foyer 的 disk layer 讓 cache 容量擴大 10-1000 倍，
大幅減少 S3 請求，同時保持 sub-100ms 延遲。

🔸 跟 Sail #1015 的關係

RisingWave 證明了 Foyer 在生產環境的可行性。
它的使用模式（block-level cache、compaction-aware refill）
比 Sail 需要的更複雜（Sail 只需 ObjectStore 層 cache）。

關鍵學習：
- Foyer 的 memory + disk 兩層設計是成熟的
- disk cache 很重要：memory 不夠大時 disk 能接住大量 warm data
- cache refill 概念可以用在 Sail：當 Delta/Iceberg compaction 產生新檔案時，
  主動 prefetch 到 local cache


## SlateDB：Foyer 在雲端 LSM 引擎的應用

SlateDB 是雲端原生的嵌入式 key-value 引擎，
把 LSM-Tree 的 SSTable 直接寫到 Object Storage（S3/GCS/ABS）。

🔸 架構

```
+-----------------------------------------+
| SlateDB                                 |
|                                         |
|  +-----------+     +------------------+ |
|  | MemTable  |     | db_cache module  | |
|  | (writes)  |     | (reads)          | |
|  +-----+-----+     +--------+---------+ |
|        |                    |            |
|        v                    v            |
|  +------------------------------------------+
|  | CachedObjectStore                        |
|  | (wraps ObjectStore with local disk cache)|
|  +------------------------------------------+
|        |
|        v
|  +------------------------------------------+
|  | ObjectStore (S3 / GCS / ABS / MinIO)     |
|  +------------------------------------------+
+-----------------------------------------+
```

🔸 兩層 Cache

SlateDB 有兩種獨立的 cache：

```
+-----------------------+----------------------------------+------------------+
| Cache                 | 快取什麼                         | 實作              |
+-----------------------+----------------------------------+------------------+
| db_cache (in-memory)  | SST blocks, index, bloom filters | Foyer 或 Moka    |
|                       | (解壓後的 in-memory 結構)        | (feature flag)   |
+-----------------------+----------------------------------+------------------+
| CachedObjectStore     | raw bytes from object store      | 自己寫的          |
| (local disk)          | (part-based, 未解壓)             | FsCacheStorage   |
+-----------------------+----------------------------------+------------------+
```

db_cache 是 in-memory cache，用 Foyer 或 Moka（預設 Foyer，64 MB）。
CachedObjectStore 是 local disk cache，用自己的 FsCacheStorage。

🔸 db_cache 的 Foyer 用法

透過 `DbCache` trait 抽象，Foyer 和 Moka 是可插拔的：

```rust
// db_cache module
pub trait DbCache {
    fn get(&self, key: &CachedKey) -> Option<CachedEntry>;
    fn insert(&self, key: CachedKey, entry: CachedEntry);
}

// CachedKey 可以定位到特定 SST 的特定 block
pub struct CachedKey {
    sst_id: SstId,
    block_type: BlockType,  // Data / Index / BloomFilter
    block_id: u64,
}

pub struct CachedEntry {
    // 解壓後的 block 資料
}
```

用 feature flag 切換：
- `foyer` feature（預設開啟）→ 用 Foyer 的 Cache（memory-only，不用 HybridCache）
- `moka` feature → 用 Moka

🔸 CachedObjectStore 的設計

這是 SlateDB 最值得 Sail 參考的部分。
它直接實作了 `ObjectStore` trait，在上面包一層 local disk cache：

```rust
pub(crate) struct CachedObjectStore {
    object_store: Arc<dyn ObjectStore>,
    part_size_bytes: usize,          // 固定大小的 part（類似 Alluxio 的 page）
    cache_storage: Arc<dyn LocalCacheStorage>,  // FsCacheStorage
    admission_picker: AdmissionPicker,
    cache_puts: bool,
    stats: Arc<CachedObjectStoreStats>,
}

impl ObjectStore for CachedObjectStore {
    async fn get_opts(&self, location: &Path, options: GetOptions) -> Result<GetResult> {
        self.cached_get_opts(location, options).await
    }
    async fn put_opts(&self, location: &Path, payload: PutPayload, opts: PutOptions) -> Result<PutResult> {
        self.cached_put_opts(location, payload, opts).await
    }
    // delete, list, copy, rename -> 直接 delegate，不 cache
}
```

GET 的邏輯：
1. 把請求的 byte range 切成固定大小的 part
2. 每個 part 獨立查 cache
3. cache miss 的 part 從 S3 讀取並回填
4. 組合所有 part 回傳 stream

```
GET bytes 0-5MB from file.parquet
       |
       v
Split into parts (part_size = 1MB):
  Part 0: 0-1MB    -> cache HIT  (local disk)
  Part 1: 1-2MB    -> cache HIT  (local disk)
  Part 2: 2-3MB    -> cache MISS -> fetch from S3 -> save to disk
  Part 3: 3-4MB    -> cache HIT  (local disk)
  Part 4: 4-5MB    -> cache MISS -> fetch from S3 -> save to disk
       |
       v
Combine all parts into result stream
```

PUT 的邏輯：
- 先寫到 S3
- 如果 `cache_puts` 開啟且 AdmissionPicker 允許，也寫到 local cache
- 好處：剛寫入的 SST 馬上可以從 local cache 讀

🔸 跟 Sail #1015 的關係

SlateDB 的 CachedObjectStore 就是 Sail #1015 要做的事。
差別是 SlateDB 用自己寫的 FsCacheStorage，Sail 打算用 Foyer。

可以直接參考的設計：
- part-based 切分（固定大小 part，類似 Alluxio 的 page）
- AdmissionPicker（控制哪些資料要寫入 cache）
- cache_puts（PUT 操作也回填 cache）
- 分離 in-memory cache（db_cache / Foyer）和 disk cache（CachedObjectStore）


## 總結對比

```
+-------------------+---------------+-----------+--------------------+-----------+
| 系統              | 快取層級      | 快取粒度  | 快取引擎           | 格式感知  |
+-------------------+---------------+-----------+--------------------+-----------+
| Trino + Alluxio   | ObjectStore   | 1 MB page | Alluxio (Java)     | 否        |
|                   | (byte range)  |           | LRU + TTL          |           |
+-------------------+---------------+-----------+--------------------+-----------+
| RisingWave        | Storage Engine| SST block | Foyer (Rust)       | 是        |
|                   | (LSM block)   | ~64 KB    | memory + disk      | (SST      |
|                   |               |           | LRU/LFU/S3-FIFO   |  block)   |
+-------------------+---------------+-----------+--------------------+-----------+
| SlateDB           | ObjectStore   | fixed     | Foyer (memory)     | 否        |
|                   | + in-memory   | part size | + FsCacheStorage   | (object   |
|                   |               |           | (disk)             |  store)   |
+-------------------+---------------+-----------+--------------------+-----------+
| LiquidCache       | Separate      | RecordBat | 自研 transcoded    | 是        |
|                   | cache server  | ch/column | format             | (pushdown |
|                   |               |           | memory + SSD       |  aware)   |
+-------------------+---------------+-----------+--------------------+-----------+
| Sail #1015 (建議) | ObjectStore   | fixed     | Foyer (Rust)       | 否        |
|                   | (byte range)  | part size | memory + disk      | (format   |
|                   |               | ~1 MB     | LRU                |  agnostic)|
+-------------------+---------------+-----------+--------------------+-----------+
```

Sail #1015 的建議設計最接近 SlateDB 的 CachedObjectStore + Trino/Alluxio 的 page-based 模式：
- ObjectStore trait 層包裝，format-agnostic
- 固定大小 part/page（建議 1 MB）
- Foyer HybridCache（memory + disk）取代 SlateDB 自寫的 FsCacheStorage
- AdmissionPicker 控制寫入策略
- TTL 或 ETag 做 invalidation


