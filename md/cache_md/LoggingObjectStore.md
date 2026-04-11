# ObjectStore Decorator Pattern：如何改造 DataFusion 但不動 DataFusion 源碼

Sail 透過 Rust 的 trait + decorator pattern，在不修改 DataFusion 任何一行源碼的前提下，
對所有檔案 I/O 加上 logging、runtime 切換、延遲初始化等功能。

這個章節用 `LoggingObjectStore` 當範例，解釋整個機制。


## ObjectStore trait：DataFusion 的檔案 I/O 介面

DataFusion 不直接呼叫 S3 / GCS / local filesystem。
它透過 `ObjectStore` trait 抽象所有檔案操作：

```rust
// object_store crate（Apache Arrow 生態系）
#[async_trait]
pub trait ObjectStore: Send + Sync + Debug + Display + 'static {
    async fn get(&self, location: &Path) -> Result<GetResult>;
    async fn get_opts(&self, location: &Path, options: GetOptions) -> Result<GetResult>;
    async fn get_range(&self, location: &Path, range: Range<u64>) -> Result<Bytes>;
    async fn get_ranges(&self, location: &Path, ranges: &[Range<u64>]) -> Result<Vec<Bytes>>;
    async fn put(&self, location: &Path, payload: PutPayload) -> Result<PutResult>;
    async fn put_opts(&self, ...) -> Result<PutResult>;
    async fn head(&self, location: &Path) -> Result<ObjectMeta>;
    async fn delete(&self, location: &Path) -> Result<()>;
    fn list(&self, prefix: Option<&Path>) -> BoxStream<'static, Result<ObjectMeta>>;
    // ... copy, rename 等
}
```

DataFusion 的 ParquetExec 讀檔案時：

```rust
// DataFusion 內部（我們不改這段）
let store = runtime_env.object_store(url)?;    // 拿到 Arc<dyn ObjectStore>
let data = store.get_range(&path, range).await?; // 讀 bytes
```

DataFusion 只認識 `Arc<dyn ObjectStore>` 這個介面。
它不關心背後是 S3、本地檔案、還是一個加了 cache 的包裝。

這就是 Rust 的 trait object（`dyn ObjectStore`）：
跟 Java 的 interface 或 Go 的 interface 類似，
呼叫方只看到介面，不知道背後的實作。


## Decorator Pattern：一層一層包裝

Sail 用 decorator pattern 在 ObjectStore 上面疊加功能。
每個 decorator 都實作 `ObjectStore` trait，
持有一個 `inner: Arc<dyn ObjectStore>` 指向下一層：

```
crates/sail-object-store/src/layers/
├── mod.rs              pub mod logging; pub mod runtime; pub mod lazy;
├── logging.rs          LoggingObjectStore   -- 記 log + tracing
├── runtime.rs          RuntimeAwareObjectStore -- 切換 tokio runtime
└── lazy.rs             LazyObjectStore      -- 延遲初始化
```

組合順序（在 `registry.rs` 裡完成）：

```
DataFusion
    |
    | Arc<dyn ObjectStore>  (DataFusion 只看到這個)
    v
+---------------------------+
| LoggingObjectStore        |  最外層：記 log + tracing span
|   inner ────────────────> |
+---------------------------+
            |
            v
+---------------------------+
| RuntimeAwareObjectStore   |  中間層：把 I/O 丟到專用 runtime
|   inner ────────────────> |  （選配，看設定決定是否啟用）
+---------------------------+
            |
            v
+---------------------------+
| LazyObjectStore           |  內層：第一次用才建立 S3 連線
|   inner (OnceCell) ─────> |
+---------------------------+
            |
            v
+---------------------------+
| AmazonS3 / Azure / GCS   |  最底層：真正的 HTTP client
+---------------------------+
```


## LoggingObjectStore 源碼解析

🔸 結構體定義

```rust
// crates/sail-object-store/src/layers/logging.rs (line 20-23)
#[derive(Debug)]
pub struct LoggingObjectStore {
    inner: Arc<dyn ObjectStore>,
}
```

只有一個欄位 `inner`，指向下一層的 ObjectStore。
`Arc<dyn ObjectStore>` 是 trait object：
- `Arc` = 引用計數智慧指標（多處共用同一個物件）
- `dyn ObjectStore` = 動態分派（runtime 才決定呼叫哪個實作）

🔸 建構函數

```rust
// line 25-29
impl LoggingObjectStore {
    pub fn new(inner: Arc<dyn ObjectStore>) -> Self {
        Self { inner }
    }
}
```

接收任何實作 ObjectStore 的東西，包進去。

🔸 Display trait

```rust
// line 31-35
impl fmt::Display for LoggingObjectStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "LoggingObjectStore({})", self.inner)
    }
}
```

印出自己的名字 + inner 的名字。
例如 `LoggingObjectStore(LazyObjectStore(AmazonS3(bucket)))`。
這就是 decorator pattern 的嵌套顯示。

🔸 實作 ObjectStore trait

```rust
// line 37-39
#[async_trait::async_trait]
#[warn(clippy::missing_trait_methods)]
impl ObjectStore for LoggingObjectStore {
```

`#[async_trait::async_trait]`：
Rust 的 trait 原生不支援 async fn（截至 2024 年），
`async_trait` macro 會把 `async fn` 轉成回傳 `Pin<Box<dyn Future>>` 的普通 fn。

`#[warn(clippy::missing_trait_methods)]`：
如果有 trait method 沒有實作，clippy 會警告。
確保每個 ObjectStore 方法都有被包裝到。

🔸 get_range 方法（最常被 Parquet reader 呼叫的）

```rust
// line 174-195
async fn get_range(&self, location: &Path, range: Range<u64>) -> Result<Bytes> {
    debug!("get_range: location: {location:?} range: {range:?}");

    let span = Span::enter_with_local_parent("ObjectStore::get_range")
        .with_properties(|| {
            [
                (SpanAttribute::OBJECT_STORE_INSTANCE, self.inner.to_string()),
                (SpanAttribute::OBJECT_STORE_LOCATION, location.to_string()),
                (SpanAttribute::OBJECT_STORE_RANGE, format!("{range:?}")),
            ]
        });
    self.inner
        .get_range(location, range)
        .in_span_with_recorder(span, |span, output| {
            record_error(span, output);
            if let Ok(output) = output {
                span.add_property(|| {
                    (SpanAttribute::OBJECT_STORE_SIZE, output.len().to_string())
                });
            }
        })
        .await
}
```

逐行解釋：

```
debug!("get_range: ...")
    -> 印一行 log（log level = debug）

let span = Span::enter_with_local_parent("ObjectStore::get_range")
    -> 建立 fastrace tracing span（分散式追蹤用）
    -> 記錄：哪個 ObjectStore instance、讀哪個檔案、讀哪個 byte range

self.inner.get_range(location, range)
    -> 把請求轉發給下一層（RuntimeAwareObjectStore 或 LazyObjectStore）
    -> 這就是 decorator 的核心：自己做完 logging，然後 delegate 給 inner

.in_span_with_recorder(span, |span, output| { ... })
    -> 等 inner 回傳後，在 span 上記錄結果
    -> 成功：記錄回傳的 bytes 大小
    -> 失敗：record_error 記錄錯誤訊息

.await
    -> 等待 async 操作完成
```

🔸 其他方法都是同樣的 pattern

```
put        -> debug! + span + self.inner.put()
put_opts   -> debug! + span + self.inner.put_opts()
get        -> debug! + span + self.inner.get()
get_opts   -> debug! + span + self.inner.get_opts()
head       -> debug! + span + self.inner.head()
delete     -> debug! + span + self.inner.delete()
list       -> debug! + span + self.inner.list()
copy       -> debug! + span + self.inner.copy()
rename     -> debug! + span + self.inner.rename()
```

每個方法：
1. 印 debug log
2. 建 tracing span（記錄操作參數）
3. 轉發給 `self.inner`（下一層）
4. 記錄結果到 span
5. 回傳

LoggingObjectStore 完全不改變任何資料，只是「觀察」然後「轉發」。


## RuntimeAwareObjectStore：切換 Tokio Runtime

另一個 decorator，功能不同但 pattern 一模一樣：

```rust
// crates/sail-object-store/src/layers/runtime.rs (line 20-24)
#[derive(Debug)]
pub struct RuntimeAwareObjectStore {
    inner: Arc<dyn ObjectStore>,
    handle: Handle,               // 指向專用的 IO runtime
}
```

```rust
// line 146-152
async fn get_range(&self, location: &Path, range: Range<u64>) -> Result<Bytes> {
    let inner = self.inner.clone();
    let location = location.clone();
    self.handle
        .spawn(async move { inner.get_range(&location, range).await })
        .await?
}
```

這個 decorator 做的事：
- 把 inner.get_range() 丟到專用的 IO tokio runtime 執行
- 避免 S3 HTTP 請求阻塞主要的 compute runtime
- 其他什麼都不做，原樣回傳結果

同樣是 decorator pattern：
`self.handle.spawn(inner.get_range())` 取代 `inner.get_range()`。


## LazyObjectStore：延遲初始化

```rust
// crates/sail-object-store/src/layers/lazy.rs (line 40-53)
pub struct LazyObjectStore<S, F> {
    inner: Arc<ObjectStoreCell<S, F>>,
}

struct ObjectStoreCell<S, F> {
    cell: OnceCell<S>,       // 只初始化一次
    initializer: F,          // 初始化函數
}
```

```rust
// line 139-145
async fn get_opts(&self, location: &Path, options: GetOptions) -> Result<GetResult> {
    self.inner
        .get_or_try_init()   // 第一次呼叫時才建立 S3 client
        .await?
        .get_opts(location, options)
        .await
}
```

這個 decorator 做的事：
- 第一次被呼叫時才建立 S3 連線（OnceCell 保證只初始化一次）
- 建好之後就直接 delegate，沒有額外開銷
- 好處：啟動時不需要等所有 S3 bucket 都連上


## 在 registry.rs 裡組裝

```rust
// crates/sail-object-store/src/registry.rs

// line 78-86: get_store() 裡面決定是否加 RuntimeAwareObjectStore
fn get_store(&self, url: &Url) -> Result<Arc<dyn ObjectStore>> {
    self.stores.entry(key).or_try_insert_with(|| {
        if self.runtime.io_runtime_for_object_store() {
            // 加 RuntimeAwareObjectStore
            Ok(Arc::new(RuntimeAwareObjectStore::try_new(
                || get_dynamic_object_store(url),
                self.runtime.io().clone(),
            )?))
        } else {
            // 不加，直接用
            get_dynamic_object_store(url)
        }
    })
}

// line 94-165: get_dynamic_object_store() 建立底層 store + 最外層 Logging
fn get_dynamic_object_store(url: &Url) -> Result<Arc<dyn ObjectStore>> {
    let store = match scheme {
        AmazonS3 => Arc::new(LazyObjectStore::new(
            move || async move { get_s3_object_store(&url).await }
        )),
        Azure => Arc::new(LazyObjectStore::new(...)),
        GCS   => Arc::new(LazyObjectStore::new(...)),
        ...
    };
    // 最外層包 LoggingObjectStore
    Ok(Arc::new(LoggingObjectStore::new(store)))
}
```

組裝結果（以 S3 為例，開啟 IO runtime）：

```
Arc<dyn ObjectStore>
  = LoggingObjectStore
      inner = RuntimeAwareObjectStore
                inner = LazyObjectStore
                          inner (OnceCell) = AmazonS3 client
```

DataFusion 拿到的是 `Arc<dyn ObjectStore>`，
它呼叫 `.get_range()` 時，會依序經過：
1. LoggingObjectStore（記 log + tracing）
2. RuntimeAwareObjectStore（切到 IO runtime）
3. LazyObjectStore（確保 S3 client 已初始化）
4. AmazonS3（發 HTTP GET with Range header）

每一層都只做自己的事，然後 delegate 給 inner。
DataFusion 完全不知道有幾層包裝。


## 為什麼 CachingObjectStore 也用同樣的 pattern

理解了上面的 decorator pattern，就知道 CachingObjectStore 怎麼做了：

```
改動前:
  LoggingObjectStore -> RuntimeAwareObjectStore -> LazyObjectStore -> S3

改動後:
  LoggingObjectStore -> CachingObjectStore -> RuntimeAwareObjectStore -> LazyObjectStore -> S3
                        ^^^^^^^^^^^^^^^^
                        新增一層，只改 registry.rs 的組裝
```

CachingObjectStore 的 get_range 邏輯：

```rust
async fn get_range(&self, location: &Path, range: Range<u64>) -> Result<Bytes> {
    let parts = split_into_parts(range, self.part_size);

    let mut result = Vec::new();
    for (part_id, part_range) in parts {
        let key = PartKey { path: location.to_string(), part_id };
        if let Some(cached) = self.cache.get(&key).await? {
            // HIT: 從 Foyer cache 讀
            result.push(cached.slice(part_range));
        } else {
            // MISS: 轉發給 inner（下一層），然後回填 cache
            let full_part_range = part_id * self.part_size..(part_id + 1) * self.part_size;
            let data = self.inner.get_range(location, full_part_range).await?;
            self.cache.insert(key, data.clone());
            result.push(data.slice(part_range));
        }
    }

    Ok(concat_bytes(result))
}
```

跟 LoggingObjectStore 一樣的 pattern：
- 持有 `inner: Arc<dyn ObjectStore>`
- 實作 `ObjectStore` trait
- 自己做完 cache 邏輯，然後 delegate 給 inner
- DataFusion 完全不知道 cache 的存在


## 總結：Decorator Pattern 的威力

```
+------------------------------+-----------------------------------+
| 你想做的事                   | 怎麼做                            |
+------------------------------+-----------------------------------+
| 記 log                       | LoggingObjectStore（已有）        |
| 切 runtime                   | RuntimeAwareObjectStore（已有）   |
| 延遲初始化                   | LazyObjectStore（已有）           |
| 加 cache                     | CachingObjectStore（要做的）      |
| 限流                         | RateLimitObjectStore（未來可加）  |
| 加密                         | EncryptingObjectStore（未來可加） |
| metrics                      | MetricsObjectStore（未來可加）    |
+------------------------------+-----------------------------------+
```

每種功能都是一個獨立的 decorator：
- 實作 `ObjectStore` trait
- 持有 `inner: Arc<dyn ObjectStore>`
- 做完自己的事後 delegate 給 inner
- 在 `registry.rs` 裡決定要疊幾層、什麼順序

不需要改 DataFusion 任何源碼。
不需要 fork object_store crate。
就跟你在 Java 裡用 `BufferedInputStream(GZIPInputStream(FileInputStream))` 一樣，
一層一層包，每層做一件事。
