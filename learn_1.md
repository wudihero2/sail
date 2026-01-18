# 一個 SQL 在 Sail 中的完整生命週期

這篇文章將追蹤一個簡單的 SQL 查詢（例如 `SELECT 1 + 1`）從 PySpark 客戶端發出，經過 Sail Spark Connect 服務器處理，最終返回結果的完整調用鏈。我們會深入到源碼層級，讓你理解整個 Sail 架構。

## 範例 SQL

```python
# PySpark Client
from pyspark.sql import SparkSession

spark = SparkSession.builder \
    .remote("sc://localhost:50051") \
    .getOrCreate()

result = spark.sql("SELECT 1 + 1 AS sum").collect()
print(result)  # [Row(sum=2)]
```

---

# 前置知識：gRPC 基礎惡補

在深入 Sail 架構之前，我們需要先理解 gRPC，因為整個 Spark Connect 協議都是基於 gRPC 構建的。

## 什麼是 gRPC？

🔸 **定義**

gRPC = **g**RPC **R**emote **P**rocedure **C**all

- 由 Google 開發的高性能、開源 RPC 框架
- 讓你可以像調用本地函數一樣調用遠程服務器上的函數
- 基於 HTTP/2 協議（相比 HTTP/1.1 有巨大性能提升）

🔸 **gRPC vs REST API**

| 特性 | gRPC | REST API |
|------|------|----------|
| 協議 | HTTP/2（二進制） | HTTP/1.1（文本） |
| 數據格式 | Protocol Buffers（二進制） | JSON（文本） |
| 性能 | 快（二進制序列化） | 慢（JSON 解析） |
| 串流支援 | 原生支援雙向流 | 需要 WebSocket 或 SSE |
| 瀏覽器支援 | 需要 gRPC-Web | 原生支援 |
| 類型安全 | 強類型（.proto 文件） | 弱類型（需要文檔） |
| 代碼生成 | 自動生成客戶端/服務器代碼 | 需要手動編寫 |

---

## gRPC 核心概念

### 1. Protocol Buffers（.proto 文件）

這是 gRPC 的 IDL（Interface Definition Language），用來定義服務和消息結構。

🔸 **範例：Spark Connect 的 ExecutePlanRequest**

檔案位置：`crates/sail-spark-connect/proto/spark/connect/base.proto`

```protobuf
// 定義消息結構
message ExecutePlanRequest {
  string session_id = 1;              // 字段編號 1
  UserContext user_context = 2;       // 嵌套消息
  Plan plan = 3;                      // 查詢計畫
  repeated string tags = 5;           // repeated = 數組
  optional string operation_id = 6;   // optional = 可選
}

message UserContext {
  string user_id = 1;
}

message Plan {
  oneof op_type {                     // oneof = 只能有一個
    Relation root = 1;
    Command command = 2;
  }
}
```

🔸 **為什麼用 Protocol Buffers？**

1. **高效序列化**：二進制格式比 JSON 小 3-10 倍
2. **強類型**：編譯時檢查，不會發送錯誤的數據類型
3. **向後兼容**：新增字段不會破壞舊客戶端
4. **跨語言**：可以生成 Python、Rust、Java 等多種語言的代碼

---

### 2. Service 定義（RPC 方法）

🔸 **範例：Spark Connect Service**

檔案位置：`crates/sail-spark-connect/proto/spark/connect/base.proto`

```protobuf
service SparkConnectService {
  // 執行查詢計畫（返回流）
  rpc ExecutePlan(ExecutePlanRequest) returns (stream ExecutePlanResponse);

  // 分析計畫（單次請求-響應）
  rpc AnalyzePlan(AnalyzePlanRequest) returns (AnalyzePlanResponse);

  // 配置管理（單次請求-響應）
  rpc Config(ConfigRequest) returns (ConfigResponse);

  // 添加資源（客戶端流）
  rpc AddArtifacts(stream AddArtifactsRequest) returns (AddArtifactsResponse);
}
```

🔸 **四種 RPC 類型**

**Unary RPC（一元 RPC）**
```
Client  ----[Request]---->  Server
Client  <---[Response]----  Server

範例：Config()
```

**Server Streaming RPC（服務器流式 RPC）**
```
Client  ----[Request]---->  Server
Client  <---[Response1]---  Server
Client  <---[Response2]---  Server
Client  <---[Response3]---  Server
Client  <---[End]----------  Server

範例：ExecutePlan() - 查詢結果可能很大，分批返回
```

**Client Streaming RPC（客戶端流式 RPC）**
```
Client  ----[Request1]---->  Server
Client  ----[Request2]---->  Server
Client  ----[Request3]---->  Server
Client  ----[End]---------->  Server
Client  <---[Response]----   Server

範例：AddArtifacts() - 上傳大文件，分塊發送
```

**Bidirectional Streaming RPC（雙向流式 RPC）**
```
Client  ----[Request1]---->  Server
Client  <---[Response1]---  Server
Client  ----[Request2]---->  Server
Client  <---[Response2]---  Server
Client  ----[End]---------->  Server
Client  <---[End]----------  Server

範例：ReattachExecute() - 斷線重連，持續交互
```

---

### 3. HTTP/2 的關鍵特性

gRPC 基於 HTTP/2，享受其所有優勢：

🔸 **多路復用（Multiplexing）**

```
HTTP/1.1（每個請求需要一個 TCP 連接）:
Connection 1: [Request A] --> [Response A]
Connection 2: [Request B] --> [Response B]
Connection 3: [Request C] --> [Response C]

HTTP/2（單一 TCP 連接，多個 stream）:
Connection:
  Stream 1: [Request A] --> [Response A]
  Stream 2: [Request B] --> [Response B]
  Stream 3: [Request C] --> [Response C]
```

好處：
- 減少 TCP 連接數（節省資源）
- 避免隊頭阻塞（Head-of-Line Blocking）
- 更低的延遲

🔸 **頭部壓縮（Header Compression）**

HTTP/1.1 每個請求都發送完整的文本頭部：
```
POST /execute HTTP/1.1
Host: localhost:50051
Content-Type: application/json
Authorization: Bearer token123...
User-Agent: PySpark/3.5.0
...
(約 500-1000 bytes)
```

HTTP/2 使用 HPACK 壓縮，重複的頭部只發送一次：
```
[Stream 1] Full Headers (500 bytes)
[Stream 2] :path: /analyze  (只發送變化的部分，20 bytes)
[Stream 3] :path: /config   (20 bytes)
```

🔸 **服務器推送（Server Push）**

雖然 gRPC 不常用這個特性，但 HTTP/2 支援服務器主動推送資源。

🔸 **流量控制（Flow Control）**

防止服務器發送過多數據淹沒客戶端：
```
Client: "我只能緩衝 64KB 數據"
Server: [發送 64KB]
Server: [等待 Client 確認]
Client: "我處理完了，可以再發 64KB"
Server: [繼續發送]
```

---

## gRPC 在 Sail 中的使用

### Sail 使用 Tonic 框架

🔸 **Tonic = Rust 的 gRPC 框架**

檔案位置：`Cargo.toml`

```toml
[dependencies]
tonic = "0.12"           # gRPC 核心
prost = "0.13"           # Protocol Buffers 序列化
tonic-build = "0.12"     # 編譯時從 .proto 生成 Rust 代碼
```

🔸 **代碼生成流程**

```
1. 編寫 .proto 文件
   ↓
2. Cargo build 時，tonic-build 自動生成 Rust 代碼
   ↓
3. 生成的代碼包含：
   - Message structs (ExecutePlanRequest, ExecutePlanResponse)
   - Service traits (SparkConnectService)
   - Client stubs (SparkConnectServiceClient)
   - Server builders (SparkConnectServiceServer)
```

🔸 **自動生成的代碼範例**

檔案位置：`target/debug/build/sail-spark-connect-xxx/out/spark.connect.rs`

```rust
// 自動生成的消息結構
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct ExecutePlanRequest {
    #[prost(string, tag = "1")]
    pub session_id: ::prost::alloc::string::String,
    #[prost(message, optional, tag = "2")]
    pub user_context: ::core::option::Option<UserContext>,
    #[prost(message, optional, tag = "3")]
    pub plan: ::core::option::Option<Plan>,
}

// 自動生成的 Service trait
#[async_trait]
pub trait SparkConnectService: Send + Sync + 'static {
    type ExecutePlanStream: futures::Stream<Item = Result<ExecutePlanResponse, Status>>
        + Send
        + 'static;

    async fn execute_plan(
        &self,
        request: tonic::Request<ExecutePlanRequest>,
    ) -> Result<tonic::Response<Self::ExecutePlanStream>, tonic::Status>;

    async fn config(
        &self,
        request: tonic::Request<ConfigRequest>,
    ) -> Result<tonic::Response<ConfigResponse>, tonic::Status>;

    // ... 其他方法
}
```

---

## gRPC 開發的三步驟與源碼對應

這一節詳細說明 gRPC 開發的完整流程，以及在 Sail 中每一步的實際代碼位置。

### 🔸 步驟 1: 編寫 .proto 文件（協議定義）

#### 1.1 Service 定義

檔案位置：`crates/sail-spark-connect/proto/spark/connect/base.proto:1092-1135`

```protobuf
// Main interface for the SparkConnect service.
service SparkConnectService {

  // Executes a request that contains the query and returns a stream of Response.
  // It is guaranteed that there is at least one ARROW batch returned even if the result set is empty.
  rpc ExecutePlan(ExecutePlanRequest) returns (stream ExecutePlanResponse) {}

  // Analyzes a query and returns a AnalyzeResponse containing metadata about the query.
  rpc AnalyzePlan(AnalyzePlanRequest) returns (AnalyzePlanResponse) {}

  // Update or fetch the configurations and returns a ConfigResponse containing the result.
  rpc Config(ConfigRequest) returns (ConfigResponse) {}

  // Add artifacts to the session and returns a AddArtifactsResponse containing metadata about
  // the added artifacts.
  rpc AddArtifacts(stream AddArtifactsRequest) returns (AddArtifactsResponse) {}

  // Check statuses of artifacts in the session and returns them in a ArtifactStatusesResponse
  rpc ArtifactStatus(ArtifactStatusesRequest) returns (ArtifactStatusesResponse) {}

  // Interrupts running executions
  rpc Interrupt(InterruptRequest) returns (InterruptResponse) {}

  // Reattach to an existing reattachable execution.
  // The ExecutePlan must have been started with ReattachOptions.reattachable=true.
  // If the ExecutePlanResponse stream ends without a ResultComplete message, there is more to
  // continue. If there is a ResultComplete, the client should use ReleaseExecute with
  rpc ReattachExecute(ReattachExecuteRequest) returns (stream ExecutePlanResponse) {}

  // Release an reattachable execution, or parts thereof.
  // The ExecutePlan must have been started with ReattachOptions.reattachable=true.
  // Non reattachable executions are released automatically and immediately after the ExecutePlan
  // RPC and ReleaseExecute may not be used.
  rpc ReleaseExecute(ReleaseExecuteRequest) returns (ReleaseExecuteResponse) {}

  // Release a session.
  // All the executions in the session will be released. Any further requests for the session with
  // that session_id for the given user_id will fail. If the session didn't exist or was already
  // released, this is a noop.
  rpc ReleaseSession(ReleaseSessionRequest) returns (ReleaseSessionResponse) {}

  // FetchErrorDetails retrieves the matched exception with details based on a provided error id.
  rpc FetchErrorDetails(FetchErrorDetailsRequest) returns (FetchErrorDetailsResponse) {}
}
```

🔸 **關鍵觀察**

這個 service 定義包含：
- **10 個 RPC 方法**（ExecutePlan, AnalyzePlan, Config, AddArtifacts, ArtifactStatus, Interrupt, ReattachExecute, ReleaseExecute, ReleaseSession, FetchErrorDetails）
- **兩種流模式**：`stream` 關鍵字表示流式傳輸
  - `returns (stream ExecutePlanResponse)` = Server Streaming（服務器流式返回多個響應）
  - `rpc AddArtifacts(stream AddArtifactsRequest)` = Client Streaming（客戶端流式發送多個請求）

#### 1.2 Message 定義範例

同樣在 `base.proto` 中定義的消息結構：

```protobuf
message ExecutePlanRequest {
  // (Required) The session_id specifies a spark session for a user id
  string session_id = 1;

  // (Optional) User context
  UserContext user_context = 2;

  // (Required) The logical plan to be executed / analyzed.
  Plan plan = 3;

  // (Optional) Unique ID for the operation
  optional string operation_id = 6;

  // (Optional) Tags to attach to the query
  repeated string tags = 7;

  // (Optional) Request options
  repeated RequestOption request_options = 8;
}

message Plan {
  oneof op_type {
    Relation root = 1;      // DataFrame API 操作
    Command command = 2;    // SQL 命令或其他命令
  }
}
```

🔸 **Protobuf 語法要點**

- `= 1, = 2, = 3` 是字段編號（用於二進制序列化，不能改變）
- `optional` = 可選字段（Protobuf 3 新增）
- `repeated` = 數組/列表
- `oneof` = 聯合類型（只能有一個字段被設置）

---

### 🔸 步驟 2: Cargo build 時自動生成 Rust 代碼

#### 2.1 配置代碼生成

檔案位置：`crates/sail-spark-connect/build.rs:7-42`

```rust
fn build_proto() -> Result<(), Box<dyn std::error::Error>> {
    // 1. 設置輸出目錄（target/debug/build/sail-spark-connect-<hash>/out/）
    let out_dir = PathBuf::from(std::env::var("OUT_DIR")?);
    let descriptor_path = out_dir.join("spark_connect_descriptor.bin");

    // 2. 配置 prost（Protocol Buffers Rust 實作）
    let mut config = Config::new();
    config.skip_debug([
        "spark.connect.LocalRelation",
        "spark.connect.ExecutePlanResponse.ArrowBatch",
    ]);

    // 3. 使用 tonic-build 編譯 proto 文件
    tonic_prost_build::configure()
        .protoc_arg("--experimental_allow_proto3_optional")  // 支援 optional 關鍵字
        .file_descriptor_set_path(&descriptor_path)          // 生成 descriptor（用於反射）
        .compile_well_known_types(true)                      // 包含 google.protobuf.* 類型
        .extern_path(".google.protobuf", "::pbjson_types")   // 使用外部 JSON 類型
        .build_server(true)                                   // 🔥 生成 Server 端代碼
        .compile_with_config(
            config,
            &[
                "proto/spark/connect/base.proto",
                "proto/spark/connect/catalog.proto",
                "proto/spark/connect/commands.proto",
                "proto/spark/connect/common.proto",
                "proto/spark/connect/example_plugins.proto",
                "proto/spark/connect/expressions.proto",
                "proto/spark/connect/ml.proto",
                "proto/spark/connect/ml_common.proto",
                "proto/spark/connect/relations.proto",
                "proto/spark/connect/types.proto",
            ],
            &["proto"],
        )?;

    // 4. 生成 JSON 序列化支援（用於日誌和調試）
    let descriptors = std::fs::read(descriptor_path)?;
    pbjson_build::Builder::new()
        .register_descriptors(&descriptors)?
        .build(&[".spark.connect"])?;

    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=build.rs");
    build_proto()?;       // 生成 protobuf 代碼
    build_spark_config()?; // 生成 Spark 配置常量
    Ok(())
}
```

🔸 **Rust build.rs 知識**

- `build.rs` 在 `cargo build` **之前**執行（編譯時代碼生成）
- `OUT_DIR` 環境變量指向 `target/debug/build/<crate-name>-<hash>/out/`
- `tonic-build` 調用 `protoc` 編譯器解析 .proto 文件，生成 Rust 代碼

#### 2.2 生成的文件

執行 `cargo build` 後，生成：

```bash
target/debug/build/sail-spark-connect-<hash>/out/
├── spark.connect.rs            # Message structs + Service trait + Client + Server
├── spark.connect.serde.rs      # JSON 序列化實作（Serde traits）
├── spark_connect_descriptor.bin # Protobuf descriptor（二進制，用於反射）
└── spark_config.rs             # Spark 配置常量（從 JSON 生成）
```

查看生成的代碼：

```bash
$ ls -lh target/debug/build/sail-spark-connect-*/out/
-rw-r--r--  353K spark_config.rs
-rw-r--r--  237K spark_connect_descriptor.bin
-rw-r--r--  300K spark.connect.rs              # 🔥 主要生成文件
-rw-r--r--  1.7M spark.connect.serde.rs
```

#### 2.3 Include 生成的代碼

檔案位置：`crates/sail-spark-connect/src/lib.rs:18-32`

```rust
pub mod spark {
    #[allow(clippy::all)]  // 生成的代碼可能不符合 Clippy 規範
    pub mod connect {
        // 🔥 Include 編譯時生成的 Rust 代碼
        // 這個宏展開為：include!(concat!(env!("OUT_DIR"), "/spark.connect.rs"));
        tonic::include_proto!("spark.connect");

        // Include JSON 序列化代碼
        tonic::include_proto!("spark.connect.serde");

        // 暴露 protobuf descriptor（供反射使用）
        pub const FILE_DESCRIPTOR_SET: &[u8] =
            tonic::include_file_descriptor_set!("spark_connect_descriptor");
    }

    #[allow(clippy::doc_markdown)]
    pub mod config {
        // Include Spark 配置常量
        include!(concat!(env!("OUT_DIR"), "/spark_config.rs"));
    }
}
```

🔸 **Rust 宏知識**

- `include!()` 宏會在編譯時將文件內容**原地展開**（就像 C 的 `#include`）
- `env!("OUT_DIR")` 在編譯時獲取環境變量
- `concat!()` 宏拼接字串

---

### 🔸 步驟 3: 生成的代碼包含三大部分

#### 3.1 Message Structs（消息結構體）

生成位置：`target/debug/build/sail-spark-connect-*/out/spark.connect.rs`

```rust
// 自動生成的 ExecutePlanRequest struct
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct ExecutePlanRequest {
    /// (Required) The session_id
    #[prost(string, tag = "1")]
    pub session_id: ::prost::alloc::string::String,

    /// (Optional) User context
    #[prost(message, optional, tag = "2")]
    pub user_context: ::core::option::Option<UserContext>,

    /// (Required) The logical plan
    #[prost(message, optional, tag = "3")]
    pub plan: ::core::option::Option<Plan>,

    /// (Optional) Unique ID for the operation
    #[prost(string, optional, tag = "6")]
    pub operation_id: ::core::option::Option<::prost::alloc::string::String>,

    /// (Optional) Tags to attach to the query
    #[prost(string, repeated, tag = "7")]
    pub tags: ::prost::alloc::vec::Vec<::prost::alloc::string::String>,

    /// (Optional) Request options
    #[prost(message, repeated, tag = "8")]
    pub request_options: ::prost::alloc::vec::Vec<execute_plan_request::RequestOption>,
}

// 自動生成的 ExecutePlanResponse struct
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct ExecutePlanResponse {
    /// The session_id
    #[prost(string, tag = "1")]
    pub session_id: ::prost::alloc::string::String,

    /// The operation_id
    #[prost(string, tag = "3")]
    pub operation_id: ::prost::alloc::string::String,

    /// The response_id (unique for each response in the stream)
    #[prost(string, tag = "4")]
    pub response_id: ::prost::alloc::string::String,

    /// Response content (oneof = 只能有一個)
    #[prost(oneof = "execute_plan_response::ResponseType", tags = "2, 5, 7, ...")]
    pub response_type: ::core::option::Option<execute_plan_response::ResponseType>,
}

// 嵌套的 enum（對應 protobuf 的 oneof）
pub mod execute_plan_response {
    #[derive(Clone, PartialEq, ::prost::Oneof)]
    pub enum ResponseType {
        #[prost(message, tag = "2")]
        ArrowBatch(super::ArrowBatch),

        #[prost(message, tag = "5")]
        SqlCommandResult(Box<super::SqlCommandResult>),

        #[prost(message, tag = "7")]
        ResultComplete(super::ResultComplete),

        // ... 其他類型
    }
}
```

🔸 **Prost 屬性宏說明**

- `#[prost(string, tag = "1")]` 表示字段 1 是字串類型
- `#[prost(message, optional, tag = "2")]` 表示字段 2 是可選的嵌套消息
- `#[prost(string, repeated, tag = "7")]` 表示字段 7 是字串數組
- `#[prost(oneof = "...")]` 表示這是聯合類型字段

#### 3.2 Service Trait（Server 端要實作的 trait）

生成位置：`target/debug/build/sail-spark-connect-*/out/spark.connect.rs:6077+`

```rust
/// Generated trait containing gRPC methods that should be implemented
/// for use with SparkConnectServiceServer.
#[async_trait]
pub trait SparkConnectService: Send + Sync + 'static {

    /// Server streaming response type for the ExecutePlan method.
    type ExecutePlanStream: tonic::codegen::tokio_stream::Stream<
            Item = std::result::Result<super::ExecutePlanResponse, tonic::Status>,
        >
        + Send
        + 'static;

    /// Executes a request that contains the query and returns a stream of Response.
    /// It is guaranteed that there is at least one ARROW batch returned even if the result set is empty.
    async fn execute_plan(
        &self,
        request: tonic::Request<super::ExecutePlanRequest>,
    ) -> std::result::Result<
        tonic::Response<Self::ExecutePlanStream>,
        tonic::Status,
    >;

    /// Analyzes a query and returns a AnalyzeResponse containing metadata about the query.
    async fn analyze_plan(
        &self,
        request: tonic::Request<super::AnalyzePlanRequest>,
    ) -> std::result::Result<
        tonic::Response<super::AnalyzePlanResponse>,
        tonic::Status,
    >;

    /// Update or fetch the configurations and returns a ConfigResponse containing the result.
    async fn config(
        &self,
        request: tonic::Request<super::ConfigRequest>,
    ) -> std::result::Result<tonic::Response<super::ConfigResponse>, tonic::Status>;

    /// Add artifacts to the session
    async fn add_artifacts(
        &self,
        request: tonic::Request<tonic::Streaming<super::AddArtifactsRequest>>,
    ) -> std::result::Result<
        tonic::Response<super::AddArtifactsResponse>,
        tonic::Status,
    >;

    /// Check statuses of artifacts
    async fn artifact_status(
        &self,
        request: tonic::Request<super::ArtifactStatusesRequest>,
    ) -> std::result::Result<
        tonic::Response<super::ArtifactStatusesResponse>,
        tonic::Status,
    >;

    /// Interrupts running executions
    async fn interrupt(
        &self,
        request: tonic::Request<super::InterruptRequest>,
    ) -> std::result::Result<
        tonic::Response<super::InterruptResponse>,
        tonic::Status,
    >;

    /// Server streaming response type for the ReattachExecute method.
    type ReattachExecuteStream: tonic::codegen::tokio_stream::Stream<
            Item = std::result::Result<super::ExecutePlanResponse, tonic::Status>,
        >
        + Send
        + 'static;

    /// Reattach to an existing reattachable execution.
    async fn reattach_execute(
        &self,
        request: tonic::Request<super::ReattachExecuteRequest>,
    ) -> std::result::Result<
        tonic::Response<Self::ReattachExecuteStream>,
        tonic::Status,
    >;

    /// Release an reattachable execution
    async fn release_execute(
        &self,
        request: tonic::Request<super::ReleaseExecuteRequest>,
    ) -> std::result::Result<
        tonic::Response<super::ReleaseExecuteResponse>,
        tonic::Status,
    >;

    /// Release a session
    async fn release_session(
        &self,
        request: tonic::Request<super::ReleaseSessionRequest>,
    ) -> std::result::Result<
        tonic::Response<super::ReleaseSessionResponse>,
        tonic::Status,
    >;

    /// Fetch error details
    async fn fetch_error_details(
        &self,
        request: tonic::Request<super::FetchErrorDetailsRequest>,
    ) -> std::result::Result<
        tonic::Response<super::FetchErrorDetailsResponse>,
        tonic::Status,
    >;
}
```

🔸 **關鍵型別**

- `tonic::Request<T>` 包含請求消息 + metadata（headers）
- `tonic::Response<T>` 包含響應消息 + metadata
- `tonic::Status` 表示 gRPC 錯誤（類似 HTTP 狀態碼）
- `tonic::Streaming<T>` 表示客戶端流式輸入
- `type ExecutePlanStream` 是關聯類型（Associated Type），實作者需要指定具體的流類型

#### 3.3 實作 Service Trait（Sail 的實作）

檔案位置：`crates/sail-spark-connect/src/server.rs:24-474`

```rust
// server.rs:24-32
#[derive(Debug)]
pub struct SparkConnectServer {
    session_manager: SessionManager,
}

impl SparkConnectServer {
    pub fn new(session_manager: SessionManager) -> Self {
        Self { session_manager }
    }
}

// server.rs:50-161
#[tonic::async_trait]  // 🔥 這個宏讓 trait 可以包含 async 方法
impl SparkConnectService for SparkConnectServer {
    // 指定流類型為我們自定義的 ExecutePlanResponseStream
    type ExecutePlanStream = ExecutePlanResponseStream;

    // 🔥 實作 execute_plan 方法
    async fn execute_plan(
        &self,
        request: Request<ExecutePlanRequest>,
    ) -> Result<Response<Self::ExecutePlanStream>, Status> {
        // 1. 取出請求內容
        let request = request.into_inner();
        debug!("{request:?}");

        // 2. 構建 SessionKey
        let session_key = SessionKey {
            user_id: request.user_context.map(|u| u.user_id).unwrap_or_default(),
            session_id: request.session_id,
        };

        // 3. 構建執行器元數據
        let metadata = ExecutorMetadata {
            operation_id: request.operation_id.unwrap_or_else(|| Uuid::new_v4().to_string()),
            tags: request.tags,
            reattachable: is_reattachable(&request.request_options),
        };

        // 4. 獲取或創建 SessionContext（這裡會觸發 Actor 通訊）
        let ctx = self.session_manager.get_or_create_session_context(session_key).await?;

        // 5. 解析 Plan（這是 protobuf oneof 字段）
        let Plan { op_type: op } = request.plan.required("plan")?;
        let op = op.required("plan op")?;

        // 6. 根據計畫類型分發到不同的處理函數
        let stream = match op {
            plan::OpType::Root(relation) => {
                // DataFrame API 調用
                service::handle_execute_relation(&ctx, relation, metadata).await?
            }
            plan::OpType::Command(Command { command_type: command }) => {
                // SQL 或命令調用
                let command = command.required("command")?;
                match command {
                    CommandType::SqlCommand(sql) => {
                        service::handle_execute_sql_command(&ctx, sql, metadata).await?
                    }
                    CommandType::WriteOperation(write) => {
                        service::handle_execute_write_operation(&ctx, write, metadata).await?
                    }
                    CommandType::CreateDataframeView(view) => {
                        service::handle_execute_create_dataframe_view(&ctx, view, metadata).await?
                    }
                    // ... 其他命令類型
                    _ => return Err(Status::unimplemented("command type not supported")),
                }
            }
        };

        // 7. 返回響應流
        Ok(Response::new(stream))
    }

    // 實作其他方法（analyze_plan, config, add_artifacts, ...）
    async fn analyze_plan(
        &self,
        request: Request<AnalyzePlanRequest>,
    ) -> Result<Response<AnalyzePlanResponse>, Status> {
        let request = request.into_inner();
        debug!("{request:?}");

        let session_key = SessionKey {
            user_id: request.user_context.map(|u| u.user_id).unwrap_or_default(),
            session_id: request.session_id.clone(),
        };

        let ctx = self.session_manager.get_or_create_session_context(session_key).await?;

        let analyze = request.analyze.required("analyze")?;
        let result = match analyze {
            Analyze::Schema(schema) => {
                let schema = service::handle_analyze_schema(&ctx, schema).await?;
                Some(analyze_plan_response::Result::Schema(schema))
            }
            Analyze::Explain(explain) => {
                let explain = service::handle_analyze_explain(&ctx, explain).await?;
                Some(analyze_plan_response::Result::Explain(explain))
            }
            // ... 其他分析類型
            _ => None,
        };

        let response = AnalyzePlanResponse {
            session_id: request.session_id.clone(),
            server_side_session_id: request.session_id,
            result,
        };

        debug!("{response:?}");
        Ok(Response::new(response))
    }

    async fn config(
        &self,
        request: Request<ConfigRequest>,
    ) -> Result<Response<ConfigResponse>, Status> {
        // ... 配置管理實作
    }

    // ... 實作所有其他 RPC 方法
}
```

🔸 **Rust 異步知識**

- `#[tonic::async_trait]` 宏處理 trait 中的 async 方法（Rust 目前不原生支援 trait 中的 async fn）
- `await?` 組合了兩個操作：
  - `await` 等待異步操作完成
  - `?` 如果結果是 `Err`，立即返回錯誤（early return）

#### 3.4 Server Builder（自動生成）

生成位置：`target/debug/build/sail-spark-connect-*/out/spark.connect.rs`

```rust
/// Generated server implementations.
pub mod spark_connect_service_server {
    use tonic::codegen::*;

    /// Server builder for SparkConnectService
    pub struct SparkConnectServiceServer<T> {
        inner: Arc<T>,
        accept_compression_encodings: EnabledCompressionEncodings,
        send_compression_encodings: EnabledCompressionEncodings,
        max_decoding_message_size: Option<usize>,
        max_encoding_message_size: Option<usize>,
    }

    impl<T: SparkConnectService> SparkConnectServiceServer<T> {
        pub fn new(inner: T) -> Self {
            Self::from_arc(Arc::new(inner))
        }

        pub fn from_arc(inner: Arc<T>) -> Self {
            Self {
                inner,
                accept_compression_encodings: Default::default(),
                send_compression_encodings: Default::default(),
                max_decoding_message_size: None,
                max_encoding_message_size: None,
            }
        }

        /// Enable gzip compression
        pub fn accept_compressed(mut self, encoding: CompressionEncoding) -> Self {
            self.accept_compression_encodings.enable(encoding);
            self
        }

        /// Enable gzip compression for responses
        pub fn send_compressed(mut self, encoding: CompressionEncoding) -> Self {
            self.send_compression_encodings.enable(encoding);
            self
        }

        /// Set max message size for decoding
        pub fn max_decoding_message_size(mut self, limit: usize) -> Self {
            self.max_decoding_message_size = Some(limit);
            self
        }

        /// Set max message size for encoding
        pub fn max_encoding_message_size(mut self, limit: usize) -> Self {
            self.max_encoding_message_size = Some(limit);
            self
        }
    }

    // 🔥 實作 tonic::codegen::Service trait（這是 Tonic 的核心 trait）
    impl<T: SparkConnectService> tonic::codegen::Service<http::Request<Body>>
        for SparkConnectServiceServer<T>
    {
        type Response = http::Response<tonic::body::BoxBody>;
        type Error = std::convert::Infallible;
        type Future = BoxFuture<Self::Response, Self::Error>;

        fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        // 🔥 根據 HTTP 請求路徑路由到對應的 RPC 方法
        fn call(&mut self, req: http::Request<Body>) -> Self::Future {
            let inner = self.inner.clone();
            match req.uri().path() {
                "/spark.connect.SparkConnectService/ExecutePlan" => {
                    // 反序列化請求 -> 調用 trait 方法 -> 序列化響應
                    // ... (生成的樣板代碼，處理序列化、壓縮、錯誤轉換等)
                }
                "/spark.connect.SparkConnectService/AnalyzePlan" => {
                    // ... 類似處理
                }
                "/spark.connect.SparkConnectService/Config" => {
                    // ... 類似處理
                }
                // ... 其他路徑
                _ => {
                    Box::pin(async move {
                        Ok(http::Response::builder()
                            .status(404)
                            .body(empty_body())
                            .unwrap())
                    })
                }
            }
        }
    }
}
```

🔸 **HTTP 路徑映射**

gRPC 使用 HTTP/2 作為傳輸層，每個 RPC 方法對應一個 HTTP 路徑：

```
POST /spark.connect.SparkConnectService/ExecutePlan
POST /spark.connect.SparkConnectService/AnalyzePlan
POST /spark.connect.SparkConnectService/Config
...
```

路徑格式：`/<package>.<service>/<method>`

#### 3.5 Client Stubs（自動生成，用於測試或客戶端）

生成位置：`target/debug/build/sail-spark-connect-*/out/spark.connect.rs:5692+`

```rust
/// Generated client implementations.
pub mod spark_connect_service_client {
    use tonic::codegen::*;

    /// Client for SparkConnectService
    pub struct SparkConnectServiceClient<T> {
        inner: tonic::client::Grpc<T>,
    }

    impl SparkConnectServiceClient<tonic::transport::Channel> {
        /// Attempt to create a new client by connecting to a given endpoint.
        pub async fn connect<D>(dst: D) -> Result<Self, tonic::transport::Error>
        where
            D: TryInto<tonic::transport::Endpoint>,
            D::Error: Into<StdError>,
        {
            let conn = tonic::transport::Endpoint::new(dst)?.connect().await?;
            Ok(Self::new(conn))
        }
    }

    impl<T> SparkConnectServiceClient<T>
    where
        T: tonic::client::GrpcService<tonic::body::BoxBody>,
        T::Error: Into<StdError>,
        T::ResponseBody: Body<Data = Bytes> + Send + 'static,
        <T::ResponseBody as Body>::Error: Into<StdError> + Send,
    {
        pub fn new(inner: T) -> Self {
            let inner = tonic::client::Grpc::new(inner);
            Self { inner }
        }

        /// Executes a request that contains the query and returns a stream of Response.
        pub async fn execute_plan(
            &mut self,
            request: impl tonic::IntoRequest<super::ExecutePlanRequest>,
        ) -> std::result::Result<
            tonic::Response<tonic::codec::Streaming<super::ExecutePlanResponse>>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;

            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/spark.connect.SparkConnectService/ExecutePlan",
            );

            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("spark.connect.SparkConnectService", "ExecutePlan"));

            self.inner.server_streaming(req, path, codec).await
        }

        /// Analyzes a query
        pub async fn analyze_plan(
            &mut self,
            request: impl tonic::IntoRequest<super::AnalyzePlanRequest>,
        ) -> std::result::Result<
            tonic::Response<super::AnalyzePlanResponse>,
            tonic::Status,
        > {
            // ... 類似實作（unary RPC）
        }

        /// Update or fetch configurations
        pub async fn config(
            &mut self,
            request: impl tonic::IntoRequest<super::ConfigRequest>,
        ) -> std::result::Result<tonic::Response<super::ConfigResponse>, tonic::Status> {
            // ... 類似實作
        }

        // ... 其他 RPC 方法
    }
}
```

🔸 **Client 使用範例（Rust 客戶端）**

```rust
use spark::connect::spark_connect_service_client::SparkConnectServiceClient;
use spark::connect::{ExecutePlanRequest, Plan};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 連接到服務器
    let mut client = SparkConnectServiceClient::connect("http://localhost:50051").await?;

    // 構建請求
    let request = ExecutePlanRequest {
        session_id: "test-session".to_string(),
        plan: Some(Plan { /* ... */ }),
        ..Default::default()
    };

    // 調用 RPC 方法
    let mut response_stream = client.execute_plan(request).await?.into_inner();

    // 處理響應流
    while let Some(response) = response_stream.message().await? {
        println!("Received: {:?}", response);
    }

    Ok(())
}
```

---

### 🔸 實際使用：啟動 Sail Server

檔案位置：`crates/sail-spark-connect/src/entrypoint.rs:13-36`

```rust
pub async fn serve(options: ServerOptions) -> Result<(), Box<dyn std::error::Error>> {
    // 1. 創建 SessionManager（管理所有用戶 session）
    let session_manager = SessionManager::new(SessionManagerOptions {
        config: options.config.clone(),
        runtime: options.runtime.clone(),
    });

    // 2. 創建我們的 SparkConnectServer（實作了 SparkConnectService trait）
    let service = SparkConnectServer::new(session_manager);

    // 3. 🔥 使用生成的 Server Builder 將我們的實作包裝成 gRPC service
    use crate::spark::connect::spark_connect_service_server::SparkConnectServiceServer;
    let service = SparkConnectServiceServer::new(service);

    // 4. 使用 Tonic 的 ServerBuilder 啟動 gRPC 服務器
    let builder = ServerBuilder::new(&options.server_options)?;
    builder
        .serve(service, options.shutdown_signal)
        .await
        .map_err(Into::into)
}
```

檔案位置：`crates/sail-server/src/builder.rs:101-124`

```rust
pub async fn serve<S>(
    self,
    service: S,
    shutdown_signal: ShutdownSignal,
) -> Result<(), Box<dyn std::error::Error>>
where
    S: Service<Request<Body>, Response = Response<BoxBody>, Error = Infallible>
        + NamedService
        + Clone
        + Send
        + 'static,
    S::Future: Send + 'static,
{
    let listener = TcpListener::bind(&self.addr).await?;
    let addr = listener.local_addr()?;
    info!("Sail Spark Connect server listening on {addr}");

    // 🔥 啟動 Tonic gRPC 服務器
    tonic::transport::Server::builder()
        .http2_keepalive_interval(self.options.http2_keepalive_interval)
        .http2_keepalive_timeout(self.options.http2_keepalive_timeout)
        .http2_adaptive_window(self.options.http2_adaptive_window)
        .tcp_nodelay(self.options.nodelay)
        .tcp_keepalive(self.options.keepalive)
        .add_service(service)  // 🔥 註冊我們的 gRPC service
        .serve_with_incoming_shutdown(
            TcpListenerStream::new(listener),
            shutdown_signal.create_future(),
        )
        .await?;

    Ok(())
}
```

---

## 總結：完整的代碼對應表

| 步驟 | 位置 | 說明 |
|------|------|------|
| **1. 定義 .proto** | `crates/sail-spark-connect/proto/spark/connect/base.proto:1092-1135` | 定義 `service SparkConnectService` 和所有 RPC 方法 |
| **1. 定義 Message** | `crates/sail-spark-connect/proto/spark/connect/base.proto` | 定義 `ExecutePlanRequest`, `ExecutePlanResponse` 等消息 |
| **2. 配置代碼生成** | `crates/sail-spark-connect/build.rs:7-42` | 使用 `tonic-build` 在編譯時生成代碼 |
| **2. 生成的代碼** | `target/debug/build/sail-spark-connect-*/out/spark.connect.rs` | 包含 Message structs、Service trait、Client、Server builder |
| **2. Include 代碼** | `crates/sail-spark-connect/src/lib.rs:18-32` | 使用 `tonic::include_proto!` 引入生成的代碼 |
| **3. 實作 Server Trait** | `crates/sail-spark-connect/src/server.rs:50-474` | 實作 `SparkConnectService` trait 的所有方法 |
| **3. 啟動 Server** | `crates/sail-spark-connect/src/entrypoint.rs:13-36` | 使用生成的 `SparkConnectServiceServer` 啟動服務 |

---

## gRPC 通訊流程圖

```
PySpark Client                                          Sail Server
     |                                                       |
     | 1. 創建 gRPC channel                                  |
     |    grpc.insecure_channel('localhost:50051')          |
     |------------------------------------------------       |
     |    建立 TCP 連接 (port 50051)                         |
     |    HTTP/2 SETTINGS 協商                               |
     |    <---------- TCP Handshake -----------------------> |
     |                                                       |
     | 2. 創建 gRPC stub                                     |
     |    stub = SparkConnectServiceStub(channel)           |
     |                                                       |
     | 3. 調用 RPC 方法                                      |
     |    response = stub.ExecutePlan(request)              |
     |                                                       |
     | 4. 序列化 Protobuf                                    |
     |    request_bytes = serialize(ExecutePlanRequest)     |
     |                                                       |
     | 5. 發送 HTTP/2 POST                                   |
     |    POST /spark.connect.SparkConnectService/ExecutePlan |
     |    Content-Type: application/grpc                    |
     |    Body: [protobuf bytes]                            |
     |-----------------------------------------------------> |
     |                                                       | 6. Tonic 接收請求
     |                                                       |    router.route("/ExecutePlan")
     |                                                       |    ↓
     |                                                       | 7. 反序列化 Protobuf
     |                                                       |    request = deserialize(bytes)
     |                                                       |    ↓
     |                                                       | 8. 調用實作方法
     |                                                       |    SparkConnectServer::execute_plan()
     |                                                       |    ↓
     |                                                       | 9. 處理邏輯
     |                                                       |    session_manager.get_or_create()
     |                                                       |    resolve_and_execute_plan()
     |                                                       |    ↓
     |                                                       | 10. 返回 Stream
     | 11. 接收第一個響應                                     |     ↓
     | <-----------------------------------------------------| 11. 序列化並發送
     |    HTTP/2 Response (stream ID 1)                     |     ExecutePlanResponse #1
     |    [protobuf bytes]                                  |
     |                                                       |
     | 12. 反序列化                                          |
     |     response1 = deserialize(bytes)                   |
     |                                                       |
     | 13. 接收更多響應                                       |
     | <-----------------------------------------------------|
     |    ExecutePlanResponse #2                            |
     | <-----------------------------------------------------|
     |    ExecutePlanResponse #3 (ResultComplete)           |
     |                                                       |
     | 14. Stream 結束                                       |
     |     for batch in response_stream: ...                |
```

---

## Sail 中的 gRPC 配置

🔸 **服務器配置**

檔案位置：`crates/sail-server/src/builder.rs:22-33`

```rust
pub struct ServerBuilderOptions {
    // 禁用 Nagle 算法（減少延遲，適合小包頻繁發送）
    pub nodelay: bool,  // true

    // TCP keepalive（60 秒發送一次探測包，防止連接被中間設備關閉）
    pub keepalive: Option<Duration>,  // 60s

    // HTTP/2 keepalive（60 秒發送 PING frame）
    pub http2_keepalive_interval: Option<Duration>,  // 60s

    // HTTP/2 keepalive 超時（10 秒未收到 PONG 則關閉連接）
    pub http2_keepalive_timeout: Option<Duration>,  // 10s

    // HTTP/2 自適應窗口（動態調整流量控制窗口大小）
    pub http2_adaptive_window: Option<bool>,  // true
}
```

🔸 **消息大小限制**

檔案位置：`crates/sail-common/src/config.rs`

```rust
pub const GRPC_MAX_MESSAGE_LENGTH_DEFAULT: usize = 128 * 1024 * 1024;  // 128 MB
```

為什麼需要限制？
- 防止惡意客戶端發送超大消息（DoS 攻擊）
- 防止內存溢出

🔸 **壓縮支援**

檔案位置：`crates/sail-spark-connect/src/entrypoint.rs:27-30`

```rust
let service = SparkConnectServiceServer::new(server)
    .accept_compressed(CompressionEncoding::Gzip)   // 接受 Gzip 壓縮的請求
    .accept_compressed(CompressionEncoding::Zstd)   // 接受 Zstd 壓縮的請求
    .send_compressed(CompressionEncoding::Gzip)     // 響應使用 Gzip 壓縮
    .send_compressed(CompressionEncoding::Zstd);    // 響應使用 Zstd 壓縮
```

Zstd vs Gzip：
- Zstd 更快（壓縮/解壓速度）
- Zstd 壓縮率稍好
- Gzip 更廣泛支援

---

## 如何調試 gRPC

🔸 **使用 grpcurl（類似 curl 的 gRPC 工具）**

```bash
# 列出所有服務
grpcurl -plaintext localhost:50051 list

# 查看服務方法
grpcurl -plaintext localhost:50051 list spark.connect.SparkConnectService

# 調用方法
grpcurl -plaintext -d '{
  "session_id": "test-session",
  "user_context": {"user_id": "test-user"},
  "operation": {
    "get_all": {"prefix": ""}
  }
}' localhost:50051 spark.connect.SparkConnectService/Config
```

🔸 **查看 gRPC 日誌**

Tonic 使用 `tracing` 框架，可以啟用詳細日誌：

```bash
RUST_LOG=tonic=debug,sail=debug sail spark server
```

你會看到：
```
[tonic] received request: path="/spark.connect.SparkConnectService/ExecutePlan"
[tonic] sending response: status=OK, stream=true
[sail] SessionManager: creating session abc-123-def-456
```

---

## 總結：為什麼 Spark Connect 選擇 gRPC？

1. **高性能**：二進制序列化 + HTTP/2 多路復用
2. **原生流式支援**：查詢結果可能有 GB 級別，需要流式返回
3. **強類型**：Protocol Buffers 確保客戶端和服務器契約一致
4. **跨語言**：PySpark、Scala Spark、Java Spark 都可以連接同一個服務器
5. **雙向流**：支援中斷、重連等高級功能

現在你已經理解 gRPC 的基礎了，讓我們進入 Sail 的實際流程！
