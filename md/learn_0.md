# Tonic 如何生成 Server 端與 Client 端邏輯

這篇文章深入探討 Tonic（Rust 的 gRPC 框架）是如何從 `.proto` 文件生成完整的 Server 端與 Client 端代碼的。我們會追蹤整個代碼生成流程，從 build.rs 執行到最終的 Rust 代碼。

---

## 完整流程概覽

```
.proto 文件
   ↓
cargo build 觸發 build.rs
   ↓
build.rs 調用 tonic-build
   ↓
tonic-build 調用 prost-build
   ↓
prost-build 調用 protoc（Google 的 Protobuf 編譯器）
   ↓
protoc 解析 .proto 文件，生成 FileDescriptorSet（中間表示）
   ↓
prost-build 讀取 FileDescriptorSet，生成 Message structs
   ↓
tonic-build 讀取 FileDescriptorSet，生成 Service trait + Server + Client
   ↓
寫入 $OUT_DIR/spark.connect.rs
   ↓
src/lib.rs 使用 include!() 宏引入生成的代碼
   ↓
編譯器編譯最終的 Rust 代碼
```

---

## 第一步：build.rs 的觸發機制

### 🔸 Cargo 的 Build Script 機制

檔案位置：`crates/sail-spark-connect/build.rs`

Cargo 在編譯 crate **之前**會先檢查是否存在 `build.rs` 文件。如果存在，會先編譯並執行它。

```rust
// build.rs 是一個獨立的可執行程序
fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=build.rs");  // 🔥 告訴 Cargo 何時重新運行
    build_proto()?;       // 生成 protobuf 代碼
    build_spark_config()?; // 生成 Spark 配置常量
    Ok(())
}
```

🔸 **cargo:rerun-if-changed 指令**

這是 Cargo 的特殊輸出格式，用於控制何時重新運行 build.rs：

```rust
println!("cargo:rerun-if-changed=build.rs");
println!("cargo:rerun-if-changed=proto/spark/connect/base.proto");
```

意思是：只有當 `build.rs` 或 `.proto` 文件改變時，才重新運行 build script。

🔸 **OUT_DIR 環境變量**

Cargo 在運行 build.rs 時會設置 `OUT_DIR` 環境變量：

```rust
let out_dir = PathBuf::from(std::env::var("OUT_DIR")?);
// out_dir = "target/debug/build/sail-spark-connect-<hash>/out/"
```

這個目錄是專門用來存放生成的代碼的。

---

## 第二步：tonic-build 的工作原理

### 🔸 tonic-build 的 API

檔案位置：`crates/sail-spark-connect/build.rs:14-36`

```rust
tonic_prost_build::configure()
    .protoc_arg("--experimental_allow_proto3_optional")  // 1. 傳遞給 protoc 的參數
    .file_descriptor_set_path(&descriptor_path)          // 2. 保存 FileDescriptorSet
    .compile_well_known_types(true)                      // 3. 包含 google.protobuf.* 類型
    .extern_path(".google.protobuf", "::pbjson_types")   // 4. 外部類型映射
    .build_server(true)                                   // 5. 🔥 生成 Server 端代碼
    .compile_with_config(
        config,
        &[
            "proto/spark/connect/base.proto",  // 要編譯的 .proto 文件列表
            // ...
        ],
        &["proto"],  // proto 文件的搜索路徑（用於 import）
    )?;
```

🔸 **關鍵配置選項**

| 配置 | 作用 |
|------|------|
| `.build_server(true)` | 生成 Server 端代碼（Service trait、Server builder） |
| `.build_client(true)` | 生成 Client 端代碼（預設啟用） |
| `.file_descriptor_set_path()` | 保存 FileDescriptorSet（用於反射和 JSON 序列化） |
| `.compile_well_known_types(true)` | 包含 google.protobuf.Timestamp 等常用類型 |
| `.extern_path()` | 將某些 protobuf 類型映射到外部 Rust 類型 |

---

## 第三步：protoc 生成 FileDescriptorSet

### 🔸 什麼是 FileDescriptorSet？

`FileDescriptorSet` 是 Protobuf 的中間表示（Intermediate Representation），它是一個二進制文件，包含所有 `.proto` 文件的結構化描述。

**為什麼需要它？**

`.proto` 文件是文本格式，不方便程序處理。`protoc` 會將其解析成結構化的數據，包含：
- 所有 message 的字段信息（名稱、類型、編號）
- 所有 service 的 RPC 方法信息
- 所有 enum 的值
- 註釋、選項等元信息

### 🔸 FileDescriptorSet 的結構

```protobuf
// google/protobuf/descriptor.proto（Protobuf 自帶的定義）
message FileDescriptorSet {
  repeated FileDescriptorProto file = 1;
}

message FileDescriptorProto {
  optional string name = 1;          // 文件名 "spark/connect/base.proto"
  optional string package = 2;       // 包名 "spark.connect"
  repeated DescriptorProto message_type = 4;  // 所有 message 定義
  repeated EnumDescriptorProto enum_type = 5; // 所有 enum 定義
  repeated ServiceDescriptorProto service = 6; // 🔥 所有 service 定義
  // ...
}

message ServiceDescriptorProto {
  optional string name = 1;  // "SparkConnectService"
  repeated MethodDescriptorProto method = 2;  // 所有 RPC 方法
}

message MethodDescriptorProto {
  optional string name = 1;              // "ExecutePlan"
  optional string input_type = 2;        // ".spark.connect.ExecutePlanRequest"
  optional string output_type = 3;       // ".spark.connect.ExecutePlanResponse"
  optional bool client_streaming = 5;    // false
  optional bool server_streaming = 6;    // true（因為有 stream 關鍵字）
}
```

### 🔸 查看生成的 FileDescriptorSet

Sail 保存了這個文件：

```bash
$ ls -lh target/debug/build/sail-spark-connect-*/out/spark_connect_descriptor.bin
-rw-r--r--  237K spark_connect_descriptor.bin
```

可以用 `protoc` 解碼它：

```bash
$ protoc --decode=google.protobuf.FileDescriptorSet \
    google/protobuf/descriptor.proto \
    < target/debug/build/sail-spark-connect-*/out/spark_connect_descriptor.bin
```

---

## 第四步：prost-build 生成 Message Structs

### 🔸 prost-build 的工作流程

`prost` 是 Rust 的 Protocol Buffers 實作（類似 Java 的 protobuf-java）。

**工作流程**：

1. 讀取 FileDescriptorSet
2. 遍歷所有 `DescriptorProto`（message 定義）
3. 為每個 message 生成對應的 Rust struct
4. 為每個字段添加 `#[prost(...)]` 屬性宏
5. 實作 `prost::Message` trait（提供序列化/反序列化方法）

### 🔸 生成的代碼範例

**Protobuf 定義**：

```protobuf
message ExecutePlanRequest {
  string session_id = 1;
  UserContext user_context = 2;
  Plan plan = 3;
  optional string operation_id = 6;
  repeated string tags = 7;
}
```

**生成的 Rust 代碼**：

```rust
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct ExecutePlanRequest {
    #[prost(string, tag = "1")]
    pub session_id: ::prost::alloc::string::String,

    #[prost(message, optional, tag = "2")]
    pub user_context: ::core::option::Option<UserContext>,

    #[prost(message, optional, tag = "3")]
    pub plan: ::core::option::Option<Plan>,

    #[prost(string, optional, tag = "6")]
    pub operation_id: ::core::option::Option<::prost::alloc::string::String>,

    #[prost(string, repeated, tag = "7")]
    pub tags: ::prost::alloc::vec::Vec<::prost::alloc::string::String>,
}
```

🔸 **#[prost(...)] 屬性宏**

這些屬性宏在編譯時會被 `prost` 的過程宏（procedural macro）處理，生成序列化/反序列化代碼。

```rust
// prost::Message trait 提供的方法
impl prost::Message for ExecutePlanRequest {
    fn encode_raw<B>(&self, buf: &mut B) where B: BufMut {
        // 將 session_id 編碼為 tag=1 的字段
        // 將 user_context 編碼為 tag=2 的字段
        // ...
    }

    fn decode<B>(buf: B) -> Result<Self, prost::DecodeError> where B: Buf {
        // 從二進制數據解碼
    }

    fn encoded_len(&self) -> usize {
        // 計算序列化後的長度
    }
}
```

---

## 第五步：tonic-build 生成 Service 代碼（核心！）

這是最關鍵的部分！tonic-build 會為每個 `service` 定義生成三部分代碼：

### 🔸 5.1 生成 Service Trait

**Protobuf 定義**：

```protobuf
service SparkConnectService {
  rpc ExecutePlan(ExecutePlanRequest) returns (stream ExecutePlanResponse);
  rpc AnalyzePlan(AnalyzePlanRequest) returns (AnalyzePlanResponse);
  rpc Config(ConfigRequest) returns (ConfigResponse);
}
```

**生成的 Trait**：

```rust
#[async_trait]
pub trait SparkConnectService: Send + Sync + 'static {
    // 對於 server streaming RPC，生成關聯類型
    type ExecutePlanStream: futures::Stream<
            Item = Result<ExecutePlanResponse, tonic::Status>
        > + Send + 'static;

    // 生成 async 方法
    async fn execute_plan(
        &self,
        request: tonic::Request<ExecutePlanRequest>,
    ) -> Result<tonic::Response<Self::ExecutePlanStream>, tonic::Status>;

    // Unary RPC 直接返回 Response
    async fn analyze_plan(
        &self,
        request: tonic::Request<AnalyzePlanRequest>,
    ) -> Result<tonic::Response<AnalyzePlanResponse>, tonic::Status>;

    async fn config(
        &self,
        request: tonic::Request<ConfigRequest>,
    ) -> Result<tonic::Response<ConfigResponse>, tonic::Status>;
}
```

🔸 **代碼生成邏輯（tonic-build 內部）**

```rust
// tonic-build 的內部邏輯（簡化版）
for service in file_descriptor.service {
    let trait_name = service.name;  // "SparkConnectService"

    for method in service.method {
        let method_name = to_snake_case(method.name);  // "execute_plan"
        let input_type = method.input_type;   // ".spark.connect.ExecutePlanRequest"
        let output_type = method.output_type; // ".spark.connect.ExecutePlanResponse"
        let is_server_streaming = method.server_streaming;

        if is_server_streaming {
            // 生成關聯類型（Associated Type）
            generate_associated_type(method_name, output_type);
        }

        // 生成 async fn 簽名
        generate_method_signature(method_name, input_type, output_type, is_server_streaming);
    }
}
```

### 🔸 5.2 生成 Server Builder

**生成的 Server 代碼**（簡化版）：

```rust
pub mod spark_connect_service_server {
    pub struct SparkConnectServiceServer<T> {
        inner: Arc<T>,  // T 是實作了 SparkConnectService trait 的類型
        // ... 壓縮、消息大小限制等配置
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
    }
}
```

### 🔸 5.3 生成 HTTP 路由邏輯（最核心！）

這是 tonic-build 最複雜的部分：將 HTTP/2 請求路由到對應的 trait 方法。

**生成的代碼**（簡化版，實際代碼更複雜）：

```rust
impl<T: SparkConnectService> tonic::codegen::Service<http::Request<Body>>
    for SparkConnectServiceServer<T>
{
    type Response = http::Response<tonic::body::BoxBody>;
    type Error = std::convert::Infallible;
    type Future = BoxFuture<Self::Response, Self::Error>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    // 🔥 核心：根據 HTTP 路徑路由到對應的方法
    fn call(&mut self, req: http::Request<Body>) -> Self::Future {
        let inner = self.inner.clone();

        match req.uri().path() {
            "/spark.connect.SparkConnectService/ExecutePlan" => {
                // 1. 反序列化請求
                let codec = tonic::codec::ProstCodec::default();
                let mut grpc = tonic::server::Grpc::new(codec);

                // 2. 調用 trait 方法
                Box::pin(async move {
                    let res = grpc.server_streaming(inner, req).await;
                    Ok(res)
                })
            }

            "/spark.connect.SparkConnectService/AnalyzePlan" => {
                // Unary RPC 處理
                Box::pin(async move {
                    let codec = tonic::codec::ProstCodec::default();
                    let mut grpc = tonic::server::Grpc::new(codec);
                    let res = grpc.unary(inner, req).await;
                    Ok(res)
                })
            }

            "/spark.connect.SparkConnectService/Config" => {
                // ...
            }

            _ => {
                // 404 Not Found
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
```

🔸 **HTTP 路徑映射**

gRPC 使用 HTTP/2 作為傳輸層，每個 RPC 方法對應一個 HTTP 路徑：

```
POST /spark.connect.SparkConnectService/ExecutePlan
POST /spark.connect.SparkConnectService/AnalyzePlan
POST /spark.connect.SparkConnectService/Config
```

路徑格式：`/<package>.<service>/<method>`

---

## 第六步：詳細的請求處理流程

讓我們深入看 `grpc.server_streaming()` 做了什麼：

```rust
// tonic 內部的 grpc.server_streaming() 方法（簡化版）
pub async fn server_streaming<T, S>(
    &mut self,
    service: Arc<T>,
    req: http::Request<Body>,
) -> http::Response<BoxBody>
where
    T: SparkConnectService,
    S: futures::Stream<Item = Result<ExecutePlanResponse, Status>>,
{
    // 1. 從 HTTP body 讀取二進制數據
    let body = req.into_body();
    let mut body_bytes = Vec::new();
    while let Some(chunk) = body.next().await {
        body_bytes.extend_from_slice(&chunk?);
    }

    // 2. 使用 prost 反序列化為 ExecutePlanRequest
    let request_message = ExecutePlanRequest::decode(&body_bytes[..])?;

    // 3. 包裝成 tonic::Request（添加 metadata）
    let request = tonic::Request::new(request_message);

    // 4. 🔥 調用實作的 trait 方法
    let response = service.execute_plan(request).await?;

    // 5. 取出響應流
    let mut response_stream = response.into_inner();

    // 6. 創建 HTTP/2 response
    let (tx, rx) = mpsc::channel(128);

    // 7. Spawn 異步任務，將流中的每個 ExecutePlanResponse 序列化並發送
    tokio::spawn(async move {
        while let Some(item) = response_stream.next().await {
            match item {
                Ok(response_message) => {
                    // 使用 prost 序列化為二進制
                    let mut buf = Vec::new();
                    response_message.encode(&mut buf)?;

                    // 發送到 HTTP/2 流
                    tx.send(Ok(buf)).await?;
                }
                Err(status) => {
                    // 發送錯誤狀態
                    tx.send(Err(status)).await?;
                    break;
                }
            }
        }
    });

    // 8. 返回 HTTP response
    http::Response::builder()
        .status(200)
        .header("content-type", "application/grpc")
        .header("grpc-encoding", "identity")
        .body(BoxBody::new(ReceiverStream::new(rx)))
        .unwrap()
}
```

---

## 第七步：代碼寫入與 Include

### 🔸 寫入生成的代碼

tonic-build 將所有生成的代碼寫入一個文件：

```rust
// tonic-build 內部邏輯
let output_file = out_dir.join("spark.connect.rs");
let mut file = File::create(output_file)?;

// 寫入文件頭
writeln!(file, "// This file is @generated by prost-build.")?;

// 寫入所有 Message structs
for message in messages {
    writeln!(file, "{}", generate_message_struct(message))?;
}

// 寫入所有 Service traits
for service in services {
    writeln!(file, "{}", generate_service_trait(service))?;
    writeln!(file, "{}", generate_server_builder(service))?;
    writeln!(file, "{}", generate_client_stub(service))?;
}
```

### 🔸 Include 生成的代碼

檔案位置：`crates/sail-spark-connect/src/lib.rs:18-26`

```rust
pub mod spark {
    pub mod connect {
        // 🔥 這個宏在編譯時會展開為：
        // include!(concat!(env!("OUT_DIR"), "/spark.connect.rs"));
        tonic::include_proto!("spark.connect");
    }
}
```

🔸 **include!() 宏的工作原理**

`include!()` 是 Rust 編譯器的內建宏，會在編譯時將文件內容**原地展開**：

```rust
// 展開前
tonic::include_proto!("spark.connect");

// 展開後（簡化）
include!(concat!(env!("OUT_DIR"), "/spark.connect.rs"));

// 進一步展開（編譯器做的）
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct ExecutePlanRequest { /* ... */ }

#[async_trait]
pub trait SparkConnectService: Send + Sync + 'static { /* ... */ }

pub mod spark_connect_service_server { /* ... */ }

pub mod spark_connect_service_client { /* ... */ }
```

---

## 完整的代碼生成時序圖

```
時間軸（Compile Time）                     文件系統
    |
    | 1. cargo build
    |    ↓
    | 2. 編譯並執行 build.rs
    |    ↓
    | 3. build.rs 調用 tonic_prost_build::configure()
    |    ↓
    | 4. tonic-build 調用 protoc
    |    ↓                                  base.proto
    |    |                                      ↓
    | 5. protoc 解析 .proto                 [protoc]
    |    ↓                                      ↓
    |    |                              spark_connect_descriptor.bin
    | 6. 生成 FileDescriptorSet              ↓
    |    ↓
    | 7. prost-build 讀取 FileDescriptorSet
    |    ↓
    | 8. 為每個 message 生成 Rust struct
    |    ↓
    | 9. tonic-build 讀取 FileDescriptorSet
    |    ↓
    | 10. 為每個 service 生成 trait
    |    ↓
    | 11. 生成 Server builder
    |    ↓
    | 12. 生成 Client stub
    |    ↓
    | 13. 將所有代碼寫入文件               spark.connect.rs
    |    ↓                                      ↓
    | 14. build.rs 執行完成
    |    ↓
    | 15. Cargo 編譯 src/lib.rs
    |    ↓
    | 16. 遇到 tonic::include_proto!()
    |    ↓
    | 17. 展開為 include!($OUT_DIR/spark.connect.rs)
    |    ↓
    | 18. 編譯器將生成的代碼原地插入         [編譯器內存]
    |    ↓
    | 19. 編譯整個 crate
    |    ↓
    | 20. 生成最終的二進制文件              libsail_spark_connect.rlib
    |
```

---

## 關鍵設計思想

### 🔸 為什麼使用 Trait？

Tonic 生成的是 **trait** 而不是具體的實作，這是一個精妙的設計：

**優點**：
1. **解耦**：gRPC 框架代碼與業務邏輯完全分離
2. **靈活性**：你可以用任何方式實作 trait（使用不同的數據庫、緩存等）
3. **可測試性**：可以輕鬆創建 mock 實作進行測試
4. **類型安全**：編譯器強制你實作所有 RPC 方法

**對比其他語言**：

```java
// Java gRPC（生成的是抽象類，需要繼承）
public abstract class SparkConnectServiceImplBase {
    public void executePlan(ExecutePlanRequest req, StreamObserver<ExecutePlanResponse> observer) {
        throw new StatusRuntimeException(Status.UNIMPLEMENTED);
    }
}

// 你需要繼承
public class MyService extends SparkConnectServiceImplBase {
    @Override
    public void executePlan(...) {
        // 實作
    }
}
```

```rust
// Rust gRPC（生成的是 trait，需要實作）
#[async_trait]
pub trait SparkConnectService: Send + Sync + 'static {
    async fn execute_plan(...) -> Result<...>;
}

// 你需要實作 trait
impl SparkConnectService for MyService {
    async fn execute_plan(...) -> Result<...> {
        // 實作
    }
}
```

Rust 的 trait 方式更加靈活，因為：
- 可以為任何類型實作 trait（不限於繼承）
- 支援多個 trait 組合（Rust 沒有繼承，但有 trait bounds）
- 零成本抽象（編譯時單態化，沒有虛表開銷）

### 🔸 為什麼使用 #[async_trait]？

Rust 原生不支援 trait 中的 async fn，因為 async fn 會返回 `impl Future`，而 trait 方法不能返回 `impl Trait`。

`#[async_trait]` 宏會將：

```rust
#[async_trait]
trait SparkConnectService {
    async fn execute_plan(&self, req: Request<ExecutePlanRequest>)
        -> Result<Response<Stream>, Status>;
}
```

轉換為：

```rust
trait SparkConnectService {
    fn execute_plan(&self, req: Request<ExecutePlanRequest>)
        -> Pin<Box<dyn Future<Output = Result<Response<Stream>, Status>> + Send + 'static>>;
}
```

這樣就可以在 trait 中使用異步方法了。

### 🔸 為什麼使用關聯類型（Associated Type）？

對於 streaming RPC，tonic 使用關聯類型：

```rust
trait SparkConnectService {
    type ExecutePlanStream: Stream<Item = Result<ExecutePlanResponse, Status>> + Send;

    async fn execute_plan(...) -> Result<Response<Self::ExecutePlanStream>, Status>;
}
```

**為什麼不用泛型？**

```rust
// 如果用泛型（不推薦）
trait SparkConnectService<S: Stream<...>> {
    async fn execute_plan(...) -> Result<Response<S>, Status>;
}
```

問題：
1. 每個實作必須指定流類型：`impl SparkConnectService<MyStream> for MyService`
2. Server builder 也需要泛型：`SparkConnectServiceServer<T, S>`
3. 複雜度爆炸

**使用關聯類型的優點**：
1. 實作者決定流類型：`type ExecutePlanStream = MyStream;`
2. Server builder 只需要一個泛型：`SparkConnectServiceServer<T>`
3. 更清晰的類型關係

---

## 總結：Tonic 代碼生成的精妙之處

### 🔸 編譯時代碼生成的優勢

1. **零運行時開銷**：所有代碼在編譯時生成，沒有反射或動態分發
2. **類型安全**：編譯器檢查所有類型，避免運行時錯誤
3. **IDE 支援**：生成的代碼可以被 rust-analyzer 分析，提供自動補全
4. **可調試**：可以查看生成的代碼，理解每一行在做什麼

### 🔸 Tonic 的設計哲學

1. **最小化手寫代碼**：只需要實作 trait 方法，其他都自動生成
2. **協議優先**：`.proto` 文件是唯一的真相來源
3. **Rust 慣用法**：生成的代碼遵循 Rust 的最佳實踐
4. **可擴展性**：可以通過 `tonic::include_proto!` 和自定義配置擴展

### 🔸 與其他語言 gRPC 框架的對比

| 語言 | 框架 | 代碼生成時機 | 抽象方式 |
|------|------|--------------|----------|
| Rust | Tonic | 編譯時（build.rs） | Trait |
| Java | grpc-java | 編譯前（protoc 插件） | Abstract Class |
| Go | grpc-go | 編譯前（protoc 插件） | Interface |
| Python | grpcio | 運行前（protoc 插件） | Duck Typing |

Rust 的編譯時生成是最獨特的，因為它與 Cargo 的 build script 機制深度集成。

---

## 實戰：查看生成的代碼

### 🔸 步驟 1：編譯項目

```bash
cd /Users/stanhsu/projects/sail
cargo build -p sail-spark-connect
```

### 🔸 步驟 2：找到生成的代碼

```bash
# 找到 OUT_DIR
find target/debug/build -name "spark.connect.rs"

# 輸出：target/debug/build/sail-spark-connect-<hash>/out/spark.connect.rs
```

### 🔸 步驟 3：查看生成的代碼

```bash
# 查看文件大小
ls -lh target/debug/build/sail-spark-connect-*/out/spark.connect.rs
# -rw-r--r--  300K spark.connect.rs

# 查看前 100 行
head -100 target/debug/build/sail-spark-connect-*/out/spark.connect.rs

# 搜索 SparkConnectService trait
grep -n "pub trait SparkConnectService" target/debug/build/sail-spark-connect-*/out/spark.connect.rs
```

### 🔸 步驟 4：理解生成的代碼結構

```bash
# 統計行數
wc -l target/debug/build/sail-spark-connect-*/out/spark.connect.rs
# 約 7000+ 行

# 查看包含的主要部分
grep -E "^(pub struct|pub trait|pub mod|pub enum)" target/debug/build/sail-spark-connect-*/out/spark.connect.rs | head -50
```

---

## 延伸閱讀

如果你想更深入了解 Tonic 的內部實作，可以閱讀：

1. **Tonic 源碼**：https://github.com/hyperium/tonic
   - `tonic-build/src/server.rs` - Server 代碼生成邏輯
   - `tonic-build/src/client.rs` - Client 代碼生成邏輯
   - `tonic/src/server/grpc.rs` - Server 運行時邏輯

2. **Prost 源碼**：https://github.com/tokio-rs/prost
   - `prost-build/src/code_generator.rs` - Message 代碼生成

3. **Protocol Buffers 文檔**：https://protobuf.dev/
   - FileDescriptorSet 規範
   - Protobuf 序列化格式

4. **Rust 異步編程**：
   - `async_trait` crate 文檔
   - Tokio 文檔（Tonic 基於 Tokio）

---

---

## Client 端代碼生成

在 `build.rs` 中，tonic-build 預設會同時生成 Server 和 Client 端代碼：

```rust
tonic_prost_build::configure()
    .build_server(true)   // 🔥 生成 Server 端
    .build_client(true)   // 🔥 生成 Client 端（預設已啟用）
    .compile_with_config(...)
```

### 🔸 生成的 Client 結構

tonic-build 會為每個 service 生成一個 `{ServiceName}Client` 結構體。

檔案位置：`target/debug/build/sail-spark-connect-*/out/spark.connect.rs`（生成的代碼）

**完整的 Client 代碼結構**：

```rust
pub mod spark_connect_service_client {
    use tonic::codegen::*;
    use tonic::codegen::http::Uri;

    // 🔥 Client 結構體
    #[derive(Debug, Clone)]
    pub struct SparkConnectServiceClient<T> {
        inner: tonic::client::Grpc<T>,  // 封裝底層 gRPC 連接
    }

    // 🔥 便利方法：直接連接到 endpoint
    impl SparkConnectServiceClient<tonic::transport::Channel> {
        pub async fn connect<D>(dst: D) -> Result<Self, tonic::transport::Error>
        where
            D: TryInto<tonic::transport::Endpoint>,
            D::Error: Into<StdError>,
        {
            let conn = tonic::transport::Endpoint::new(dst)?.connect().await?;
            Ok(Self::new(conn))
        }
    }

    // 🔥 泛型實作：支援任何傳輸層
    impl<T> SparkConnectServiceClient<T>
    where
        T: tonic::client::GrpcService<tonic::body::Body>,
        T::Error: Into<StdError>,
        T::ResponseBody: Body<Data = Bytes> + Send + 'static,
        <T::ResponseBody as Body>::Error: Into<StdError> + Send,
    {
        pub fn new(inner: T) -> Self {
            let inner = tonic::client::Grpc::new(inner);
            Self { inner }
        }

        pub fn with_interceptor<F>(
            inner: T,
            interceptor: F,
        ) -> SparkConnectServiceClient<InterceptedService<T, F>>
        where
            F: tonic::service::Interceptor,
        {
            // 添加攔截器（用於身份驗證、日誌等）
            SparkConnectServiceClient::new(InterceptedService::new(inner, interceptor))
        }

        // 🔥 RPC 方法：ExecutePlan（Server Streaming）
        pub async fn execute_plan(
            &mut self,
            request: impl tonic::IntoRequest<super::ExecutePlanRequest>,
        ) -> Result<
            tonic::Response<tonic::codec::Streaming<super::ExecutePlanResponse>>,
            tonic::Status,
        > {
            // 1. 確保連接就緒
            self.inner.ready().await.map_err(|e| {
                tonic::Status::unknown(format!("Service was not ready: {}", e.into()))
            })?;

            // 2. 創建 codec（用於序列化/反序列化）
            let codec = tonic_prost::ProstCodec::default();

            // 3. 設置 HTTP 路徑
            let path = http::uri::PathAndQuery::from_static(
                "/spark.connect.SparkConnectService/ExecutePlan",
            );

            // 4. 轉換請求
            let mut req = request.into_request();
            req.extensions_mut().insert(
                GrpcMethod::new("spark.connect.SparkConnectService", "ExecutePlan"),
            );

            // 5. 🔥 發送 server_streaming 請求
            self.inner.server_streaming(req, path, codec).await
        }

        // 🔥 RPC 方法：AnalyzePlan（Unary）
        pub async fn analyze_plan(
            &mut self,
            request: impl tonic::IntoRequest<super::AnalyzePlanRequest>,
        ) -> Result<tonic::Response<super::AnalyzePlanResponse>, tonic::Status> {
            self.inner.ready().await.map_err(|e| {
                tonic::Status::unknown(format!("Service was not ready: {}", e.into()))
            })?;

            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/spark.connect.SparkConnectService/AnalyzePlan",
            );

            let mut req = request.into_request();
            req.extensions_mut().insert(
                GrpcMethod::new("spark.connect.SparkConnectService", "AnalyzePlan"),
            );

            // 🔥 發送 unary 請求
            self.inner.unary(req, path, codec).await
        }

        // ... 其他 RPC 方法
    }
}
```

---

### 🔸 Client 的工作流程

讓我們追蹤一個 Client 請求的完整流程：

```rust
// 使用 Client 的示例代碼
use spark::connect::spark_connect_service_client::SparkConnectServiceClient;
use spark::connect::ExecutePlanRequest;

async fn call_server() -> Result<(), Box<dyn std::error::Error>> {
    // 步驟 1: 連接到 Server
    let mut client = SparkConnectServiceClient::connect("http://localhost:50051").await?;

    // 步驟 2: 創建請求
    let request = ExecutePlanRequest {
        session_id: "my-session".to_string(),
        // ... 其他字段
    };

    // 步驟 3: 發送請求（內部流程）
    let response = client.execute_plan(request).await?;

    // 步驟 4: 處理響應流
    let mut stream = response.into_inner();
    while let Some(item) = stream.message().await? {
        println!("Received: {:?}", item);
    }

    Ok(())
}
```

**內部流程詳解**：

```
1. client.execute_plan(request)
   ↓
2. self.inner.ready().await
   - 檢查底層 HTTP/2 連接是否就緒
   - 如果連接池有可用連接，直接使用
   - 否則等待連接可用
   ↓
3. request.into_request()
   - 將 ExecutePlanRequest 包裝成 tonic::Request<ExecutePlanRequest>
   - Request 包含 metadata（類似 HTTP headers）
   ↓
4. req.extensions_mut().insert(GrpcMethod::new(...))
   - 添加方法元信息（用於追蹤和日誌）
   ↓
5. self.inner.server_streaming(req, path, codec).await
   ↓
   5.1 使用 prost 序列化 ExecutePlanRequest
       - ExecutePlanRequest::encode(&mut buf)
       - 生成二進制數據
   ↓
   5.2 創建 HTTP/2 POST 請求
       POST /spark.connect.SparkConnectService/ExecutePlan HTTP/2
       Content-Type: application/grpc

       [二進制數據]
   ↓
   5.3 發送到 Server
   ↓
   5.4 接收 HTTP/2 響應流
   ↓
   5.5 創建 tonic::codec::Streaming<ExecutePlanResponse>
       - 這是一個 Stream，每次調用 .message().await 會：
         a. 從 HTTP/2 流讀取一個消息幀
         b. 使用 prost 反序列化為 ExecutePlanResponse
         c. 返回給調用者
   ↓
6. 返回 Result<Response<Streaming<ExecutePlanResponse>>, Status>
```

---

### 🔸 Client 端與 Server 端的對應關係

| 概念 | Server 端 | Client 端 |
|------|-----------|-----------|
| **抽象方式** | Trait（需要實作） | Struct（直接使用） |
| **生成的類型** | `SparkConnectService` trait | `SparkConnectServiceClient<T>` struct |
| **方法簽名** | `async fn execute_plan(&self, req: Request<...>) -> Result<Response<Stream>, Status>` | `async fn execute_plan(&mut self, req: impl IntoRequest<...>) -> Result<Response<Streaming<...>>, Status>` |
| **傳輸層** | 綁定到具體的 TCP listener | 泛型 `T: GrpcService<Body>` |
| **使用方式** | `impl SparkConnectService for MyService` | `SparkConnectServiceClient::connect(url).await` |
| **流類型** | 自定義 `type ExecutePlanStream` | 固定 `tonic::codec::Streaming<Response>` |

---

### 🔸 Client 的關鍵設計

**1. 泛型傳輸層**

```rust
pub struct SparkConnectServiceClient<T> {
    inner: tonic::client::Grpc<T>,
}
```

`T` 可以是：
- `tonic::transport::Channel` - 預設的 HTTP/2 傳輸
- 自定義傳輸層（例如 Unix domain socket）
- Mock 傳輸（用於測試）

**2. IntoRequest trait**

```rust
pub async fn execute_plan(
    &mut self,
    request: impl tonic::IntoRequest<super::ExecutePlanRequest>,
) -> ...
```

`IntoRequest` 允許以下幾種調用方式：

```rust
// 方式 1: 直接傳遞 Message
client.execute_plan(ExecutePlanRequest { ... }).await?;

// 方式 2: 傳遞 tonic::Request（可以添加 metadata）
let mut req = tonic::Request::new(ExecutePlanRequest { ... });
req.metadata_mut().insert("authorization", "Bearer token".parse()?);
client.execute_plan(req).await?;
```

**3. Interceptor 模式**

```rust
pub fn with_interceptor<F>(
    inner: T,
    interceptor: F,
) -> SparkConnectServiceClient<InterceptedService<T, F>>
where
    F: tonic::service::Interceptor,
```

Interceptor 可以在每個請求發送前修改它，常用於：

```rust
// 添加身份驗證
fn auth_interceptor(mut req: Request<()>) -> Result<Request<()>, Status> {
    let token = get_auth_token()?;
    req.metadata_mut().insert(
        "authorization",
        format!("Bearer {}", token).parse().unwrap(),
    );
    Ok(req)
}

let client = SparkConnectServiceClient::connect(url)
    .await?
    .with_interceptor(auth_interceptor);
```

---

### 🔸 四種 RPC 模式的 Client 端實作

tonic-build 根據 RPC 的流類型生成不同的 Client 方法：

**1. Unary RPC**

```protobuf
rpc AnalyzePlan(AnalyzePlanRequest) returns (AnalyzePlanResponse);
```

```rust
// 生成的 Client 方法
pub async fn analyze_plan(
    &mut self,
    request: impl tonic::IntoRequest<super::AnalyzePlanRequest>,
) -> Result<tonic::Response<super::AnalyzePlanResponse>, tonic::Status> {
    self.inner.unary(req, path, codec).await
    //         ^^^^^ 🔥 使用 unary 方法
}
```

**2. Server Streaming RPC**

```protobuf
rpc ExecutePlan(ExecutePlanRequest) returns (stream ExecutePlanResponse);
```

```rust
// 生成的 Client 方法
pub async fn execute_plan(
    &mut self,
    request: impl tonic::IntoRequest<super::ExecutePlanRequest>,
) -> Result<
    tonic::Response<tonic::codec::Streaming<super::ExecutePlanResponse>>,
    //              ^^^^^^^^^^^^^^^^^^^^^^^^^ 🔥 返回一個 Stream
    tonic::Status,
> {
    self.inner.server_streaming(req, path, codec).await
    //         ^^^^^^^^^^^^^^^^ 🔥 使用 server_streaming 方法
}

// 使用方式
let response = client.execute_plan(request).await?;
let mut stream = response.into_inner();
while let Some(msg) = stream.message().await? {
    println!("{:?}", msg);
}
```

**3. Client Streaming RPC**

```protobuf
rpc Upload(stream UploadRequest) returns (UploadResponse);
```

```rust
// 生成的 Client 方法
pub async fn upload(
    &mut self,
    request: impl tonic::IntoStreamingRequest<Message = super::UploadRequest>,
    //       ^^^^^^^^^^^^^^^^^^^^^^^^^^^ 🔥 接收一個 Stream
) -> Result<tonic::Response<super::UploadResponse>, tonic::Status> {
    self.inner.client_streaming(req, path, codec).await
    //         ^^^^^^^^^^^^^^^^ 🔥 使用 client_streaming 方法
}

// 使用方式
let stream = tokio_stream::iter(vec![
    UploadRequest { chunk: vec![1, 2, 3] },
    UploadRequest { chunk: vec![4, 5, 6] },
]);
let response = client.upload(stream).await?;
```

**4. Bidirectional Streaming RPC**

```protobuf
rpc Chat(stream ChatMessage) returns (stream ChatMessage);
```

```rust
// 生成的 Client 方法
pub async fn chat(
    &mut self,
    request: impl tonic::IntoStreamingRequest<Message = super::ChatMessage>,
) -> Result<
    tonic::Response<tonic::codec::Streaming<super::ChatMessage>>,
    //              ^^^^^^^^^^^^^^^^^^^^^^^^^ 🔥 返回一個 Stream
    tonic::Status,
> {
    self.inner.streaming(req, path, codec).await
    //         ^^^^^^^^^ 🔥 使用 streaming 方法
}

// 使用方式
let input_stream = tokio_stream::iter(vec![
    ChatMessage { text: "Hello".to_string() },
    ChatMessage { text: "World".to_string() },
]);
let response = client.chat(input_stream).await?;
let mut output_stream = response.into_inner();
while let Some(msg) = output_stream.message().await? {
    println!("Received: {}", msg.text);
}
```

---

### 🔸 Client 端與 Server 端的代碼對比

讓我們並排對比相同 RPC 方法的 Server 和 Client 實作：

**Server 端（Trait）**：

```rust
#[async_trait]
pub trait SparkConnectService: Send + Sync + 'static {
    type ExecutePlanStream: Stream<Item = Result<ExecutePlanResponse, Status>> + Send;

    async fn execute_plan(
        &self,  // 🔥 &self（不可變）
        request: tonic::Request<ExecutePlanRequest>,
    ) -> Result<tonic::Response<Self::ExecutePlanStream>, tonic::Status>;
    //                          ^^^^^^^^^^^^^^^^^^^^^^ 🔥 自定義流類型
}
```

**Client 端（Struct）**：

```rust
pub struct SparkConnectServiceClient<T> {
    inner: tonic::client::Grpc<T>,
}

impl<T: GrpcService<Body>> SparkConnectServiceClient<T> {
    pub async fn execute_plan(
        &mut self,  // 🔥 &mut self（需要修改內部狀態）
        request: impl tonic::IntoRequest<ExecutePlanRequest>,
        //       ^^^^ 🔥 IntoRequest trait（更靈活）
    ) -> Result<
        tonic::Response<tonic::codec::Streaming<ExecutePlanResponse>>,
        //              ^^^^^^^^^^^^^^^^^^^^^^^^^^^ 🔥 固定的 Streaming 類型
        tonic::Status,
    > {
        self.inner.server_streaming(request.into_request(), path, codec).await
    }
}
```

**關鍵差異**：

| 方面 | Server 端 | Client 端 |
|------|-----------|-----------|
| `self` 類型 | `&self`（不可變） | `&mut self`（需要修改連接狀態） |
| 請求類型 | `Request<T>` | `impl IntoRequest<T>`（支援多種輸入） |
| 流類型 | 關聯類型 `Self::Stream`（靈活） | 固定 `Streaming<T>`（統一） |
| 實作方式 | 需要實作 trait | 直接使用生成的 struct |

---

### 🔸 實際案例：Sail 中沒有使用 Client

有趣的是，Sail 本身並**不使用**生成的 Client 代碼！

```rust
// crates/sail-spark-connect/build.rs
tonic_prost_build::configure()
    .build_server(true)   // ✅ 使用：Sail 是一個 gRPC Server
    .build_client(true)   // ⚠️  生成但不使用（預設啟用）
    .compile_with_config(...)
```

**為什麼不使用 Client？**

因為 Sail 的架構是：

```
PySpark Client (Python) --gRPC--> Sail Server (Rust)
                                       |
                                       v
                                  DataFusion
```

Sail 是 Server 端，它接收來自 PySpark 的請求。PySpark 使用 Python 的 gRPC 客戶端庫，而不是 Rust 的。

**如果要測試 Sail，可以用以下方式**：

1. 使用 PySpark 客戶端（官方支援）
2. 使用 `grpcurl` 命令行工具
3. 寫一個 Rust 測試程序，使用生成的 `SparkConnectServiceClient`

**Rust 測試 Client 示例**（如果要測試的話）：

```rust
#[cfg(test)]
mod tests {
    use super::spark::connect::spark_connect_service_client::SparkConnectServiceClient;
    use super::spark::connect::{ExecutePlanRequest, Plan};

    #[tokio::test]
    async fn test_execute_plan() {
        // 連接到 Sail Server
        let mut client = SparkConnectServiceClient::connect("http://localhost:50051")
            .await
            .unwrap();

        // 創建請求
        let request = ExecutePlanRequest {
            session_id: "test-session".to_string(),
            user_context: None,
            plan: Some(Plan {
                // ... 構建 SQL 查詢計劃
            }),
            operation_id: None,
            tags: vec![],
        };

        // 發送請求
        let response = client.execute_plan(request).await.unwrap();

        // 處理響應
        let mut stream = response.into_inner();
        while let Some(msg) = stream.message().await.unwrap() {
            println!("Response: {:?}", msg);
        }
    }
}
```

---

## 總結：Server 與 Client 的完整對比

### 🔸 代碼生成的三個產物

從一個 `.proto` service 定義，tonic-build 生成：

| 產物 | 類型 | 用途 | 位置 |
|------|------|------|------|
| **Message Structs** | `pub struct ExecutePlanRequest` | 序列化/反序列化 | 由 prost-build 生成 |
| **Service Trait** | `pub trait SparkConnectService` | Server 端實作 | 由 tonic-build 生成 |
| **Server Builder** | `pub struct SparkConnectServiceServer<T>` | 創建 gRPC Server | 由 tonic-build 生成 |
| **Client Stub** | `pub struct SparkConnectServiceClient<T>` | Client 端調用 | 由 tonic-build 生成 |

### 🔸 完整的 gRPC 通信流程

```
Client 端                        網絡                         Server 端
   |                              |                              |
   | 1. 創建 Message               |                              |
   |    ExecutePlanRequest        |                              |
   |                              |                              |
   | 2. 調用 Client 方法           |                              |
   |    client.execute_plan()     |                              |
   |                              |                              |
   | 3. prost 序列化               |                              |
   |    Message -> bytes          |                              |
   |                              |                              |
   | 4. 創建 HTTP/2 POST           |                              |
   |    Path: /pkg.Svc/Method     |                              |
   |         |                    |                              |
   |         +--------------------+----------------------------->|
   |                              |                              |
   |                              |  5. 路由到 trait 方法        |
   |                              |     match req.uri().path()  |
   |                              |                              |
   |                              |  6. prost 反序列化           |
   |                              |     bytes -> Message        |
   |                              |                              |
   |                              |  7. 調用實作的 trait 方法    |
   |                              |     service.execute_plan()  |
   |                              |                              |
   |                              |  8. 業務邏輯處理             |
   |                              |     (DataFusion 查詢)       |
   |                              |                              |
   |                              |  9. 返回 Stream              |
   |                              |                              |
   |         +<-------------------+------------------------------+
   |         |                    |  10. prost 序列化每個響應    |
   | 11. Streaming<Response>      |      Response -> bytes      |
   |     stream.message().await   |                              |
   |                              |                              |
   | 12. prost 反序列化            |                              |
   |     bytes -> Response        |                              |
   |                              |                              |
   | 13. 業務邏輯處理              |                              |
   |     println!("{:?}", msg)    |                              |
```

---

希望這篇文章幫助你徹底理解了 Tonic 是如何從 `.proto` 文件生成完整的 Server 端與 Client 端邏輯的！
