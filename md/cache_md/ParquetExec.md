# DataFusion ParquetExec 完整調用鏈：從 SQL 查詢到 ObjectStore::get_range

這篇文章追蹤 DataFusion 讀取 Parquet 檔案的完整路徑，
從 DataSourceExec::execute 一路到 ObjectStore::get_range。

Sail 使用的版本：DataFusion 52.1.0（datafusion-datasource-parquet-52.1.0）


## 整體流程圖

```
SELECT * FROM my_table WHERE col > 10
         |
         v
+-------------------+
| DataSourceExec    |  ExecutionPlan::execute(partition)
|   .execute()      |
+-------------------+
         |
         v
+-------------------+
| FileStream        |  poll_next() loop: file by file
|   .poll_inner()   |
+-------------------+
         |
         | calls FileOpener::open(partitioned_file)
         v
+-------------------+
| ParquetOpener     |  FileOpener impl
|   .open()         |  -> returns a Future<BoxStream<RecordBatch>>
+-------------------+
         |
         | Step 1: create AsyncFileReader
         v
+-------------------------------+
| ParquetFileReaderFactory      |  trait: create_reader()
|   -> DefaultParquetFile-      |
|      ReaderFactory            |
+-------------------------------+
         |
         | returns ParquetFileReader (wraps ParquetObjectReader)
         v
+-------------------------------+
| ParquetObjectReader           |  parquet crate (arrow-rs)
|   store: Arc<dyn ObjectStore> |
|   path: Path                  |
+-------------------------------+
         |
         | Step 2: load metadata
         | ArrowReaderMetadata::load_async()
         |   -> AsyncFileReader::get_metadata()
         |     -> ParquetObjectReader::get_bytes()
         |       -> ObjectStore::get_range()    <-- first I/O
         |
         | Step 3: schema coercion
         | Step 4: build pruning predicates
         | Step 5: load page index (if needed)  <-- optional I/O
         | Step 6: row group pruning (stats + bloom filter)
         | Step 7: page index pruning
         |
         | Step 8: build ParquetRecordBatchStreamBuilder
         v
+-------------------------------+
| ParquetRecordBatchStream      |  arrow-rs parquet reader
|   .build()                    |
+-------------------------------+
         |
         | reads column chunks on demand
         | AsyncFileReader::get_bytes(range)
         |   -> ParquetObjectReader::get_bytes(range)
         |     -> ObjectStore::get_range(path, range)   <-- data I/O
         v
+-------------------------------+
| ObjectStore (trait object)    |
|   -> LoggingObjectStore       |
|     -> RuntimeAwareObjectStore|
|       -> LazyObjectStore      |
|         -> AmazonS3 / Local   |
+-------------------------------+
```


## Crate 對照表

```
+------------------------------------+------------------------------------------+
| Crate                              | 角色                                     |
+------------------------------------+------------------------------------------+
| datafusion-datasource              | DataSourceExec, FileStream, FileOpener   |
|                                    | FileScanConfig                           |
+------------------------------------+------------------------------------------+
| datafusion-datasource-parquet      | ParquetSource, ParquetOpener,            |
|                                    | ParquetFileReaderFactory,                |
|                                    | DefaultParquetFileReaderFactory           |
+------------------------------------+------------------------------------------+
| parquet (arrow-rs)                 | ParquetObjectReader, AsyncFileReader,    |
|                                    | ParquetRecordBatchStreamBuilder,         |
|                                    | ArrowReaderMetadata                      |
+------------------------------------+------------------------------------------+
| object_store                       | ObjectStore trait, get_range, get_ranges |
+------------------------------------+------------------------------------------+
| sail-object-store                  | LoggingObjectStore,                      |
|                                    | RuntimeAwareObjectStore,                 |
|                                    | LazyObjectStore                          |
+------------------------------------+------------------------------------------+
```


## 建議閱讀順序

```
1. source.rs     -> ParquetSource 結構體 + FileSource::create_file_opener
2. reader.rs     -> ParquetFileReaderFactory + DefaultParquetFileReaderFactory
3. opener.rs     -> ParquetOpener::open() 完整 pipeline
4. store.rs      -> ParquetObjectReader::get_bytes (parquet crate)
5. file_stream.rs -> FileStream::poll_inner (datafusion-datasource crate)
```


## 用 SELECT * FROM s3://bucket/data.parquet WHERE id > 100 當例子

假設這個查詢要讀一個 S3 上的 Parquet 檔案，
檔案有 3 個 row group，每個 row group 有 10000 行。


## Step 0：DataSourceExec::execute 建立 FileStream

DataSourceExec 是 DataFusion 所有檔案掃描的統一執行計畫。
它呼叫 FileSource::create_file_opener 拿到 ParquetOpener，
然後用 FileStream 包起來。

```rust
// datafusion-datasource/src/source.rs (DataSourceExec)
// 簡化版本
fn execute(&self, partition: usize, context: Arc<TaskContext>) -> Result<Stream> {
    let object_store = context.runtime_env().object_store(url)?;
    let opener = self.source.create_file_opener(object_store, config, partition)?;
    let stream = FileStream::new(config, partition, opener, metrics)?;
    Ok(Box::pin(stream))
}
```

重點：
- `context.runtime_env().object_store(url)` 從 RuntimeEnv 拿到 ObjectStore
- 在 Sail 裡這個 ObjectStore 已經被 LoggingObjectStore、RuntimeAwareObjectStore、LazyObjectStore 包裝過了
- `create_file_opener` 建立 ParquetOpener 並把 ObjectStore 傳進去


## Step 1：ParquetSource::create_file_opener

ParquetSource 實作 FileSource trait。
create_file_opener 負責建立 ParquetOpener，這是整個 Parquet 讀取的核心。

```rust
// datafusion-datasource-parquet/src/source.rs (line 511-572)
impl FileSource for ParquetSource {
    fn create_file_opener(
        &self,
        object_store: Arc<dyn ObjectStore>,
        base_config: &FileScanConfig,
        partition: usize,
    ) -> datafusion_common::Result<Arc<dyn FileOpener>> {
```

這個函數做三件事：

🔸 建立 ParquetFileReaderFactory

```rust
        // line 523-526
        let parquet_file_reader_factory =
            self.parquet_file_reader_factory.clone().unwrap_or_else(|| {
                Arc::new(DefaultParquetFileReaderFactory::new(object_store)) as _
            });
```

如果使用者沒有自訂 factory，就用 DefaultParquetFileReaderFactory。
DefaultParquetFileReaderFactory 持有 `Arc<dyn ObjectStore>`，
等一下建立的 ParquetObjectReader 會用這個 ObjectStore 來讀檔案。

🔸 建立 ParquetOpener

```rust
        // line 544-571
        let opener = Arc::new(ParquetOpener {
            partition_index: partition,
            projection: self.projection.clone(),
            batch_size: self.batch_size.expect("..."),
            limit: base_config.limit,
            predicate: self.predicate.clone(),
            table_schema: self.table_schema.clone(),
            metadata_size_hint: self.metadata_size_hint,
            metrics: self.metrics().clone(),
            parquet_file_reader_factory,
            pushdown_filters: self.pushdown_filters(),
            reorder_filters: self.reorder_filters(),
            enable_page_index: self.enable_page_index(),
            enable_bloom_filter: self.bloom_filter_on_read(),
            enable_row_group_stats_pruning: self.table_parquet_options.global.pruning,
            // ... 其他設定
        });
        Ok(opener)
    }
}
```

ParquetOpener 結構體是一個巨大的設定集合，包含了所有 Parquet 讀取需要的資訊：
- projection：要讀哪些欄位
- predicate：WHERE 條件（用來 pruning）
- batch_size：每個 RecordBatch 多少行
- limit：LIMIT N 的 N
- pushdown_filters：是否在 Parquet decoder 層做 filter
- enable_page_index：是否用 page index pruning
- enable_bloom_filter：是否用 bloom filter pruning


## Step 2：FileStream 驅動 ParquetOpener

FileStream 是 DataFusion 的通用檔案串流。
它持有一個 `VecDeque<PartitionedFile>`（要讀的檔案列表）和一個 `Arc<dyn FileOpener>`。

```rust
// datafusion-datasource/src/file_stream.rs (line 47-66)
pub struct FileStream {
    file_iter: VecDeque<PartitionedFile>,
    projected_schema: SchemaRef,
    remain: Option<usize>,
    file_opener: Arc<dyn FileOpener>,
    state: FileStreamState,
    // ...
}
```

當 DataFusion 的 ExecutionPlan 框架 poll FileStream 時：

```rust
// file_stream.rs (line 110, 簡化)
fn poll_inner(&mut self, cx: &mut Context<'_>) -> Poll<Option<Result<RecordBatch>>> {
    loop {
        match &mut self.state {
            FileStreamState::Idle => {
                // 取下一個檔案，呼叫 opener.open()
                match self.start_next_file().transpose() {
                    Ok(Some(future)) => self.state = FileStreamState::Open { future },
                    Ok(None) => return Poll::Ready(None),  // 沒有更多檔案
                    // ...
                }
            }
            FileStreamState::Open { future } => {
                // 等待 open() 完成，拿到 BoxStream<RecordBatch>
                let stream = ready!(future.poll(cx))?;
                self.state = FileStreamState::Scan { stream, next: None };
            }
            FileStreamState::Scan { stream, .. } => {
                // 從 stream 讀 RecordBatch
                let batch = ready!(stream.poll_next(cx));
                // ... 處理 limit, metrics 等
                return Poll::Ready(batch);
            }
            // ...
        }
    }
}
```

狀態機：
- Idle：取下一個檔案，呼叫 `file_opener.open(partitioned_file)`
- Open：等待 open() 完成（包含 metadata 載入、pruning 等）
- Scan：從回傳的 stream 逐批讀取 RecordBatch

`start_next_file` 呼叫 `self.file_opener.open(part_file)`，
對 Parquet 來說就是 `ParquetOpener::open()`。


## Step 3：ParquetOpener::open() — 核心 pipeline

這是整個 Parquet 讀取的重頭戲。
open() 回傳一個 Future，裡面包含完整的 Parquet 開檔 + 讀取 pipeline。

```rust
// datafusion-datasource-parquet/src/opener.rs (line 181-635)
impl FileOpener for ParquetOpener {
    fn open(&self, partitioned_file: PartitionedFile) -> Result<FileOpenFuture> {
```

open() 前半段在同步環境下準備參數，後半段包在 `Box::pin(async move { ... })` 裡非同步執行。

🔸 建立 AsyncFileReader

```rust
        // line 194-200
        let mut async_file_reader: Box<dyn AsyncFileReader> =
            self.parquet_file_reader_factory.create_reader(
                self.partition_index,
                partitioned_file.clone(),
                metadata_size_hint,
                &self.metrics,
            )?;
```

這裡呼叫了 ParquetFileReaderFactory::create_reader。
回傳的 Box<dyn AsyncFileReader> 就是讀取 Parquet 資料的入口。


🔸 載入 Parquet metadata（第一次 I/O）

```rust
        // 進入 async block 後
        // line 347-349
        let mut reader_metadata =
            ArrowReaderMetadata::load_async(&mut async_file_reader, options.clone())
                .await?;
```

ArrowReaderMetadata::load_async 會：
1. 讀 Parquet footer（檔案尾端的 metadata）
2. 解析 row group 資訊、schema、column chunk 偏移量等

這個操作最終呼叫：
- AsyncFileReader::get_metadata()
- 裡面呼叫 ParquetObjectReader::get_bytes(range)
- 最終到 ObjectStore::get_range(path, range)

第一個 I/O 請求通常是讀檔案尾端的幾 KB（footer）。
如果設了 metadata_size_hint，會一次多讀一些，避免兩次 round trip。


🔸 Schema Coercion

```rust
        // line 363-373
        let mut physical_file_schema = Arc::clone(reader_metadata.schema());

        if let Some(merged) = apply_file_schema_type_coercions(
            &logical_file_schema,
            &physical_file_schema,
        ) {
            physical_file_schema = Arc::new(merged);
            options = options.with_schema(Arc::clone(&physical_file_schema));
            reader_metadata = ArrowReaderMetadata::try_new(
                Arc::clone(reader_metadata.metadata()),
                options.clone(),
            )?;
        }
```

Parquet 檔案的 physical schema 可能跟 table 的 logical schema 不同。
例如 logical schema 是 Utf8View 但檔案裡是 Utf8，需要 coercion。
這一步只調整型別，不做 I/O。


🔸 建立 Pruning Predicates

```rust
        // line 410-414
        let (pruning_predicate, page_pruning_predicate) = build_pruning_predicates(
            predicate.as_ref(),
            &physical_file_schema,
            &predicate_creation_errors,
        );
```

把 WHERE 條件（predicate）轉成兩種 pruning 工具：
- pruning_predicate：用 row group 的 min/max 統計來判斷能不能跳過整個 row group
- page_pruning_predicate：用 page 的 min/max 統計來判斷能不能跳過個別 page


🔸 載入 Page Index（第二次可能的 I/O）

```rust
        // line 419-427
        if should_enable_page_index(enable_page_index, &page_pruning_predicate) {
            reader_metadata = load_page_index(
                reader_metadata,
                &mut async_file_reader,
                options.with_page_index(true),
            )
            .await?;
        }
```

Page Index 不存在 footer 裡面，需要額外的 I/O 去讀。
只有在 enable_page_index = true 且有 page pruning predicate 時才會讀。
這個 I/O 最終也是透過 ObjectStore::get_range。


🔸 建立 ParquetRecordBatchStreamBuilder

```rust
        // line 431-434
        let mut builder = ParquetRecordBatchStreamBuilder::new_with_metadata(
            async_file_reader,
            reader_metadata,
        );
```

把 AsyncFileReader 和 metadata 交給 arrow-rs 的 ParquetRecordBatchStreamBuilder。
這個 builder 會在 .build() 時建立實際的讀取串流。


🔸 Row Group Pruning（statistics-based）

```rust
        // line 469-524
        let file_metadata = Arc::clone(builder.metadata());
        let rg_metadata = file_metadata.row_groups();
        let access_plan = create_initial_plan(&file_name, extensions, rg_metadata.len())?;
        let mut row_groups = RowGroupAccessPlanFilter::new(access_plan);

        if let Some(predicate) = predicate.as_ref() {
            if enable_row_group_stats_pruning {
                row_groups.prune_by_statistics(
                    &physical_file_schema,
                    builder.parquet_schema(),
                    rg_metadata,
                    predicate,
                    &file_metrics,
                );
            }

            if enable_bloom_filter && !row_groups.is_empty() {
                row_groups
                    .prune_by_bloom_filters(
                        &physical_file_schema,
                        &mut builder,
                        predicate,
                        &file_metrics,
                    )
                    .await;
            }
        }
```

用我們的例子 `WHERE id > 100`：
- 如果 row group 0 的 id 欄位 max = 50，那整個 row group 0 可以跳過
- 如果 row group 1 有 bloom filter，可以進一步判斷
- 跳過的 row group 完全不會產生 I/O

bloom filter 的讀取需要 I/O（透過 builder 裡的 AsyncFileReader），
但 statistics pruning 只用 footer 裡已經讀到的 metadata。


🔸 Page Index Pruning

```rust
        // line 531-542
        if enable_page_index
            && !access_plan.is_empty()
            && let Some(p) = page_pruning_predicate
        {
            access_plan = p.prune_plan_with_page_index(
                access_plan,
                &physical_file_schema,
                builder.parquet_schema(),
                file_metadata.as_ref(),
                &file_metrics,
            );
        }
```

Page index pruning 更細粒度：
- 每個 column chunk 分成多個 page
- 如果某個 page 的 min/max 不符合 predicate，可以跳過
- 會產生 RowSelection（標記哪些行要讀、哪些要跳）


🔸 Apply Access Plan + Build Stream

```rust
        // line 545-571
        let mut prepared_plan =
            PreparedAccessPlan::from_access_plan(access_plan, rg_metadata)?;

        builder = prepared_plan.apply_to_builder(builder);

        if let Some(limit) = limit {
            builder = builder.with_limit(limit)
        }

        let stream = builder
            .with_projection(mask)        // 只讀需要的欄位
            .with_batch_size(batch_size)   // 每批多少行
            .build()?;                     // 建立串流
```

apply_to_builder 做兩件事：
- with_row_groups(indexes)：告訴 builder 只讀哪些 row group
- with_row_selection(selection)：告訴 builder 哪些行要讀

.build() 建立 ParquetRecordBatchStream，
之後每次 poll 都會從 ObjectStore 讀取 column chunk 資料。


🔸 Map Stream（projection + schema）

```rust
        // line 592-622
        let stream = stream.map_err(DataFusionError::from).map(move |b| {
            b.and_then(|mut b| {
                b = projector.project_batch(&b)?;
                if replace_schema {
                    let (_stream_schema, arrays, num_rows) = b.into_parts();
                    RecordBatch::try_new_with_options(
                        Arc::clone(&output_schema),
                        arrays,
                        &options,
                    )
                    .map_err(Into::into)
                } else {
                    Ok(b)
                }
            })
        });
```

每個 RecordBatch 讀出來後：
- projector.project_batch：做 projection（只保留 SELECT 需要的欄位）
- replace_schema：如果 physical schema 跟 output schema 不同，替換 schema


## Step 4：DefaultParquetFileReaderFactory::create_reader

回頭看 Step 3 裡的第一步：建立 AsyncFileReader。

```rust
// datafusion-datasource-parquet/src/reader.rs (line 145-175)
impl ParquetFileReaderFactory for DefaultParquetFileReaderFactory {
    fn create_reader(
        &self,
        partition_index: usize,
        partitioned_file: PartitionedFile,
        metadata_size_hint: Option<usize>,
        metrics: &ExecutionPlanMetricsSet,
    ) -> datafusion_common::Result<Box<dyn AsyncFileReader + Send>> {
        let file_metrics = ParquetFileMetrics::new(
            partition_index,
            partitioned_file.object_meta.location.as_ref(),
            metrics,
        );

        let store = Arc::clone(&self.store);
        let mut inner = ParquetObjectReader::new(
            store,
            partitioned_file.object_meta.location.clone(),
        )
        .with_file_size(partitioned_file.object_meta.size);

        if let Some(hint) = metadata_size_hint {
            inner = inner.with_footer_size_hint(hint)
        };

        Ok(Box::new(ParquetFileReader {
            inner,
            file_metrics,
            partitioned_file,
        }))
    }
}
```

重點：
- `Arc::clone(&self.store)` 把 ObjectStore 的引用計數 +1，不是深拷貝
- `ParquetObjectReader::new(store, path)` 建立 arrow-rs 的 Parquet reader
- `.with_file_size(size)` 告訴 reader 檔案大小，避免額外的 HEAD 請求
- `.with_footer_size_hint(hint)` 告訴 reader 第一次 I/O 讀多少 bytes（含 footer）
- 回傳 ParquetFileReader，它包裝了 ParquetObjectReader

ParquetFileReader 結構體：

```rust
// reader.rs (line 97-101)
pub struct ParquetFileReader {
    pub file_metrics: ParquetFileMetrics,
    pub inner: ParquetObjectReader,
    pub partitioned_file: PartitionedFile,
}
```


## Step 5：ParquetFileReader -> ParquetObjectReader -> ObjectStore

ParquetFileReader 實作 AsyncFileReader trait。
每次 Parquet reader 需要讀資料，都會呼叫 get_bytes 或 get_byte_ranges。

```rust
// reader.rs (line 103-131)
impl AsyncFileReader for ParquetFileReader {
    fn get_bytes(
        &mut self,
        range: Range<u64>,
    ) -> BoxFuture<'_, parquet::errors::Result<Bytes>> {
        let bytes_scanned = range.end - range.start;
        self.file_metrics.bytes_scanned.add(bytes_scanned as usize);
        self.inner.get_bytes(range)
    }

    fn get_byte_ranges(
        &mut self,
        ranges: Vec<Range<u64>>,
    ) -> BoxFuture<'_, parquet::errors::Result<Vec<Bytes>>> {
        let total: u64 = ranges.iter().map(|r| r.end - r.start).sum();
        self.file_metrics.bytes_scanned.add(total as usize);
        self.inner.get_byte_ranges(ranges)
    }
}
```

ParquetFileReader 做的事：
1. 記錄 bytes_scanned metrics
2. delegate 給 `self.inner`（ParquetObjectReader）

接下來是 parquet crate 裡的 ParquetObjectReader：

```rust
// parquet/src/arrow/async_reader/store.rs (line 54-63)
pub struct ParquetObjectReader {
    store: Arc<dyn ObjectStore>,
    path: Path,
    file_size: Option<u64>,
    metadata_size_hint: Option<usize>,
    // ...
}
```

AsyncFileReader 實作裡的 get_bytes：

```rust
// store.rs (line 187-189)
impl AsyncFileReader for ParquetObjectReader {
    fn get_bytes(&mut self, range: Range<u64>) -> BoxFuture<'_, Result<Bytes>> {
        self.spawn(|store, path| store.get_range(path, range))
    }

    fn get_byte_ranges(&mut self, ranges: Vec<Range<u64>>) -> BoxFuture<'_, Result<Vec<Bytes>>> {
        self.spawn(|store, path| async move { store.get_ranges(path, &ranges).await }.boxed())
    }
}
```

這就是最終的 I/O 調用：
- `store.get_range(path, range)` 讀一段 byte range
- `store.get_ranges(path, &ranges)` 讀多段 byte range（批量 I/O）

`self.spawn` 方法處理 optional runtime 切換：

```rust
// store.rs (line 143-168)
fn spawn<F, O, E>(&self, f: F) -> BoxFuture<'_, Result<O>>
where
    F: for<'a> FnOnce(&'a Arc<dyn ObjectStore>, &'a Path) -> BoxFuture<'a, Result<O, E>>
        + Send + 'static,
{
    match &self.runtime {
        Some(handle) => {
            // 如果有指定 runtime，在那個 runtime 上執行
            let path = self.path.clone();
            let store = Arc::clone(&self.store);
            handle.spawn(async move { f(&store, &path).await })
                // ...
        }
        None => f(&self.store, &self.path).map_err(|e| e.into()).boxed(),
        // 沒有指定 runtime，直接在當前 runtime 執行
    }
}
```


## 完整 I/O 時序

用我們的例子 `SELECT * FROM s3://bucket/data.parquet WHERE id > 100`：

```
Time  |  Action                                          | I/O?
------+--------------------------------------------------+------
  1   | DataSourceExec::execute(partition=0)              |  -
  2   |   FileStream 建立，狀態 = Idle                    |  -
  3   |   poll_inner: Idle -> Open                        |  -
  4   |     ParquetOpener::open(data.parquet)             |  -
  5   |       create_reader -> ParquetObjectReader        |  -
  6   |       ArrowReaderMetadata::load_async             |
  7   |         get_bytes(file_size-8..file_size)         | YES  讀 footer magic + length
  8   |         get_bytes(file_size-footer..file_size)    | YES  讀完整 footer
  9   |       schema coercion                             |  -
 10   |       build pruning predicates                    |  -
 11   |       row group pruning by statistics             |  -
      |         RG0: id max=50 < 100  -> SKIP            |
      |         RG1: id max=200 > 100 -> KEEP            |
      |         RG2: id max=300 > 100 -> KEEP            |
 12   |       bloom filter pruning (if enabled)           | YES  讀 bloom filter bytes
 13   |       page index pruning (if enabled)             |  -   用 Step 8 讀到的 page index
 14   |       builder.with_row_groups([1, 2])             |  -
      |       builder.with_projection(mask)               |  -
      |       builder.build()                             |  -
 15   |   poll_inner: Open -> Scan                        |  -
 16   |     stream.poll_next()                            |
 17   |       get_bytes(rg1_col_id_offset..end)           | YES  讀 RG1 的 id column chunk
 18   |       get_bytes(rg1_col_other_offset..end)        | YES  讀 RG1 的其他 column chunks
 19   |       decode -> RecordBatch                       |  -
 20   |       projector.project_batch()                   |  -
 21   |       return RecordBatch                          |  -
 22   |     stream.poll_next()                            |
 23   |       get_bytes(rg2_col_id_offset..end)           | YES  讀 RG2 的 id column chunk
 24   |       ...                                         | YES
 25   |     stream.poll_next() -> None                    |  -
 26   |   poll_inner: Scan -> Idle                        |  -
 27   |   poll_inner: Idle -> 沒有更多檔案 -> Done        |  -
```

注意：
- RG0 被 statistics pruning 跳過了，完全不產生 I/O
- 每個 column chunk 是獨立的 I/O 請求（get_bytes）
- 如果有 projection（只讀部分欄位），不需要讀所有 column chunks
- 如果有 filter pushdown + page index，可能只讀部分 pages


## 這跟 CachingObjectStore 的關係

從上面的流程可以看到，所有 Parquet 資料讀取最終都通過：

```
ParquetObjectReader::get_bytes(range)
    -> store.get_range(path, range)
```

而 `store` 就是 `Arc<dyn ObjectStore>`，
也就是 Sail 的 decorator chain：

```
LoggingObjectStore::get_range(path, range)
    -> CachingObjectStore::get_range(path, range)    <-- 在這裡快取
        -> RuntimeAwareObjectStore::get_range(path, range)
            -> LazyObjectStore::get_range(path, range)
                -> AmazonS3::get_range(path, range)  <-- 實際 HTTP 請求
```

CachingObjectStore 在 ObjectStore 層攔截所有 get_range 請求：
- Cache HIT：直接回傳快取的 bytes，不需要走到 S3
- Cache MISS：轉發給 inner（下一層），拿到 bytes 後放入快取

因為 Parquet reader 的所有 I/O 都通過 ObjectStore trait，
CachingObjectStore 可以快取：
- Footer metadata（避免每次查詢都重讀 footer）
- Column chunk 資料（第二次讀同一個 column chunk 直接從 cache 拿）
- Page index 資料

而且完全不需要理解 Parquet 格式。
它只看到 `get_range(path, 1024..2048)` 這樣的 byte range 請求，
不知道這段 bytes 是 footer、column chunk、還是 page index。
