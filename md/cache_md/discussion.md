## Issue #1015 社群回饋與分析

2026 年 4 月，社群成員 wudihero2 在 Issue #1015 留言，
分享了他在 Trino + Iceberg 場景遇到的 S3 請求成本問題。
以下整理他的回饋和 maintainer shehabgamin 的回覆，
加上我的分析與建議。

🔸 wudihero2 的痛點：S3 請求成本太高

他指出用 Trino 查 Iceberg 表時，S3 GET 請求的費用驚人。
Trino 的嵌入式 Alluxio 可以大幅降低這些成本（透過 worker 本地快取）。

他的問題：
- Sail 能不能在每個 Worker 做 local file caching（用 Foyer）？
- 持久化儲存（PVC）怎麼支援？
- cache key/value 要怎麼設計？是否圍繞 `ObjectStore::get_range(path, range)`？
- 有沒有考慮過 Alluxio、LiquidCache 或分散式 cache 服務？

他建議先從簡單的 per-worker local caching 開始。

🔸 shehabgamin 的方向

maintainer 回覆了幾個重點：

1. 在 sail-object-store crate 加 caching layer
2. 參考 OCRA 專案的做法，把 Moka 換成 Foyer
3. PVC 透過 worker_pod_template 設定（K8s 已支援）
4. cache key 圍繞 path + range
5. 還需要釐清：TTL 支援、range merge/overlap 怎麼處理
6. Worker auto-scaling 會降低 cache 效益（Worker 被回收 cache 就沒了）
7. Cache-aware scheduling 是進階挑戰

🔸 關於 OCRA 專案

shehabgamin 提到參考 OCRA 專案。
OCRA 全名是 "Object-store Cache in Rust for All"（github.com/lancedb/ocra），
由 LanceDB 團隊開發，是一個基於 arrow-rs ObjectStore trait 的 read-through cache library。
Apache-2.0 授權，已發布到 crates.io（版本 0.1.1）。

OCRA 的核心架構只有 5 個源碼檔案：

```
src/
├── lib.rs              公開 ReadThroughCache + PageCache trait
├── read_through.rs     ReadThroughCache<C: PageCache> 實作 ObjectStore trait
├── paging.rs           PageCache trait 定義
├── memory.rs           InMemoryCache 實作（基於 Moka）
├── memory/builder.rs   InMemoryCacheBuilder
└── stats.rs            CacheStats trait + AtomicIntCacheStats
```

核心設計是兩個元件：

1. PageCache trait（paging.rs）— 定義 page-based cache 的介面
2. ReadThroughCache（read_through.rs）— 包裝 ObjectStore，加入 cache 層

🔸 OCRA 的 PageCache trait

這是 OCRA 的 cache 抽象層，定義了固定大小 page 的 cache 操作：

```rust
// ocra/src/paging.rs
#[async_trait]
pub trait PageCache: Sync + Send + Debug + 'static {
    fn page_size(&self) -> usize;      // 每個 page 的大小
    fn capacity(&self) -> usize;       // cache 容量（page 數量）
    fn size(&self) -> usize;           // 已使用的 bytes

    // 讀取 page，cache miss 時用 loader 從 S3 拉
    async fn get_with(
        &self,
        location: &Path,
        page_id: u32,
        loader: impl Future<Output = Result<Bytes>> + Send,
    ) -> Result<Bytes>;

    // 讀取 page 的某個 range
    async fn get_range_with(
        &self,
        location: &Path,
        page_id: u32,
        range: Range<usize>,
        loader: impl Future<Output = Result<Bytes>> + Send,
    ) -> Result<Bytes>;

    // 快取 metadata（HEAD 請求）
    async fn head(
        &self,
        location: &Path,
        loader: impl Future<Output = Result<ObjectMeta>> + Send,
    ) -> Result<ObjectMeta>;

    // 寫入 + 失效
    async fn put(&self, location: &Path, page_id: u32, data: Bytes) -> Result<()>;
    async fn invalidate(&self, location: &Path) -> Result<()>;
}
```

關鍵設計：
- key 是 (location, page_id)，page_id = offset / page_size
- get_with 和 get_range_with 接受一個 loader Future 參數
- cache miss 時自動呼叫 loader 從 inner store 拉資料並回填
- 這就是 "read-through" 的語義：讀的時候自動填充

🔸 OCRA 的 InMemoryCache 實作

目前 OCRA 只有 memory cache（基於 Moka），沒有 disk cache：

```rust
// ocra/src/memory.rs
pub const DEFAULT_PAGE_SIZE: usize = 16 * 1024;          // 16 KB
const DEFAULT_TIME_TO_IDLE: Duration = Duration::from_secs(60 * 30);  // 30 分鐘
const DEFAULT_METADATA_CACHE_SIZE: usize = 32 * 1024 * 1024;         // 32 MB

pub struct InMemoryCache {
    capacity: usize,
    page_size: usize,

    // page cache: key = (location_id, page_id)
    cache: Cache<(u64, u32), Bytes>,          // Moka async Cache

    // metadata cache: key = location_id
    metadata_cache: Cache<u64, ObjectMeta>,

    // Path -> u64 location_id 的對照表
    location_lookup: RwLock<HashMap<Path, u64>>,
    next_location_id: AtomicU64,
}
```

注意 OCRA 的 page_size 預設是 16 KB，
這跟 Alluxio（1 MB）和 SlateDB（也是 part-based）差很多。
16 KB 適合 LanceDB 的向量查詢場景（一次讀小量資料），
但對 Sail 的大量 Parquet scan 來說太小了（會產生太多 key）。

🔸 OCRA 的 ReadThroughCache：核心 get_range 流程

```rust
// ocra/src/read_through.rs
pub struct ReadThroughCache<C: PageCache> {
    inner: Arc<dyn ObjectStore>,
    cache: Arc<C>,
    parallelism: usize,       // 預設 = CPU 核心數
    stats: Arc<dyn CacheStats>,
}
```

get_range 的完整流程：

```rust
async fn get_range<C: PageCache>(
    store: Arc<dyn ObjectStore>,
    cache: Arc<C>,
    stats: Arc<dyn CacheStats>,
    location: &Path,
    range: Range<usize>,
    parallelism: usize,
) -> Result<Bytes> {
    let page_size = cache.page_size();
    // 1. 把 range 對齊到 page 邊界
    let start = (range.start / page_size) * page_size;

    // 2. 先拿 metadata（知道檔案多大，避免讀超過檔案尾）
    let meta = cache.head(location, store.head(location)).await?;

    // 3. 每個 page 獨立查 cache，miss 就從 S3 拉
    let pages = stream::iter((start..range.end).step_by(page_size))
        .map(|offset| {
            let page_id = offset / page_size;
            // 計算這個 page 跟請求 range 的交集
            let intersection =
                max(offset, range.start)..min(offset + page_size, range.end);
            let range_in_page =
                intersection.start - offset..intersection.end - offset;
            // 這個 page 在檔案裡的實際結尾（防止讀超過檔案尾）
            let page_end = min(offset + page_size, meta.size);

            stats.inc_total_reads();

            async move {
                page_cache.get_range_with(
                    location,
                    page_id as u32,
                    range_in_page,
                    async {
                        stats.inc_total_misses();
                        // cache miss: 從 S3 讀整個 page
                        store.get_range(location, offset..page_end).await
                    },
                ).await
            }
        })
        .buffered(parallelism)  // 並行讀取多個 page
        .try_collect::<Vec<_>>()
        .await?;

    // 4. 把多個 page 的 bytes 拼接回來
    if pages.len() == 1 {
        return Ok(pages.into_iter().next().unwrap());
    }
    let mut buf = BytesMut::with_capacity(range.len());
    for page in pages {
        buf.extend_from_slice(&page);
    }
    Ok(buf.into())
}
```

用一個例子走過流程：

```
假設 page_size = 16KB，請求 get_range(orders.parquet, 30KB..50KB)

Step 1: 對齊到 page 邊界
  start = (30KB / 16KB) * 16KB = 16KB

Step 2: 切成 pages
  page_id=1  offset=16KB  range_in_page=14KB..16KB  (只要 30KB-32KB 的部分)
  page_id=2  offset=32KB  range_in_page=0..16KB     (整個 page)
  page_id=3  offset=48KB  range_in_page=0..2KB      (只要 48KB-50KB 的部分)

Step 3: 每個 page 獨立查 cache
  page 1: cache.get_range_with(path, 1, 14KB..16KB, loader)
    -> HIT: 回傳 cache 裡的 bytes[14KB..16KB]
    -> MISS: 從 S3 讀 16KB..32KB，存到 cache，回傳 bytes[14KB..16KB]

  page 2: cache.get_range_with(path, 2, 0..16KB, loader)
    -> 整個 page，類似

  page 3: cache.get_range_with(path, 3, 0..2KB, loader)
    -> 只需要前 2KB

Step 4: 拼接
  bytes = page1_slice + page2_slice + page3_slice
  = 2KB + 16KB + 2KB = 20KB (= 50KB - 30KB)
```

🔸 OCRA 的 ObjectStore trait 實作

```rust
#[async_trait]
impl<C: PageCache> ObjectStore for ReadThroughCache<C> {
    // 讀取：走 cache
    async fn get_range(&self, location: &Path, range: Range<usize>) -> Result<Bytes> {
        get_range(self.inner.clone(), self.cache.clone(), ...).await
    }

    // 完整讀取：逐 page 串流
    async fn get(&self, location: &Path) -> Result<GetResult> {
        let meta = self.head(location).await?;
        // 把整個檔案切成 page stream
        let s = stream::iter((0..meta.size).step_by(page_size))
            .map(|offset| get_range(store, cache, ..., offset..offset+page_size, ...))
            .buffered(self.parallelism)
            .boxed();
        Ok(GetResult { payload: GetResultPayload::Stream(s), ... })
    }

    // metadata：也走 cache
    async fn head(&self, location: &Path) -> Result<ObjectMeta> {
        self.cache.head(location, self.inner.head(location)).await
    }

    // 寫入操作：先 invalidate cache，再 delegate
    async fn put_opts(&self, location: &Path, ...) -> Result<PutResult> {
        self.cache.invalidate(location).await?;
        self.inner.put_opts(location, payload, options).await
    }

    async fn delete(&self, location: &Path) -> Result<()> {
        self.cache.invalidate(location).await?;
        self.inner.delete(location).await
    }

    async fn copy(&self, from: &Path, to: &Path) -> Result<()> {
        self.cache.invalidate(to).await?;   // 只 invalidate 目標
        self.inner.copy(from, to).await
    }

    // get_opts 尚未實作
    async fn get_opts(&self, ...) -> Result<GetResult> {
        todo!()
    }

    // list, list_with_delimiter: 直接 delegate，不 cache
    fn list(&self, prefix: Option<&Path>) -> BoxStream<Result<ObjectMeta>> {
        self.inner.list(prefix)
    }
}
```

🔸 OCRA 的 invalidation 策略

```rust
// ocra/src/memory.rs
async fn invalidate(&self, location: &Path) -> Result<()> {
    // 只從 lookup table 移除 path -> location_id 的對應
    // 不逐一刪除 cache 裡的 page entries
    let mut id_map = self.location_lookup.write().await;
    id_map.remove(location);
    Ok(())
}
```

這是一個聰明的 O(1) invalidation：
不是去刪 cache 裡的每一個 (location_id, page_id) entry（O(n)），
而是把 path -> location_id 的映射刪掉。
下次同一個 path 進來會拿到新的 location_id，
舊的 entries 自然變成孤兒，靠 Moka 的 TTL / LRU 自動驅逐。

缺點是 invalidation 後短暫期間舊資料還在 cache 裡佔空間，
但對 Data Lake 場景（檔案 immutable）不會有問題。

🔸 OCRA 的使用方式

```rust
use ocra::{ReadThroughCache, memory::InMemoryCache};

// 建立 memory cache（用系統 75% 記憶體）
let memory_cache = Arc::new(
    InMemoryCache::with_sys_memory(0.75).build()
);

// 包裝 S3 ObjectStore
let s3_store: Arc<dyn ObjectStore> = Arc::new(AmazonS3Builder::new()...build()?);
let cached_store: Arc<dyn ObjectStore> =
    Arc::new(ReadThroughCache::new(s3_store, memory_cache));

// 對 DataFusion 完全透明
let data = cached_store.get_range(&path, 1024..2048).await?;
```

🔸 OCRA vs Sail 需求的差距

```
+---------------------------+---------------------------+-------------------------+
| 面向                      | OCRA 現狀                 | Sail #1015 需求         |
+---------------------------+---------------------------+-------------------------+
| Cache 引擎                | Moka（純 memory）         | Foyer（memory + disk）  |
+---------------------------+---------------------------+-------------------------+
| 預設 page size            | 16 KB                     | 建議 1 MB               |
+---------------------------+---------------------------+-------------------------+
| Disk cache                | 沒有                      | 需要（NVMe SSD）        |
+---------------------------+---------------------------+-------------------------+
| TTL                       | time-to-idle 30 分鐘      | 需要 time-to-live       |
|                           | （idle 後驅逐）           | （絕對過期時間）        |
+---------------------------+---------------------------+-------------------------+
| ETag 驗證                 | 沒有                      | 選配（raw S3 場景）     |
+---------------------------+---------------------------+-------------------------+
| get_opts 支援             | todo!()                   | 必須實作                |
+---------------------------+---------------------------+-------------------------+
| Admission 策略            | 沒有（全部 cache）        | 需要 AdmissionPicker    |
+---------------------------+---------------------------+-------------------------+
| Metrics                   | 簡單的 AtomicU64 計數     | OpenTelemetry 整合      |
+---------------------------+---------------------------+-------------------------+
| 生產驗證                  | LanceDB 內部使用          | 需要在 K8s cluster 驗證 |
+---------------------------+---------------------------+-------------------------+
```

OCRA 的 page_size 預設 16 KB 是為 LanceDB 的向量查詢優化的
（Lance 格式的一次讀取通常是幾十 KB 的向量片段）。

Sail / DataFusion 讀 Parquet 時，每次 get_range 讀的是一整個 column chunk。
column chunk 的大小取決於 row group 行數、欄位型態、和壓縮率。
以下是相關的預設值（來自 Sail 的 application.yaml 和 arrow-rs 的 properties.rs）：

```
+-----------------------------------+-----------------------+-------------------+
| 設定                              | 預設值                | 來源              |
+-----------------------------------+-----------------------+-------------------+
| parquet.data_page_size_limit      | 1,048,576 (1 MB)      | application.yaml  |
|                                   |                       | :418              |
+-----------------------------------+-----------------------+-------------------+
| parquet.max_row_group_size        | 1,048,576 rows        | application.yaml  |
|                                   | (不是 bytes)          | :481              |
+-----------------------------------+-----------------------+-------------------+
| arrow-rs DEFAULT_PAGE_SIZE        | 1,048,576 (1 MB)      | parquet/src/file/ |
|                                   |                       | properties.rs     |
+-----------------------------------+-----------------------+-------------------+
| Parquet 格式規格建議 row group    | 512 MB - 1 GB         | parquet.apache    |
|                                   |                       | .org              |
+-----------------------------------+-----------------------+-------------------+
| Parquet 格式規格建議 data page    | 8 KB                  | parquet.apache    |
| (但 arrow-rs/DataFusion 用 1 MB)  |                       | .org              |
+-----------------------------------+-----------------------+-------------------+
```

一個 column chunk = 該欄位在一個 row group 內的所有 data pages。
每個 data page 上限 1 MB，一個 column chunk 通常有 1 到數十個 pages，
所以實際 column chunk 大小從幾百 KB 到數十 MB 不等，
視欄位型態（int32 vs string vs nested struct）和壓縮率（zstd 預設 level 3）而定。

用 16 KB page 來 cache 這些 MB 級的讀取會切出太多 key，metadata 開銷太大。
建議 1 MB page 是因為剛好跟 data_page_size_limit 對齊。

🔸 Sail 可以從 OCRA 借鑑什麼

1. PageCache trait 的抽象設計
OCRA 把 cache 抽成 PageCache trait，InMemoryCache 是一個實作。
Sail 可以用同樣的 trait，但改成 Foyer HybridCache 作為實作。
這讓未來換 cache 引擎（或加第二層 disk cache）變得容易。

2. ReadThroughCache 的 ObjectStore 包裝模式
get_range 切 page -> 查 cache -> miss 時 load -> 回填 -> 拼接。
這個流程幾乎可以直接搬到 Sail 的 CachingObjectStore。

3. get_range_with 的 loader pattern
用 Future 參數讓 cache 層控制「何時讀 S3」，
避免 cache hit 時不必要的 S3 請求。

4. O(1) invalidation 技巧
刪 path->id 映射而不是逐一刪 page，
這個技巧在 Data Lake 場景特別適用。

5. buffered parallelism
用 `stream::iter().buffered(parallelism)` 並行讀多個 page，
parallelism 預設 = CPU 核心數。

不能直接照搬的部分：
- Moka 要換成 Foyer（加 disk layer）
- page_size 要從 16 KB 調到 1 MB
- 要實作 get_opts（OCRA 目前是 todo!()）
- 要加 TTL / ETag invalidation
- 要加 AdmissionPicker（控制哪些資料值得 cache）

shehabgamin 建議的做法正是這樣：
把 OCRA 的架構搬過來，但把 Moka 換成 Foyer，
同時補上 OCRA 缺少的 disk cache、TTL、admission 等功能。

🔸 關於 range overlap 問題

shehabgamin 提到的 range overlap 是一個好問題。
如果用 raw range 當 cache key，不同查詢即使讀到重疊的 bytes 也會 miss：

```
問題場景（raw range 當 key）：

第一次查詢: GET range 0-5MB   -> cache key: (path, 0..5MB)
第二次查詢: GET range 2-3MB   -> cache key: (path, 2..3MB)
                                  MISS! 因為 key 不同

兩個 range 有重疊但 key 不匹配，導致不必要的 cache miss。
```

固定大小 part 的解法：

```
第一次查詢: GET range 0-5MB
  -> 切成 part 0,1,2,3,4 各 1MB
  -> 全部 cache

第二次查詢: GET range 2-3MB
  -> 切成 part 2
  -> HIT! 因為 part 2 已經在 cache 裡
```

固定 part 大小（如 1 MB）讓所有 range 請求對齊到同一組 key，
徹底消除 overlap 問題。這也是 Alluxio 和 SlateDB 的做法。

🔸 關於 range merge 問題

shehabgamin 還問了 "How is range merged"，
這是另一個問題：多個相鄰的小 range cache miss 時，
要不要合併成一個大 S3 GET 來減少請求次數？

```
問題場景：

DataFusion 讀 Parquet 時連續發 3 個 get_range：
  (1) get_range(path, 0MB..1MB)     column A
  (2) get_range(path, 1MB..2MB)     column B
  (3) get_range(path, 2MB..3MB)     column C

如果三個都 cache miss，走 S3：
  不合併: 3 個 S3 GET 請求（S3 按請求次數計費）
  合併:   1 個 S3 GET(0..3MB)，再切回 3 份存 cache
```

固定 part 的設計讓這個問題變得可控。
因為 cache miss 的單位是 part（1 MB），
可以在 CachingObjectStore 層做 miss batch：

```
CachingObjectStore 收到 get_range(path, 0..5MB)
    |
    v
切成 5 個 part，逐一查 cache：
  part 0: HIT
  part 1: MISS     \
  part 2: MISS      | 三個連續 miss
  part 3: MISS     /
  part 4: HIT
    |
    v
合併連續 miss 的 part：
  不是發 3 個 S3 GET(1MB..2MB, 2MB..3MB, 3MB..4MB)
  而是發 1 個 S3 GET(1MB..4MB)
  拿到 3MB bytes 後切成 3 份，各自存進 cache
    |
    v
最後拼接 5 個 part 回傳
```

這樣做的好處：

```
+-----------------------------+------------+-----------+
|                             | 不合併     | 合併      |
+-----------------------------+------------+-----------+
| S3 GET 請求數               | 3          | 1         |
+-----------------------------+------------+-----------+
| S3 請求成本                 | 3x         | 1x        |
| ($0.0004 per 1000 GET)     |            |           |
+-----------------------------+------------+-----------+
| 網路 round trip             | 3 次       | 1 次      |
+-----------------------------+------------+-----------+
| 傳輸的 bytes 總量           | 相同       | 相同      |
+-----------------------------+------------+-----------+
```

要不要合併取決於 miss 的 part 是否連續。
如果 miss 的 part 中間夾著 HIT，就不該合併（會重複讀已有的 bytes）：

```
part 0: MISS  \
part 1: HIT    |  不連續，不合併
part 2: MISS  /

→ 發 2 個獨立的 S3 GET：
  GET(0..1MB) 給 part 0
  GET(2..3MB) 給 part 2
```

也可以設一個 gap tolerance：如果中間只隔 1-2 個 HIT 的 part，
寧可多讀那 1-2 MB 也要合併，因為減少一個 S3 請求比多讀 1 MB 划算。

```
gap_tolerance = 2 MB 時：

part 0: MISS
part 1: HIT     ← 中間只隔 1 MB，在 tolerance 內
part 2: MISS

→ 合併成 1 個 S3 GET(0..3MB)
  part 1 的 bytes 讀到了但 cache 裡已有，丟掉就好
  省了 1 個 S3 請求
```

OCRA 目前沒有做 merge（每個 page miss 獨立發 S3 GET），
但用 `.buffered(parallelism)` 並行讀取多個 page 來降低延遲。
Sail 可以在 CachingObjectStore 裡加一層 merge 邏輯作為優化。

🔸 關於 TTL

Foyer 原生支援 TTL（Time-To-Live），在建立 cache 時設定即可：

```rust
let cache = HybridCacheBuilder::new()
    .memory(64 * 1024 * 1024)
    .storage()
    .with_engine_config(BlockEngineConfig::new(device))
    .build()
    .await?;

// insert with TTL
cache.insert_with_ttl(key, value, Duration::from_secs(3600));
```

建議策略：
- Data Lake 檔案（Delta/Iceberg）：不需要 TTL，檔案 immutable
- Raw S3 檔案：設定 TTL（如 1 小時），保底防止讀到過期資料
- 可以在 config 裡讓使用者自行調整

🔸 關於 Worker auto-scaling 影響 cache 效益

shehabgamin 提到一個很實際的問題：
K8s Worker Pod 被 auto-scaling 回收時，cache 也跟著消失。

```
時間線：
T1: Worker Pod A 啟動，慢慢 warm up cache
T2: 處理查詢，cache 開始有效
T3: 查詢結束，Worker idle 60s
T4: Worker Pod A 被回收，cache 消失
T5: 新查詢來，啟動 Worker Pod B
T6: Pod B 是 cold cache，又要重新 warm up
```

這是 per-worker cache 的根本限制。三個緩解策略：

策略 1：PVC 持久化（推薦）
Pod 回收但 PV 還在。新 Pod 掛載同一個 PV，disk cache 立即可用。
但需要注意 PV 的 accessMode 和 Pod scheduling 的配合：
- ReadWriteOnce PV 只能被一個 Pod 掛載
- 需要 nodeAffinity 讓新 Pod 排到同一個 node

策略 2：延長 Worker idle timeout
把 `worker_max_idle_time` 從 60s 調高（如 10 分鐘或 1 小時），
減少 Worker 被頻繁回收的機率。
代價是占用更多 K8s 資源。

策略 3：Cache prefetch / warm-up
新 Worker 啟動時，Driver 告訴它「接下來可能要讀哪些檔案」，
Worker 提前 prefetch 到 local cache。
這需要 Driver 有 file-level 的排程資訊，實作較複雜。

🔸 關於 Cache-Aware Scheduling

shehabgamin 提到這是進階挑戰。
概念是：Driver 排程 task 時考慮「哪個 Worker 已經 cache 了相關資料」，
把 task 優先分配給有 cache 的 Worker。

```
Driver 看到的資訊：
  Task: scan orders/part-00001.parquet
  Worker 0: cache 裡有 part-00001 的 70% data
  Worker 1: cache 裡有 part-00001 的 0% data
  Worker 2: cache 裡沒有相關資料

  -> 優先分配給 Worker 0
```

這需要：
1. 每個 Worker 向 Driver 匯報 cache 狀態（cache manifest）
2. Driver 的 scheduler 加入 cache affinity 邏輯
3. 平衡 cache affinity 和 load balancing

參考：
- Trino 有 SoftAffinitySchedulingStrategy，基於檔案 hash 做 sticky assignment
- Alluxio 的 data locality 排程也是類似概念

Sail 目前的排程在 `crates/sail-execution/src/driver/task_assigner/core.rs`，
用 first-fit 策略分配（不是 round-robin，也不是 load-based）：

```rust
// crates/sail-execution/src/driver/task_assigner/core.rs (line 274-288)
struct TaskSlotAssigner {
    slots: Vec<(WorkerId, Vec<usize>)>,  // 每個 Worker 的空閒 slot 列表
}

impl TaskSlotAssigner {
    fn next(&mut self) -> Option<(WorkerId, usize)> {
        self.slots
            .iter_mut()
            // find_map: 找到第一個有空 slot 的 Worker 就回傳
            .find_map(|(worker_id, slots)| slots.pop().map(|slot| (*worker_id, slot)))
    }
}
```

流程：
1. `build_worker_task_slot_assigner()` 收集所有 Active Worker 的空閒 slots
2. `next()` 用 find_map 找第一個有空位的 Worker，pop 一個 slot
3. 一個 Worker 的 slots 用完才輪到下一個

這是 first-fit 策略：先把第一個 Worker 填滿，再往下。
好處是簡單高效，缺點是無法考慮 cache locality。

加入 cache-aware 排程是一個值得做但優先度較低的改進，
建議放在 Phase 1 完成之後。

🔸 簡化版 cache-aware：一致性 hash

不需要 Worker 匯報 cache 狀態，
用一致性 hash 把同一個檔案永遠分配給同一個 Worker：

```
hash(file_path) % num_workers = target_worker

orders/part-00001.parquet -> hash -> Worker 0
orders/part-00002.parquet -> hash -> Worker 1
orders/part-00003.parquet -> hash -> Worker 2
orders/part-00001.parquet -> hash -> Worker 0  (同一檔案，同一 Worker)
```

好處：
- 零通訊開銷，不需要匯報 cache 狀態
- 同一檔案永遠被同一 Worker 讀，cache 命中率最高
- Trino 的 SoftAffinitySchedulingStrategy 就是這個做法

缺點：
- Worker 數量變化時 hash 重新分配，cache 大量失效
- 可以用 consistent hashing（一致性 hash）緩解，但不能完全避免

🔸 為什麼普通 hash 有問題

普通 hash 用 `hash(file) % N` 分配。N 一變，幾乎全部重新分配：

```
3 個 Worker 時（% 3）：
  hash("part-001") % 3 = 1  → Worker 1
  hash("part-002") % 3 = 0  → Worker 0
  hash("part-003") % 3 = 2  → Worker 2
  hash("part-004") % 3 = 1  → Worker 1
  hash("part-005") % 3 = 0  → Worker 0
  hash("part-006") % 3 = 2  → Worker 2

加到 4 個 Worker（% 4）：
  hash("part-001") % 4 = 2  → Worker 2  ← 變了
  hash("part-002") % 4 = 3  → Worker 3  ← 變了
  hash("part-003") % 4 = 1  → Worker 1  ← 變了
  hash("part-004") % 4 = 0  → Worker 0  ← 變了
  hash("part-005") % 4 = 2  → Worker 2  ← 變了
  hash("part-006") % 4 = 1  → Worker 1  ← 變了

6 個檔案全部重新分配，所有 cache 失效。
```

🔸 Consistent Hashing 怎麼緩解

把 hash 值想成一個環（ring），範圍 0 ~ 399（簡化）。
Worker hash 到 ring 上的固定位置，把 ring 切成幾段。
每個檔案的 hash 值落在哪一段，就歸那段的 Worker 管。

```
3 個 Worker hash 到 ring 上：
  W0 = 100, W1 = 200, W2 = 300

ring 被切成 3 段：
  段 A: 301 ~ 100  → W0
  段 B: 101 ~ 200  → W1
  段 C: 201 ~ 300  → W2

              0/400
               |
       W2(300) |  W0(100)
          \    |    /
           \   |   /
            \  |  /
             段C  段A
              \|/
     ---------+---------
              /|\
             / | \
            /  |  \
           / 段B  \
          /    |    \
       W1(200) |
               |

分配：
  hash("part-001") = 50   → 段 A → W0
  hash("part-002") = 120  → 段 B → W1
  hash("part-003") = 250  → 段 C → W2
  hash("part-004") = 80   → 段 A → W0
  hash("part-005") = 350  → 段 A → W0
  hash("part-006") = 180  → 段 B → W1
```

現在加一個 W3，hash 到 150。
W3 插在 W0(100) 和 W1(200) 之間，只把段 B 切成兩半：

```
  段 A:  301 ~ 100  → W0（沒變）
  段 B1: 101 ~ 150  → W3（新的，從 W1 搶走的半段）
  段 B2: 151 ~ 200  → W1（縮小了，但還是 W1）
  段 C:  201 ~ 300  → W2（沒變）

重新分配：
  hash("part-001") = 50   → 段 A  → W0  ← 沒變
  hash("part-002") = 120  → 段 B1 → W3  ← 變了（120 落在被 W3 搶走的半段）
  hash("part-003") = 250  → 段 C  → W2  ← 沒變
  hash("part-004") = 80   → 段 A  → W0  ← 沒變
  hash("part-005") = 350  → 段 A  → W0  ← 沒變
  hash("part-006") = 180  → 段 B2 → W1  ← 沒變（180 還在 W1 的段裡）
```

6 個檔案只有 1 個被重新分配。
新 Worker 只影響 ring 上它左邊鄰居的那一小段，其他完全不動。

🔸 數學上的差距

```
+-------------------+---------------------------+---------------------------+
| 場景              | 普通 hash (% N)           | Consistent Hashing        |
+-------------------+---------------------------+---------------------------+
| 3 -> 4 Workers    | ~75% 檔案重新分配         | ~25% 檔案重新分配         |
| (加 1 個)          | (幾乎全部)                | (只有新 Worker 搶走的段)  |
+-------------------+---------------------------+---------------------------+
| N -> N+1 一般公式 | ~(N-1)/N 檔案受影響       | ~1/N 檔案受影響           |
|                   | (N 越大越慘)              | (N 越大影響越小)          |
+-------------------+---------------------------+---------------------------+
```

為什麼說「不能完全避免」：
即使用 consistent hashing，加減 Worker 時還是有 ~1/N 的檔案被重新分配。
這些檔案在新 Worker 上是 cold cache，要重新從 S3 讀。
只是從「幾乎全部失效」降到「一小部分失效」。

🔸 更推薦的方案：固定 Logical Shards + Rendezvous Hashing

Consistent Hashing 已經比普通 hash 好很多，
但在「Worker 本地 cache」的場景有一個更實用的做法：
把映射拆成兩層，中間加一層固定數量的 logical shard。

核心思路：不讓 key 直接對應 Worker，而是先對應到一個固定的 shard，
再把 shard 指派給 Worker。

```
一層映射（之前討論的方案）：
  cache key -----> Worker        key 直接對 Worker，Worker 變就全部重算

兩層映射（推薦）：
  cache key -----> shard -----> Worker
                   (固定)       (可變)
```

第一層：key → shard（永遠不變）

shard 數量在系統初始化時就固定（例如 2048），永遠不變：

```
shard_id = xxhash64(path, block_id) % 2048

舉例：
  xxhash64("orders/part-001.parquet", block=5)  % 2048 = 731
  xxhash64("orders/part-001.parquet", block=6)  % 2048 = 1204
  xxhash64("orders/part-002.parquet", block=0)  % 2048 = 42
  xxhash64("users/data.parquet",      block=3)  % 2048 = 731

不管 Worker 有 3 個還是 30 個，shard 731 永遠是 shard 731。
這層用 % 完全沒問題，因為 2048 這個數字永遠不變。
```

第二層：shard → Worker（用 Rendezvous Hashing）

Rendezvous Hashing（又叫 HRW，Highest Random Weight）的規則：
對每個 shard，跟每個 Worker 算一個分數，分數最高的 Worker 拿走這個 shard。

```
score(shard, worker) = hash(shard_id, worker_id)

以 shard 731 為例，3 個 Worker：

  score(731, W0) = hash(731, 0) = 8723491
  score(731, W1) = hash(731, 1) = 3219087
  score(731, W2) = hash(731, 2) = 9501234  ← 最高

  → shard 731 歸 W2 管
```

對所有 2048 個 shard 都做一遍，每個 Worker 大約分到 2048/3 ≈ 683 個 shard。

加入 W3 時發生什麼：

```
shard 731 重新算分數：
  score(731, W0) = 8723491         （跟之前一樣）
  score(731, W1) = 3219087         （跟之前一樣）
  score(731, W2) = 9501234         （跟之前一樣）
  score(731, W3) = 5812903         ← 新加的

  W2 還是最高 → shard 731 還是歸 W2，不動！

shard 42 重新算分數：
  score(42, W0) = 4102938
  score(42, W1) = 6721034          ← 原本最高
  score(42, W2) = 2918374
  score(42, W3) = 8934521          ← 新加的，而且超過 W1

  → shard 42 從 W1 搬到 W3
```

關鍵：一個 shard 只有在新 Worker 的分數恰好超過原本 winner 時才會搬。
大部分 shard 原本的 winner 不會被超過，所以不動。

```
3 → 4 Workers 時：
  2048 個 shard 裡大約只有 ~512 個（25%）搬到 W3
  其他 ~1536 個 shard 不動，這些 shard 的 cache 繼續有效
```

完整的 cache-aware 排程流程：

```
SELECT order_id, amount FROM orders WHERE region = 'US'

DataFusion 決定要讀 orders/part-001.parquet 的 offset 13MB-16MB

  Step 1: 切成 cache block
    block_id = 13MB / 1MB = 13

  Step 2: key → shard（永遠不變）
    shard_id = xxhash64("orders/part-001.parquet", 13) % 2048 = 731

  Step 3: shard → Worker（Rendezvous Hashing）
    score(731, W0) = 8723491
    score(731, W1) = 3219087
    score(731, W2) = 9501234  ← 最高
    → shard 731 歸 W2

  Step 4: Driver 把這個 scan task 分配給 W2
    W2 的 Foyer cache 裡大機率已有 block 13 → cache HIT
```

為什麼比 Consistent Hashing 更實用：

```
+----------------------------+-------------------+-------------------------+
| 需求                       | Consistent Hash   | Logical Shards + HRW    |
|                            | Ring              |                         |
+----------------------------+-------------------+-------------------------+
| 加減 Worker 只影響一小部分 | 可以              | 可以（一樣 ~1/N）       |
+----------------------------+-------------------+-------------------------+
| 看某個 shard 的 hit rate   | 困難（ring 上沒有 | 直接查 shard_id 的統計  |
|                            | 明確的 bucket）   |                         |
+----------------------------+-------------------+-------------------------+
| 手動把熱點 shard 搬到      | 要調 ring 上的    | 改 mapping table 一行   |
| 另一個 Worker              | virtual node      |                         |
+----------------------------+-------------------+-------------------------+
| shard 綁定 PVC             | 不自然            | shard → PVC 直接對應    |
+----------------------------+-------------------+-------------------------+
| 預熱特定 shard             | 要算 ring 上哪段  | 直接列出 shard 清單     |
+----------------------------+-------------------+-------------------------+
| rebalance 粒度             | 由 ring 決定      | 以 shard 為單位，可控   |
+----------------------------+-------------------+-------------------------+
```

核心差異：Consistent Hashing 的分配是隱式的（由 ring 位置決定），
Logical Shards 是顯式的（有一張明確的 shard → worker 對照表）。
顯式的好操作、好觀察、好調整。

三種方案的建議排序：

```
+----+--------------------------------+-----------+---------------------------+
| #  | 方案                           | 推薦度    | 適用場景                  |
+----+--------------------------------+-----------+---------------------------+
| 1  | 固定 Logical Shards            | 最推薦    | Worker 本地 cache         |
|    | + Rendezvous Hashing           |           | 需要可觀察、可控          |
|    |                                |           | 未來可能要 rebalance      |
+----+--------------------------------+-----------+---------------------------+
| 2  | Consistent Hash Ring           | 次推薦    | 不想管 shard              |
|    | + Virtual Nodes                |           | 只想新增 Worker 少搬資料  |
+----+--------------------------------+-----------+---------------------------+
| 3  | hash(key) % worker_count       | 不建議    | 除非 Worker 數永遠不變    |
|    |                                |           | 或不在意 cache 失效       |
+----+--------------------------------+-----------+---------------------------+
```

建議的 Sail 初版參數：

```
cache key:      (object_path, block_id)
block size:     1 MB（對齊 data_page_size_limit）
logical shards: 2048
hash function:  xxHash64（快速、分佈好、非密碼學）
shard → worker: Rendezvous Hashing
```

🔸 跟外部方案的距離

wudihero2 提到的幾個替代方案，跟 Sail 的距離：

```
+---------------------+-----------+---------------------------------------+
| 方案                | 可行性    | 說明                                  |
+---------------------+-----------+---------------------------------------+
| Foyer local cache   | 高        | Issue #1015 的主要方向                |
| (CachingObjectStore)| (Phase 1) | 最簡單，per-worker，不需要額外服務    |
+---------------------+-----------+---------------------------------------+
| Alluxio embedded    | 中        | 需要 JVM（Sail 是 Rust）              |
|                     |           | 但概念完全可以用 Foyer 重現           |
|                     |           | 架構上 Sail 的方案等同 Alluxio 嵌入式 |
+---------------------+-----------+---------------------------------------+
| LiquidCache         | 中低      | 需要額外的 cache server               |
|                     |           | 但 pushdown 能力比 local cache 強     |
|                     |           | 適合作為 Phase 2 或 Phase 3 參考      |
+---------------------+-----------+---------------------------------------+
| 分散式 cache 服務   | 低        | 增加維運複雜度                        |
| (Redis / Memcached) | (不建議)  | 額外 network hop                      |
|                     |           | 除非多個 Sail cluster 共享 cache      |
+---------------------+-----------+---------------------------------------+
```

🔸 建議的實作優先順序（整合社群回饋後）

```
Step 1: CachingObjectStore + Foyer HybridCache (memory + disk)
        - ObjectStore trait 包裝
        - 固定 1 MB part size
        - key = (path, part_id), value = raw bytes
        - TTL 可設定
        - 對 DataFusion 透明

Step 2: K8s PVC 支援
        - worker_pod_template 設定 volumeMount
        - Foyer disk layer 指向 PVC mountPath
        - Pod 重啟後 disk cache 自動恢復

Step 3: Config 介面
        - cache.object_store.enabled
        - cache.object_store.memory_size
        - cache.object_store.disk_path / disk_size
        - cache.object_store.part_size
        - cache.object_store.ttl

Step 4: Cache metrics + observability
        - Foyer 支援 OpenTelemetry / Prometheus
        - 暴露 hit rate, miss rate, eviction rate, disk usage

Step 5: Cache-aware scheduling (進階)
        - 一致性 hash 分配 file -> worker
        - 或 Worker 匯報 cache manifest + Driver affinity 排程
```
