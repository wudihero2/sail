# DataFusion PR #17022：用 Cached Metadata 加速 ListingTable Statistics

PR #17022 修復了一個效能漏洞：查詢執行時已經有 metadata cache，
但 ListingTable 在收集 statistics 時繞過了這個 cache，每次都重新從 ObjectStore 讀 footer。
修復後，同一個查詢從 1.3 秒降到 0.164 秒。


## 問題：兩條讀 metadata 的路徑

DataFusion 讀 Parquet 時，有兩個獨立的流程會讀取 file metadata（footer + page index）：

```
路徑 1：查詢執行（讀資料）
  DataSourceExec -> ParquetOpener -> CachedParquetFileReader
    -> get_metadata()
    -> 有 cache ✅（PR #16971 已處理）

路徑 2：統計資訊收集（optimizer 用）
  ListingTable -> ParquetFormat::infer_stats()
    -> fetch_statistics()
    -> fetch_parquet_metadata()
    -> 沒有 cache ❌（PR #17022 要修的）
```

路徑 2 在每次查詢規劃（query planning）時都會被呼叫，用來收集每個 Parquet 檔案的統計資訊（row count、min/max、null count）。
如果有 100 個 Parquet 檔案，每次查詢就要讀 100 次 footer。即使路徑 1 已經 cache 了 metadata，路徑 2 完全不知道。


## 解決方案：共用 FileMetadataCache

PR #17022 的核心改動：讓路徑 2 也透過 `DFParquetMetadata` 走同一個 `FileMetadataCache`。

```
修復後：
                     +---------------------+
                     |  FileMetadataCache   |  <-- 全域共用，在 CacheManager 裡
                     |  (LRU, memory limit) |
                     +-----+-------+-------+
                           |       |
           +---------------+       +---------------+
           |                                       |
路徑 1：查詢執行                          路徑 2：統計收集
CachedParquetFileReader               ParquetFormat::infer_stats()
  -> get_metadata()                     -> DFParquetMetadata::fetch_statistics()
  -> DFParquetMetadata::fetch_metadata()    -> DFParquetMetadata::fetch_metadata()
  -> check cache first ✅                   -> check cache first ✅
```

兩條路徑最終都呼叫 `DFParquetMetadata::fetch_metadata()`，
這個方法會先查 cache，命中就直接回傳，不命中才讀 ObjectStore。


## 調用鏈總覽

```
路徑 2（統計收集）的完整調用鏈：

ListingTable::collect_stat_and_ordering()
  |
  v
ParquetFormat::infer_stats()                    // file_format.rs line 432-450
  |
  v
DFParquetMetadata::fetch_statistics()           // metadata.rs line 210-213
  |
  v
DFParquetMetadata::fetch_metadata()             // metadata.rs line 116-176  <-- 核心
  |
  +-- cache hit? -> return Arc<ParquetMetaData>
  |
  +-- cache miss -> ObjectStoreFetch::fetch()    // file_format.rs line 1063-1084
                    |
                    v
                    ObjectStore::get_range()      // 真正讀 S3/GCS
                    |
                    v
                    ParquetMetaDataReader::load_and_finish()
                    |
                    v
                    put into cache -> return Arc<ParquetMetaData>
```


## 源碼解析

🔸 入口：ParquetFormat::infer_stats()

```rust
// datafusion-datasource-parquet/src/file_format.rs (line 432-450)
async fn infer_stats(
    &self,
    state: &dyn Session,
    store: &Arc<dyn ObjectStore>,
    table_schema: SchemaRef,
    object: &ObjectMeta,
) -> Result<Statistics> {
    let file_decryption_properties =
        get_file_decryption_properties(state, &self.options, &object.location)
            .await?;
    let file_metadata_cache =
        state.runtime_env().cache_manager.get_file_metadata_cache();
    DFParquetMetadata::new(store, object)
        .with_metadata_size_hint(self.metadata_size_hint())
        .with_decryption_properties(file_decryption_properties)
        .with_file_metadata_cache(Some(file_metadata_cache))
        .fetch_statistics(&table_schema)
        .await
}
```

這是 `FileFormat` trait 的實作。
每個 Parquet 檔案在查詢規劃階段會被呼叫一次，用來取得統計資訊。

`state.runtime_env().cache_manager.get_file_metadata_cache()` 取得全域的 `FileMetadataCache`。
這是 PR #17022 的關鍵改動：把 cache 傳進 `DFParquetMetadata`。

`DFParquetMetadata` 是 builder pattern，用 `with_*` 串接設定，最後呼叫 `fetch_statistics()`。

🔸 DFParquetMetadata 結構體

```rust
// datafusion-datasource-parquet/src/metadata.rs (line 63-71)
pub struct DFParquetMetadata<'a> {
    store: &'a dyn ObjectStore,
    object_meta: &'a ObjectMeta,
    metadata_size_hint: Option<usize>,
    decryption_properties: Option<Arc<FileDecryptionProperties>>,
    file_metadata_cache: Option<Arc<dyn FileMetadataCache>>,
    pub coerce_int96: Option<TimeUnit>,
}
```

`DFParquetMetadata` 是統一的 Parquet metadata 讀取入口。
它持有 `ObjectStore` 參考（用來讀檔案）和 `FileMetadataCache` 參考（用來查/寫 cache）。
兩個參考都是 borrow（`&'a`），不持有 ownership。

🔸 fetch_statistics()

```rust
// metadata.rs (line 210-213)
pub async fn fetch_statistics(&self, table_schema: &SchemaRef) -> Result<Statistics> {
    let metadata = self.fetch_metadata().await?;
    Self::statistics_from_parquet_metadata(&metadata, table_schema)
}
```

先呼叫 `fetch_metadata()` 拿到 `ParquetMetaData`，再用 `statistics_from_parquet_metadata()` 把 metadata 裡的統計資訊轉成 DataFusion 的 `Statistics` 格式。

關鍵在 `fetch_metadata()`：它會先查 cache。

🔸 fetch_metadata()：cache 查詢的核心

```rust
// metadata.rs (line 116-176)
pub async fn fetch_metadata(&self) -> Result<Arc<ParquetMetaData>> {
    let Self {
        store,
        object_meta,
        metadata_size_hint,
        decryption_properties,
        file_metadata_cache,
        coerce_int96: _,
    } = self;

    let fetch = ObjectStoreFetch::new(*store, object_meta);

    // 判斷是否可以 cache（加密檔案不 cache）
    let cache_metadata =
        !cfg!(feature = "parquet_encryption") || decryption_properties.is_none();

    // --- STEP 1: 查 cache ---
    if cache_metadata
        && let Some(file_metadata_cache) = file_metadata_cache.as_ref()
        && let Some(cached) = file_metadata_cache.get(&object_meta.location)
        && cached.is_valid_for(object_meta)
        && let Some(cached_parquet) = cached
            .file_metadata
            .as_any()
            .downcast_ref::<CachedParquetMetaData>()
    {
        return Ok(Arc::clone(cached_parquet.parquet_metadata()));
    }

    // --- STEP 2: cache miss，從 ObjectStore 讀 ---
    let mut reader =
        ParquetMetaDataReader::new().with_prefetch_hint(*metadata_size_hint);

    // 如果要 cache，就順便載入完整 page index
    if cache_metadata && file_metadata_cache.is_some() {
        reader = reader.with_page_index_policy(PageIndexPolicy::Optional);
    }

    let metadata = Arc::new(
        reader
            .load_and_finish(fetch, object_meta.size)
            .await
            .map_err(DataFusionError::from)?,
    );

    // --- STEP 3: 寫入 cache ---
    if cache_metadata && let Some(file_metadata_cache) = file_metadata_cache {
        file_metadata_cache.put(
            &object_meta.location,
            CachedFileMetadataEntry::new(
                (*object_meta).clone(),
                Arc::new(CachedParquetMetaData::new(Arc::clone(&metadata))),
            ),
        );
    }

    Ok(metadata)
}
```

這是整個 cache 機制的核心，拆解每一步：

STEP 1 - 查 cache：
- `file_metadata_cache.get(&object_meta.location)` 用檔案路徑當 key 查 cache
- `cached.is_valid_for(object_meta)` 比較 file size 和 last_modified 確認 cache 沒過期
- `downcast_ref::<CachedParquetMetaData>()` 把通用的 `dyn FileMetadata` 轉回具體型別
- 如果全部通過，直接回傳 `Arc<ParquetMetaData>`，零 I/O

STEP 2 - Cache miss，讀 ObjectStore：
- `ParquetMetaDataReader` 是 parquet crate 提供的 metadata 讀取器
- `with_prefetch_hint` 可以一次讀取 footer 尾部的 N bytes，減少 round trip
- `with_page_index_policy(PageIndexPolicy::Optional)` 告訴 reader 順便載入 page index
  這很重要：如果 cache 只存 footer 不存 page index，後續 page pruning 時還是要再讀一次
- `load_and_finish(fetch, object_meta.size)` 透過 `ObjectStoreFetch` 讀取實際 bytes

STEP 3 - 寫入 cache：
- `CachedFileMetadataEntry::new()` 包裝 metadata + ObjectMeta（用來驗證 cache 有效性）
- `CachedParquetMetaData::new()` 包裝 `Arc<ParquetMetaData>` 讓它實作 `FileMetadata` trait

🔸 ObjectStoreFetch：MetadataFetch 介面卡

```rust
// file_format.rs (line 1063-1084)
pub struct ObjectStoreFetch<'a> {
    store: &'a dyn ObjectStore,
    meta: &'a ObjectMeta,
}

impl MetadataFetch for ObjectStoreFetch<'_> {
    fn fetch(&mut self, range: Range<u64>) -> BoxFuture<'_, Result<Bytes, ParquetError>> {
        async {
            self.store
                .get_range(&self.meta.location, range)
                .await
                .map_err(ParquetError::from)
        }
        .boxed()
    }
}
```

`MetadataFetch` 是 parquet crate 定義的 trait，讓 `ParquetMetaDataReader` 可以按 byte range 讀取。
`ObjectStoreFetch` 把它接到 DataFusion 的 `ObjectStore` 上。
每次 `fetch(range)` 呼叫就是一次 `ObjectStore::get_range()`，也就是一次 HTTP GET with Range header。

🔸 CachedParquetMetaData：cache value 包裝

```rust
// metadata.rs (line 619-645)
pub struct CachedParquetMetaData(Arc<ParquetMetaData>);

impl CachedParquetMetaData {
    pub fn new(metadata: Arc<ParquetMetaData>) -> Self {
        Self(metadata)
    }
    pub fn parquet_metadata(&self) -> &Arc<ParquetMetaData> {
        &self.0
    }
}

impl FileMetadata for CachedParquetMetaData {
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn memory_size(&self) -> usize {
        self.0.memory_size()
    }
    fn extra_info(&self) -> HashMap<String, String> {
        let page_index =
            self.0.column_index().is_some() && self.0.offset_index().is_some();
        HashMap::from([("page_index".to_owned(), page_index.to_string())])
    }
}
```

`CachedParquetMetaData` 是一個 newtype wrapper（Rust pattern：用 tuple struct 包裝一個型別）。
它讓 `ParquetMetaData` 實作 `FileMetadata` trait，這樣才能放進通用的 `FileMetadataCache` 裡。

`memory_size()` 用來讓 LRU cache 計算目前佔了多少記憶體。
`extra_info()` 回報是否有 page index（除錯用）。
`as_any()` 讓外部可以 `downcast_ref::<CachedParquetMetaData>()` 拿回原始型別。

🔸 CachedFileMetadataEntry：cache entry 帶驗證資訊

```rust
// datafusion-execution/src/cache/cache_manager.rs
pub struct CachedFileMetadataEntry {
    pub meta: ObjectMeta,
    pub file_metadata: Arc<dyn FileMetadata>,
}

impl CachedFileMetadataEntry {
    pub fn is_valid_for(&self, current_meta: &ObjectMeta) -> bool {
        self.meta.size == current_meta.size
            && self.meta.last_modified == current_meta.last_modified
    }
}
```

cache entry 不只存 metadata，還存了當時的 `ObjectMeta`（file size + last_modified）。
每次從 cache 取出時，用 `is_valid_for()` 比對目前的檔案 metadata。
如果檔案被修改了（size 或 last_modified 不同），cache entry 自動失效。
這就是 cache invalidation 的策略：不需要 TTL，靠 ObjectStore 的 list 結果來判斷。


## FileMetadataCache 與 CacheManager

🔸 FileMetadataCache trait

```rust
// datafusion-execution/src/cache/cache_manager.rs
pub trait FileMetadataCache: CacheAccessor<Path, CachedFileMetadataEntry> {
    fn cache_limit(&self) -> usize;
    fn update_cache_limit(&self, limit: usize);
    fn list_entries(&self) -> HashMap<Path, FileMetadataCacheEntry>;
}
```

`FileMetadataCache` 繼承自 `CacheAccessor`，提供 `get` / `put` / `remove` / `len` 等方法。
額外加了 `cache_limit()` 和 `update_cache_limit()` 來控制記憶體上限。

🔸 DefaultFilesMetadataCache

```rust
// datafusion-execution/src/cache/file_metadata_cache.rs
pub struct DefaultFilesMetadataCache {
    state: Mutex<DefaultFilesMetadataCacheState>,
}
```

預設實作用 `Mutex` 包裝一個 LRU cache。
cache key 是 `Path`（檔案路徑），value 是 `CachedFileMetadataEntry`。
當記憶體用量超過上限時，LRU eviction 會自動淘汰最久沒被存取的 entry。

預設記憶體上限：50 MB。

```rust
pub const DEFAULT_METADATA_CACHE_LIMIT: usize = 50 * 1024 * 1024; // 50 MB
```

🔸 CacheManager

```rust
// datafusion-execution/src/cache/cache_manager.rs
pub struct CacheManager {
    file_statistic_cache: Option<Arc<dyn FileStatisticsCache>>,
    list_files_cache: Option<Arc<dyn ListFilesCache>>,
    file_metadata_cache: Arc<dyn FileMetadataCache>,
}
```

`CacheManager` 是 DataFusion 的全域 cache 管理器，掛在 `RuntimeEnv` 上。
它管三種 cache：

```
+-------------------+----------------------------------+----------------------------+
| Cache             | 存什麼                           | 用途                       |
+-------------------+----------------------------------+----------------------------+
| file_statistic    | Statistics (已計算好的統計)       | 避免重複計算 statistics    |
|   _cache          | (舊版，可選)                     |                            |
+-------------------+----------------------------------+----------------------------+
| list_files_cache  | Vec<ObjectMeta> (目錄列表)       | 避免重複 list directory    |
+-------------------+----------------------------------+----------------------------+
| file_metadata     | ParquetMetaData (footer +        | 避免重複讀 Parquet footer  |
|   _cache          |  page index)                     | PR #16971 + #17022         |
+-------------------+----------------------------------+----------------------------+
```


## 路徑 1：查詢執行時的 cache（CachedParquetFileReaderFactory）

路徑 1 在 `create_physical_plan()` 裡設定：

```rust
// file_format.rs (line 503-541)
async fn create_physical_plan(
    &self,
    state: &dyn Session,
    conf: FileScanConfig,
) -> Result<Arc<dyn ExecutionPlan>> {
    // ...
    // Use the CachedParquetFileReaderFactory
    let metadata_cache = state.runtime_env().cache_manager.get_file_metadata_cache();
    let store = state
        .runtime_env()
        .object_store(conf.object_store_url.clone())?;
    let cached_parquet_read_factory =
        Arc::new(CachedParquetFileReaderFactory::new(store, metadata_cache));
    source = source.with_parquet_file_reader_factory(cached_parquet_read_factory);
    // ...
}
```

把 `CachedParquetFileReaderFactory` 設為 ParquetSource 的 reader factory。
這樣查詢執行時，每個 worker 開 Parquet 檔案時就會走 cache。

🔸 CachedParquetFileReaderFactory

```rust
// reader.rs (line 182-234)
pub struct CachedParquetFileReaderFactory {
    store: Arc<dyn ObjectStore>,
    metadata_cache: Arc<dyn FileMetadataCache>,
}

impl ParquetFileReaderFactory for CachedParquetFileReaderFactory {
    fn create_reader(
        &self,
        partition_index: usize,
        partitioned_file: PartitionedFile,
        metadata_size_hint: Option<usize>,
        metrics: &ExecutionPlanMetricsSet,
    ) -> datafusion_common::Result<Box<dyn AsyncFileReader + Send>> {
        // ... 建立 ParquetObjectReader
        Ok(Box::new(CachedParquetFileReader::new(
            file_metrics,
            Arc::clone(&self.store),
            inner,
            partitioned_file,
            Arc::clone(&self.metadata_cache),
            metadata_size_hint,
        )))
    }
}
```

跟 `DefaultParquetFileReaderFactory` 的差別：回傳 `CachedParquetFileReader` 而不是 `ParquetFileReader`。

🔸 CachedParquetFileReader::get_metadata()

```rust
// reader.rs (line 268-322)
impl AsyncFileReader for CachedParquetFileReader {
    fn get_bytes(
        &mut self,
        range: Range<u64>,
    ) -> BoxFuture<'_, parquet::errors::Result<Bytes>> {
        // data bytes 不 cache，直接讀 ObjectStore
        let bytes_scanned = range.end - range.start;
        self.file_metrics.bytes_scanned.add(bytes_scanned as usize);
        self.inner.get_bytes(range)
    }

    fn get_metadata<'a>(
        &'a mut self,
        options: Option<&'a ArrowReaderOptions>,
    ) -> BoxFuture<'a, parquet::errors::Result<Arc<ParquetMetaData>>> {
        // metadata 走 cache
        async move {
            DFParquetMetadata::new(&self.store, &object_meta)
                .with_file_metadata_cache(Some(Arc::clone(&metadata_cache)))
                .with_metadata_size_hint(self.metadata_size_hint)
                .fetch_metadata()
                .await
                .map_err(|e| ParquetError::General(...))
        }
        .boxed()
    }
}
```

注意：`get_bytes()`（讀實際資料）不走 cache，直接讀 ObjectStore。
只有 `get_metadata()`（讀 footer + page index）走 `DFParquetMetadata::fetch_metadata()`。
這確保路徑 1 和路徑 2 共用同一個 cache 和同一套 cache 邏輯。


## 完整流程圖

一個查詢的完整 cache 流程：

```
SELECT * FROM parquet_table WHERE id > 100

Phase 1：Query Planning（路徑 2）
+------------------+     +-------------------+     +-----------+
| ListingTable     | --> | ParquetFormat     | --> | DFParquet |
| collect_stat     |     | ::infer_stats()   |     | Metadata  |
| _and_ordering()  |     |                   |     | ::fetch_  |
+------------------+     +-------------------+     | metadata()|
                                                   +-----------+
                                                        |
                                          cache miss    v    cache hit
                                       +----------+ +------+ +--------+
                                       |ObjectStore| |return| |return  |
                                       |get_range()| |cached| |Arc<    |
                                       +----------+ |      | |Parquet |
                                            |        +------+ |Meta>  |
                                            v                  +--------+
                                       +---------+
                                       |put cache|
                                       +---------+

Phase 2：Query Execution（路徑 1）
+------------------+     +-------------------+     +-----------+
| DataSourceExec   | --> | ParquetOpener     | --> | Cached    |
| ::execute()      |     | ::open()          |     | Parquet   |
|                  |     |                   |     | FileReader|
+------------------+     +-------------------+     | ::get_    |
                                                   | metadata()|
                                                   +-----------+
                                                        |
                                          cache HIT ✅   v    (Phase 1 already cached)
                                                   +--------+
                                                   |return   |
                                                   |Arc<     |
                                                   |Parquet  |
                                                   |Meta>    |
                                                   +--------+
```

Phase 1（planning）時 cache miss，讀 ObjectStore 一次，寫入 cache。
Phase 2（execution）時 cache hit，直接用 Phase 1 cache 的 metadata，零 I/O。
這就是 1.3s -> 0.164s 的加速原因。


## 效能影響

```
+-------------------+----------+----------+---------+
| 場景              | 無 cache | 有 cache | 加速比  |
+-------------------+----------+----------+---------+
| S3 ClickHouse     | 1.3s     | 0.164s   | 7.9x   |
| 100M rows bench   | 4.6s     | 0.37s    | 12.4x  |
+-------------------+----------+----------+---------+
```

加速來源：
- Parquet footer 通常在檔案尾部 1-10 KB
- Page index 可能 10-100 KB
- 每次讀 footer 需要 1-2 次 HTTP round trip（先讀 8 bytes magic，再讀 footer 長度指定的 bytes）
- 如果有 100 個檔案，就是 100-200 次 HTTP request
- Cache 命中後，這些 HTTP request 全部省掉


## 與 CachingObjectStore 的差異

```
+--------------------+----------------------------+----------------------------+
| 比較               | FileMetadataCache          | CachingObjectStore         |
|                    | (PR #16971 + #17022)       | (Sail Issue #1015)         |
+--------------------+----------------------------+----------------------------+
| Cache 什麼         | Parquet metadata           | 任意 byte range            |
|                    | (footer + page index)      | (data blocks)              |
+--------------------+----------------------------+----------------------------+
| Cache 在哪一層     | AsyncFileReader 層         | ObjectStore trait 層       |
|                    | (format-specific)          | (format-agnostic)          |
+--------------------+----------------------------+----------------------------+
| 受惠的格式         | 只有 Parquet               | 所有格式                   |
|                    |                            | (CSV, JSON, Parquet, ...)  |
+--------------------+----------------------------+----------------------------+
| 儲存位置           | 純記憶體 (Mutex + LRU)     | 記憶體 + 磁碟 (Foyer)     |
+--------------------+----------------------------+----------------------------+
| 預設記憶體上限     | 50 MB                      | 可設定                     |
+--------------------+----------------------------+----------------------------+
| 互斥嗎？           | 不互斥，可以同時用         | 不互斥，可以同時用         |
+--------------------+----------------------------+----------------------------+
```

FileMetadataCache 和 CachingObjectStore 在不同層做 cache，可以同時啟用：

```
CachedParquetFileReader::get_metadata()     <-- FileMetadataCache (metadata)
  |
  v (cache miss)
DFParquetMetadata::fetch_metadata()
  |
  v
ObjectStoreFetch::fetch()
  |
  v
ObjectStore::get_range()
  |
  v
CachingObjectStore                          <-- Foyer Cache (data bytes)
  |
  v (cache miss)
LoggingObjectStore -> RuntimeAwareObjectStore -> LazyObjectStore -> S3
```

如果兩個都啟用：
- 第一次查詢：metadata cache miss -> Foyer cache miss -> 讀 S3 -> 寫入兩層 cache
- 第二次查詢：metadata cache hit -> 連 ObjectStore 都不用呼叫

FileMetadataCache 省的是 metadata I/O（每次查詢規劃時）。
CachingObjectStore 省的是 data I/O（讀取實際 row group 資料時）。
兩者互補，不衝突。


## Cache 過期判斷機制

DataFusion 的 metadata cache 沒有 TTL（沒有「N 秒後自動過期」的機制）。
它靠 ObjectMeta 比對 來被動判斷 cache 是否還有效。

🔸 is_valid_for()：過期判斷的核心

```rust
// datafusion-execution/src/cache/cache_manager.rs
pub struct CachedFileMetadataEntry {
    pub meta: ObjectMeta,                   // cache 當時存的檔案資訊
    pub file_metadata: Arc<dyn FileMetadata>,
}

impl CachedFileMetadataEntry {
    pub fn is_valid_for(&self, current_meta: &ObjectMeta) -> bool {
        self.meta.size == current_meta.size
            && self.meta.last_modified == current_meta.last_modified
    }
}
```

只比兩個欄位：file size 和 last_modified。
沒有比 e_tag，也沒有比 version。

🔸 current_meta 從哪來

```
查詢開始
    |
    v
Phase 0: list directory
    ListingTable 呼叫 ObjectStore::list(prefix)
    拿到每個檔案的 ObjectMeta {
        location:      "s3://bucket/orders/part-001.parquet",
        size:          104857600,        // 100 MB
        last_modified: 2025-03-15T10:30:00Z,
        e_tag:         Some("abc123"),
    }
    |
    v
Phase 1: 對每個檔案讀 metadata（footer）
    file_metadata_cache.get(&path)    拿到 cached entry
    cached.is_valid_for(&object_meta) 用 Phase 0 的 ObjectMeta 比對
    |
    +→ size=100MB 且 last_modified=2025-03-15T10:30:00Z
    |  跟 cache 裡存的一樣 → HIT，不讀 S3
    |
    +→ 不一樣（檔案被改過）→ 過期，重新從 S3 讀 footer
```

關鍵：is_valid_for() 用的 current_meta 不是額外發 HEAD 請求拿的，
而是 Phase 0 的 list 結果裡已經包含了。
所以這個過期判斷不會增加額外的 S3 請求。

🔸 三層 cache 的過期信任鏈

```
file_listing_cache（Sail 用 Moka，有 TTL）
    |
    | TTL 過期 → 重新 ObjectStore::list()
    | TTL 未過期 → 用 cache 裡的 list 結果
    |
    v
ObjectMeta（list 結果裡的 size + last_modified）
    |
    v
file_metadata_cache.is_valid_for()
    |
    +→ 一致 → footer cache 有效，零 I/O
    +→ 不一致 → footer cache 過期，重讀 S3
    |
    v
file_statistics_cache
    （跟 metadata cache 類似，也靠 ObjectMeta 比對）
```

真正控制「多快發現檔案被改了」的是 file_listing_cache 的 TTL。
metadata cache 本身不控制過期，它被動依賴 listing cache 提供的最新 ObjectMeta。

🔸 每一層的過期策略

```
+----------------------------+-------------------+----------------------------+
| Cache                      | 過期策略          | 說明                       |
+----------------------------+-------------------+----------------------------+
| file_listing_cache         | Moka TTL          | TTL 到期 → 重新 list       |
| (Sail: MokaFileListingCa   | + LRU 驅逐        | 這是唯一真正跟 S3 確認     |
|  che)                      |                   | 「檔案有沒有變」的地方     |
+----------------------------+-------------------+----------------------------+
| file_metadata_cache        | ObjectMeta 比對   | 比對 size + last_modified  |
| (Sail: MokaFileMetadataC   | + Moka TTL 保底   | 資訊來自 listing cache     |
|  ache)                     |                   | Moka TTL 是額外的保底機制  |
+----------------------------+-------------------+----------------------------+
| file_statistics_cache      | ObjectMeta 比對   | 跟 metadata cache 一樣     |
| (Sail: MokaFileStatistic   | + Moka TTL 保底   |                            |
|  sCache)                   |                   |                            |
+----------------------------+-------------------+----------------------------+
```

🔸 DataFusion 預設 vs Sail 的差異

DataFusion 的 DefaultFilesMetadataCache 只有 Mutex + LRU，沒有 TTL。
Sail 用 Moka 替換，額外加了 TTL 和 size_limit：

```rust
// sail-cache/src/file_metadata_cache.rs (line 21-47)
pub fn new(ttl: Option<u64>, size_limit: Option<u64>) -> Self {
    let mut builder = Cache::builder().eviction_policy(EvictionPolicy::lru());

    if let Some(ttl) = ttl {
        builder = builder.time_to_live(Duration::from_secs(ttl));
    }                           // ^^^^^^^^^^^^^^ DataFusion 沒有的
    if let Some(size_limit) = size_limit {
        builder = builder
            .weigher(|_key, entry| entry.file_metadata.memory_size() as u32)
            .max_capacity(size_limit);
    }
    // ...
}
```

Sail 的 TTL 設定來自 application.yaml：

```yaml
# crates/sail-common/src/config/application.yaml
cache:
  type: "global"          # global / session / none
  file_metadata:
    ttl: 60               # 秒，0 表示不設 TTL
    size_limit: "50M"
  file_statistics:
    ttl: 60
    size_limit: "50M"
  file_listing:
    ttl: 60
    size_limit: "50M"
```

🔸 為什麼 DataFusion 不用 TTL 也沒問題

DataFusion 的預設 cache（DefaultFilesMetadataCache）沒有 TTL，
但因為每次查詢都會先 list directory 拿到最新的 ObjectMeta，
is_valid_for() 會自動偵測到檔案被改過。

唯一的盲點是：如果 list directory 本身也被 cache 了，
那就要等 listing cache 的 TTL 到期才能發現檔案變化。

```
情境：S3 上的 orders.parquet 被覆蓋了（同名但內容不同）

1. listing cache TTL 未到期
   → list 結果還是舊的 ObjectMeta
   → is_valid_for() 拿舊 ObjectMeta 比，還是一致
   → metadata cache HIT → 讀到舊的 footer（過期資料！）

2. listing cache TTL 到期
   → 重新 ObjectStore::list()
   → 拿到新的 ObjectMeta（size 或 last_modified 不同）
   → is_valid_for() 比對失敗
   → metadata cache 過期 → 重讀新的 footer

所以發現過期的最大延遲 = listing cache 的 TTL（Sail 預設 1800 秒 = 30 分鐘，且預設關閉）
```

🔸 Data Lake 場景為什麼更安全

Delta Lake / Iceberg 的檔案是 immutable（write-once）。
刪除是透過 metadata 標記，不是覆蓋檔案。
所以同一個 path 的 bytes 永遠不會變，
is_valid_for() 永遠回傳 true，cache 永遠有效。

```
Delta Lake 新增資料：
  寫入新檔案 part-00004.parquet（新 path，不影響舊 cache）

Delta Lake 刪除資料：
  metadata 標記 part-00002.parquet 為刪除
  實際檔案還在 S3 上
  但 list directory 不再回傳它
  → 不會被查詢讀到
  → cache 裡的 entry 靠 LRU 自然驅逐

Delta Lake compaction：
  合併 part-001 + part-002 → part-005（新 path）
  舊檔案標記刪除
  新檔案的 cache 從零開始 warm up
```

所以 DataFusion 的 is_valid_for() 設計對 Data Lake 場景幾乎是完美的：
immutable 檔案不需要 TTL，listing 層保證只讀到有效檔案。
