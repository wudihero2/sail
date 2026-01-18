# Cargo.toml 詳細解說

這份文件解釋 Sail 專案的 Cargo.toml 配置檔。這是一個 Rust Workspace 專案，包含約 30 個 crates。

## Workspace 基本設定

```toml
[workspace]
members = [
    "crates/*",
]
resolver = "2"
```

🔸 members: 指定 crates/ 目錄下所有子目錄都是 workspace 成員
🔸 resolver = "2": 使用 Cargo 新版依賴解析器，更精確處理 features 和平台相依性

## 共享 Package 元資料

```toml
[workspace.package]
version = "0.4.2"
authors = ["LakeSail <hello@lakesail.com>"]
edition = "2021"
homepage = "https://lakesail.com"
license = "Apache-2.0"
readme = "README.md"
repository = "https://github.com/lakehq/sail"
rust-version = "1.87.0"
```

🔸 所有子 crate 可繼承這些設定，避免重複宣告
🔸 rust-version = "1.87.0": 最低 Rust 版本要求，確保團隊使用一致版本

## Clippy Lints 規則

```toml
[workspace.lints.clippy]
unwrap_used = "deny"
expect_used = "deny"
panic = "deny"
dbg_macro = "deny"
todo = "deny"
```

🔸 這些規則強制程式碼品質:
- unwrap_used / expect_used: 禁止 .unwrap() 和 .expect()，必須用 ? 或 match 處理錯誤
- panic: 禁止 panic!() 巨集
- dbg_macro: 禁止 dbg!() 除錯巨集進入 production
- todo: 禁止 todo!() 巨集，確保沒有未完成的程式碼

## 依賴分類說明

### 🔸 非同步執行框架

```toml
tokio = { version = "1.47.1", features = ["full"] }
tokio-stream = { version = "0.1.17", features = ["time", "io-util"] }
async-trait = "0.1.89"
async-recursion = "1.1.1"
async-stream = "0.3.6"
futures = "0.3.31"
```

Tokio 是 Rust 非同步執行核心，features = ["full"] 啟用所有功能 (runtime, io, net, time 等)

### 🔸 gRPC 與 Protocol Buffers

```toml
tonic = { version = "0.14.1", features = ["tls-ring", "tls-native-roots", "gzip", "zstd"] }
tonic-build = "0.14.1"
tonic-reflection = "0.14.1"
tonic-health = "0.14.1"
prost = "0.14"
prost-build = "0.14"
pbjson = "0.8.0"
```

🔸 tonic: gRPC 框架，實作 Spark Connect 協議
🔸 features 說明:
- tls-ring: 使用 ring 加密庫做 TLS
- tls-native-roots: 使用系統 CA 憑證
- gzip/zstd: 壓縮支援，減少網路傳輸量
🔸 prost: Protocol Buffers 編解碼
🔸 pbjson: 讓 protobuf 訊息支援 JSON 序列化

### 🔸 序列化框架

```toml
serde = { version = "1.0.228", features = ["derive"] }
serde_json = "1.0.145"
serde_yaml = "0.9.34"
serde_with = { version = "3.15.0", default-features = false, features = ["base64", "std", "macros"] }
```

Serde 是 Rust 序列化標準，支援 JSON/YAML 等格式轉換

### 🔸 Apache Arrow 生態系

```toml
arrow = { version = "57.0.0", features = ["chrono-tz"] }
arrow-buffer = { version = "57.0.0" }
arrow-schema = { version = "57.0.0", features = ["serde"] }
arrow-flight = { version = "57.0.0" }
arrow-pyarrow = { version = "57.0.0" }
parquet = { version = "57.0.0" }
serde_arrow = { version = "0.13.7", features = ["arrow-57"] }
```

🔸 Arrow 是列式記憶體格式，Sail 的資料交換核心
🔸 arrow-flight: Arrow 資料的 gRPC 傳輸協議
🔸 arrow-pyarrow: Python PyArrow 互操作
🔸 parquet: 列式檔案格式讀寫
🔸 chrono-tz feature: 時區感知的時間戳處理

### 🔸 DataFusion 查詢引擎

```toml
datafusion = { version = "51.0.0", features = ["serde", "avro", "sql"] }
datafusion-common = { version = "51.0.0", features = ["object_store", "avro"] }
datafusion-datasource = { version = "51.0.0" }
datafusion-expr = { version = "51.0.0" }
datafusion-expr-common = { version = "51.0.0" }
datafusion-proto = { version = "51.0.0" }
datafusion-functions = { version = "51.0.0" }
datafusion-functions-nested = { version = "51.0.0" }
datafusion-physical-expr = { version = "51.0.0" }
datafusion-spark = { version = "51.0.0" }
datafusion-functions-json = { git = "https://github.com/lakehq/datafusion-functions-json.git", rev = "f768013" }
```

🔸 DataFusion 是 Sail 底層查詢引擎
🔸 datafusion-spark: Spark 相容性支援
🔸 JSON 函數使用 LakeSail fork 版本，有客製化修改

### 🔸 Python 互操作

```toml
pyo3 = { version = "0.26.0", features = ["serde"] }
```

PyO3 讓 Rust 能嵌入 Python 直譯器，支援 Python UDF 執行

### 🔸 雲端儲存

```toml
object_store = { version = "0.12.4", features = ["aws", "gcp", "azure", "http"] }
hdfs-native-object-store = "0.15.0"
aws-config = "1.8.10"
aws-credential-types = "1.2.9"
```

🔸 object_store: 統一的雲端儲存抽象層 (S3, GCS, Azure Blob)
🔸 hdfs-native: HDFS 原生支援

### 🔸 Lakehouse 格式

```toml
delta_kernel = { version = "0.17.0", features = ["arrow-57", "default-engine-rustls", "internal-api"] }
```

Delta Lake 支援，使用 delta-kernel-rs 實作

### 🔸 Kubernetes 整合

```toml
kube = "2.0.1"
k8s-openapi = { version = "0.26.0", features = ["latest"] }
```

用於 Kubernetes 叢集模式部署與 worker 管理

### 🔸 SQL 解析

```toml
chumsky = { version = "0.11.2", default-features = false, features = ["pratt"] }
```

Parser combinator 函式庫，sail-sql-parser 用它實作 Spark SQL 解析器。pratt feature 提供運算子優先順序解析。

### 🔸 加密與雜湊

```toml
aes = "0.8.4"
aes-gcm = "0.10.3"
cbc = { version = "0.1.2", features = ["std"] }
base64 = "0.22.1"
md-5 = "0.10.6"
sha1 = "0.10.6"
crc32fast = "1.5.0"
twox-hash = "2.1.2"
murmur3 = "0.5.2"
rustls = "0.23.35"
```

🔸 AES/CBC: 資料加密
🔸 MD5/SHA1/CRC32: 雜湊與校驗
🔸 murmur3: Spark 相容的 hash partitioning

### 🔸 可觀測性

```toml
log = "0.4.28"
env_logger = "0.11.8"
fastrace = { version = "0.7.14", features = ["enable"] }
fastrace-opentelemetry = "0.14.0"
opentelemetry = "0.31.0"
opentelemetry_sdk = "0.31.0"
opentelemetry-otlp = { version = "0.31.0", features = ["tls", "tls-roots", "grpc-tonic"] }
```

🔸 fastrace: 高效能分散式追蹤
🔸 OpenTelemetry: 標準可觀測性協議，可輸出到 Jaeger/Zipkin 等

### 🔸 快取

```toml
moka = { version = "0.12.11", features = ["sync"] }
dashmap = "6.1.0"
```

🔸 moka: 高效能 concurrent cache，用於 metadata/statistics 快取
🔸 dashmap: 並發 HashMap

### 🔸 記憶體配置

```toml
mimalloc = { version = "0.1.48", default-features = false }
```

高效能記憶體分配器，替代系統預設 malloc

### 🔸 測試

```toml
wiremock = "0.6.5"
testcontainers = "0.25.2"
tempfile = "3.23.0"
```

🔸 wiremock: HTTP mock 測試
🔸 testcontainers: Docker 容器化整合測試

## Patch 區塊

```toml
[patch.crates-io]
# Override dependencies to use our forked versions.
# You can use `path = "..."` to temporarily point to your local copy of the crates to speed up local development.
```

用於覆蓋 crates.io 上的套件，指向 fork 版本或本地開發路徑

## Release Profile 最佳化

```toml
[profile.release]
opt-level = 3
debug = false
strip = true
debug-assertions = false
overflow-checks = false
lto = true
panic = 'unwind'
incremental = false
codegen-units = 1
```

### 🔸 opt-level = 3

編譯器最佳化等級，範圍 0-3:

```
opt-level = 0  → 無最佳化，編譯最快，執行最慢 (debug 預設)
opt-level = 1  → 基本最佳化
opt-level = 2  → 大部分最佳化 (release 預設)
opt-level = 3  → 全部最佳化，包含 loop unrolling、向量化
```

等級 3 會嘗試更激進的最佳化如:
- Loop unrolling: 展開迴圈減少分支
- Auto-vectorization: 使用 SIMD 指令
- Inline 更多函數

### 🔸 debug = false

是否在 binary 中包含 debug 資訊 (DWARF):

```
debug = true   → 包含完整 debug 資訊，可用 gdb/lldb 除錯
debug = false  → 不包含，binary 更小
```

### 🔸 strip = true

移除 binary 中的符號表:

```
strip = false  → 保留符號，crash 時有函數名稱
strip = true   → 移除符號，binary 更小

# 實際效果範例:
# strip = false: sail binary 約 150MB
# strip = true:  sail binary 約 50MB
```

### 🔸 debug-assertions = false

是否啟用 debug_assert!() 巨集:

```rust
debug_assert!(x > 0);  // 只在 debug-assertions = true 時執行

// debug-assertions = true:  會檢查，失敗會 panic
// debug-assertions = false: 完全移除，零成本
```

### 🔸 overflow-checks = false

整數溢位檢查:

```rust
let x: u8 = 255;
let y = x + 1;

// overflow-checks = true:  panic! (255 + 1 溢位)
// overflow-checks = false: y = 0 (wrapping，靜默溢位)
```

Release 關閉是因為檢查有效能成本，且程式邏輯應該自己處理溢位

### 🔸 lto = true

Link Time Optimization (連結時最佳化):

```
編譯流程:
  source.rs → LLVM IR → object file → linker → binary
                                        ↑
                                   LTO 在這裡

lto = false:
  每個 crate 獨立最佳化，linker 只是連結

lto = true:
  linker 看到所有 crate 的 LLVM IR
  可以跨 crate 做 inline、死碼消除等
```

好處: 跨 crate 最佳化，更小更快的 binary
壞處: 編譯時間大幅增加 (可能 2-5 倍)

### 🔸 panic = 'unwind'

Panic 處理策略:

```
panic = 'unwind':
  panic 時展開 stack，執行 Drop
  可被 catch_unwind 捕捉
  binary 較大 (包含展開資訊)

panic = 'abort':
  panic 時直接終止程序
  不執行 Drop，不能捕捉
  binary 較小
```

Sail 用 unwind 因為需要優雅處理錯誤和清理資源

### 🔸 incremental = false

增量編譯:

```
incremental = true:
  只重編修改過的部分
  編譯快，但產出可能不是最佳化

incremental = false:
  每次完整重編
  編譯慢，但產出完全最佳化
```

Release build 關閉確保最佳品質

### 🔸 codegen-units = 1

平行編譯單元數:

```
codegen-units = 16 (預設):
  編譯器將 crate 分成 16 份平行編譯
  編譯快，但跨單元最佳化受限

codegen-units = 1:
  整個 crate 作為一個單元編譯
  編譯慢，但最佳化效果最好
```

設為 1 配合 LTO 達到最大最佳化

### 🔸 Release Profile 總結

```
                    編譯時間    執行速度    Binary 大小
預設 release:         快         快          中
Sail release:        很慢       最快         小

# Sail 的設定適合 CI/CD 產出正式版本
# 本地開發用 cargo build (debug) 即可
```

## Coverage Profile

```toml
[profile.coverage]
inherits = "dev"
incremental = false
```

🔸 inherits = "dev": 繼承 dev profile 的所有設定
🔸 incremental = false: 關閉增量編譯

為什麼覆蓋率測試要關閉增量編譯:

```
增量編譯會快取未修改的程式碼
快取的部分不會重新插入覆蓋率計數器
導致覆蓋率數據不完整或不準確

關閉後每次都完整重編，確保所有程式碼都有計數器
```

## 版本相依性說明

```toml
# The `tonic` version must match the one used in `arrow-flight`
# The `prost` version must match the one used in `tonic`
# The `axum` version must match the one used in `tonic`
# The `pyo3` version must match the one used in `arrow-pyarrow`
# The `object_store` version must match the one used in DataFusion
```

### 🔸 為什麼版本必須匹配

這些套件之間有 ABI (Application Binary Interface) 相依性:

```
arrow-flight 使用 tonic 的型別:
  arrow_flight::FlightServiceServer<T: tonic::...>

如果版本不同:
  Sail 用 tonic 0.14
  arrow-flight 用 tonic 0.13
  → 編譯錯誤: 型別不匹配
```

### 🔸 相依鏈示意

```
┌─────────────────────────────────────────────────────────┐
│                        Sail                              │
└─────────────────────────────────────────────────────────┘
        │              │              │              │
        ▼              ▼              ▼              ▼
   ┌─────────┐   ┌──────────┐   ┌─────────┐   ┌───────────┐
   │  tonic  │   │  arrow-  │   │  pyo3   │   │ datafusion│
   │  0.14   │   │  flight  │   │  0.26   │   │   51.0    │
   └────┬────┘   └────┬─────┘   └────┬────┘   └─────┬─────┘
        │             │              │              │
        │        需要匹配         需要匹配       需要匹配
        │             │              │              │
        ▼             ▼              ▼              ▼
   ┌─────────┐   ┌─────────┐   ┌──────────┐   ┌───────────┐
   │  prost  │   │  tonic  │   │  arrow-  │   │  object_  │
   │  0.14   │   │  0.14   │   │  pyarrow │   │  store    │
   └─────────┘   └─────────┘   └──────────┘   └───────────┘
```

### 🔸 升級流程範例

假設要升級 Arrow 到 58.0.0:

```bash
# 1. 查看 arrow-flight 58 用的 tonic 版本
# https://github.com/apache/arrow-rs/blob/58.0.0/arrow-flight/Cargo.toml
# 假設是 tonic 0.15

# 2. 查看 tonic 0.15 用的 prost 和 axum 版本
# https://github.com/hyperium/tonic/blob/v0.15/tonic/Cargo.toml

# 3. 查看 arrow-pyarrow 58 用的 pyo3 版本
# https://github.com/apache/arrow-rs/blob/58.0.0/arrow-pyarrow/Cargo.toml

# 4. 查看 DataFusion 對應版本用的 object_store
# https://github.com/apache/datafusion/blob/...

# 5. 一次性更新所有相關版本
```

### 🔸 版本不匹配的錯誤訊息

```
error[E0308]: mismatched types
  --> src/server.rs:42:5
   |
42 |     tonic::transport::Server::builder()
   |     ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^ expected struct
   |     `tonic_0_13::transport::Server`, found struct
   |     `tonic_0_14::transport::Server`
```

看到這種錯誤就是版本不匹配，需要對齊版本
