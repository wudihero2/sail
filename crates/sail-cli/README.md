# sail-cli

Sail 的命令列介面，提供 Spark Connect 伺服器、PySpark Shell 和 MCP 伺服器功能。

## 📁 檔案結構

```
sail-cli/src/
├── main.rs           # 程式入口點，處理 Python 嵌入邏輯
├── lib.rs            # 模組定義與匯出
├── runner.rs         # CLI 命令解析與分發（使用 clap）
├── python.rs         # Python 模組載入器與日誌橋接
├── spark/
│   ├── mod.rs        # Spark 子模組匯出
│   ├── server.rs     # Spark Connect gRPC 伺服器
│   ├── shell.rs      # PySpark 互動式 Shell
│   └── mcp_server.rs # MCP (Model Context Protocol) 伺服器
└── worker/
    ├── mod.rs        # Worker 子模組匯出
    └── entrypoint.rs # 分散式 Worker 入口點
```

## 🔸 CLI 命令

```
sail
├── spark
│   ├── server --ip --port -C     # 啟動 Spark Connect 伺服器
│   ├── shell                      # 啟動 PySpark Shell
│   └── mcp-server --host --port --transport --spark-remote
└── worker                         # 內部使用的 Worker 程序
```

## 🔸 建議閱讀順序

| 順序 | 檔案 | 說明 |
|------|------|------|
| 1 | main.rs | 程式入口，理解 Python 嵌入機制 |
| 2 | runner.rs | CLI 結構定義，了解所有可用命令 |
| 3 | spark/server.rs | 核心伺服器啟動邏輯 |
| 4 | spark/shell.rs | 互動式 Shell 實作 |
| 5 | python.rs | Python 與 Rust 的橋接層 |
| 6 | worker/entrypoint.rs | 分散式執行的 Worker |
| 7 | sail-common | 設定載入機制 |
| 8 | sail-spark-connect | gRPC 伺服器實作 |

## 🔸 調用鏈總覽

```
                            ┌─────────────────────────────────────┐
                            │           main.rs::main()           │
                            └─────────────────┬───────────────────┘
                                              │
                    ┌─────────────────────────┴─────────────────────────┐
                    │                                                   │
         [RUN_PYTHON=true]                                   [RUN_PYTHON=false]
                    │                                                   │
                    ▼                                                   ▼
    ┌───────────────────────────┐                    ┌──────────────────────────────┐
    │ run_python_interpreter()  │                    │    Python::initialize()      │
    │   直接啟動 Python 直譯器  │                    │    初始化嵌入式 Python       │
    └───────────────────────────┘                    └──────────────┬───────────────┘
                                                                    │
                                                                    ▼
                                                     ┌──────────────────────────────┐
                                                     │    runner::main(args)        │
                                                     │    解析 CLI 參數並分發       │
                                                     └──────────────┬───────────────┘
                                                                    │
                    ┌───────────────────┬───────────────────┬───────┴───────┐
                    │                   │                   │               │
                    ▼                   ▼                   ▼               ▼
          ┌─────────────┐    ┌──────────────────┐  ┌─────────────┐  ┌───────────────┐
          │ run_worker  │    │ run_spark_server │  │ run_shell   │  │ run_mcp_server│
          └─────────────┘    └──────────────────┘  └─────────────┘  └───────────────┘
```

## 🔸 Dependencies

| Crate | 用途 |
|-------|------|
| sail-common | 共用設定（AppConfig、CliConfig）與錯誤處理 |
| sail-execution | 分散式執行引擎，Worker 使用 |
| sail-telemetry | 遙測、追蹤與日誌初始化 |
| sail-spark-connect | Spark Connect gRPC 服務實作 |
| sail-server | gRPC 伺服器建構器 |
| clap | CLI 參數解析，使用 derive 巨集 |
| tokio | 異步運行時，處理並發連線 |
| pyo3 | Python 嵌入與 FFI 綁定 |
| mimalloc | 高效能記憶體分配器（可選特性） |
| rustls | TLS 加密，使用 aws-lc-rs 後端 |
| fastrace | 分散式追蹤，結束時 flush |
| figment | 設定載入，支援多來源合併 |

## 🔸 使用範例

```bash
# 啟動 Spark Connect 伺服器（預設 127.0.0.1:50051）
sail spark server --port 50051

# 啟動 PySpark 互動式 Shell
sail spark shell

# 啟動 MCP 伺服器（SSE 傳輸）
sail spark mcp-server --transport sse --port 8000

# 啟動 MCP 伺服器並連接到外部 Spark
sail spark mcp-server --spark-remote "sc://remote-host:50051"
```

---

## 🔸 完整調用鏈詳解：sail spark server

這裡以最常用的 `sail spark server` 命令為例，從頭到尾解釋每一個函數調用。

### 第一層：main.rs::main()

```rust
use std::ffi::NulError;

use pyo3::ffi::{PyUnicode_AsWideCharString, PyUnicode_FromString, Py_Main};
use pyo3::Python;
use sail_common::config::{CliConfig, CliConfigEnv};
use sail_common::error::CommonError;
```

這段是模組引入：
- `std::ffi::NulError`：處理 C 字串中包含 null 字元的錯誤型別
- `pyo3::ffi::*`：Python C API 的低階 FFI 綁定，用於直接操作 Python 直譯器
- `pyo3::Python`：pyo3 的高階 API，管理 Python 直譯器生命週期
- `CliConfig`：CLI 設定結構，從環境變數載入
- `CommonError`：統一錯誤型別

```rust
#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;
```

Rust 語法解說：
- `#[cfg(feature = "mimalloc")]`：條件編譯屬性，只有在 Cargo.toml 中啟用 `mimalloc` feature 時才編譯這段程式碼
- `#[global_allocator]`：特殊屬性，指定這個靜態變數為全域記憶體分配器
- `static GLOBAL`：靜態變數，生命週期是整個程式執行期間
- `mimalloc::MiMalloc`：mimalloc 分配器的實例

這行的作用是把整個程式的全域記憶體配置器換成 mimalloc。也就是說：之後所有 `Box`, `Vec`, `String`, `HashMap` 等在 heap 上 alloc/free，全部都會走 mimalloc。mimalloc 是微軟開發的高效能記憶體分配器，比系統預設的 malloc 快。

```rust
fn main() -> Result<(), Box<dyn std::error::Error>> {
    if rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .is_err()
    {
        Err(CommonError::InternalError(
            "failed to install crypto provider".to_string(),
        ))?;
    }
```

Rust 語法解說：
- `Result<(), Box<dyn std::error::Error>>`：函數回傳型別
  - `()`：unit type，代表成功時不回傳任何值
  - `Box<dyn std::error::Error>`：動態分派的錯誤型別
    - `Box`：堆積分配的智慧指標，擁有資料的所有權
    - `dyn`：動態分派，執行時才決定具體型別
    - `std::error::Error`：標準錯誤 trait
- `rustls::crypto::aws_lc_rs::default_provider()`：取得預設的加密提供者（aws-lc-rs）
- `.install_default()`：嘗試將它設為全域預設
- `.is_err()`：檢查是否失敗
- `Err(...)?`：建立一個錯誤並用 `?` 運算子提早回傳

這段初始化 TLS 加密提供者。rustls 是 Rust 的 TLS 實作，aws-lc-rs 是 AWS 的加密庫（基於 BoringSSL）。如果初始化失敗，整個程式就會提早結束。

```rust
    let config = CliConfig::load()?;
```

這行呼叫 `CliConfig::load()` 載入 CLI 設定。`?` 運算子會在失敗時提早回傳錯誤。

### 第二層：CliConfig::load() (sail-common/src/config/cli.rs)

```rust
use figment::providers::{Env, Serialized};
use figment::Figment;
use serde::{Deserialize, Serialize};

use crate::error::{CommonError, CommonResult};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CliConfig {
    pub run_python: bool,
}
```

Rust 語法解說：
- `#[derive(...)]`：自動實作 traits
  - `Debug`：讓結構可以用 `{:?}` 格式化輸出
  - `Clone`：讓結構可以呼叫 `.clone()` 複製
  - `Default`：提供預設值（`run_python: false`）
  - `Serialize`/`Deserialize`：serde 的序列化/反序列化
- `pub struct CliConfig`：公開的結構體
- `pub run_python: bool`：公開欄位，布林值

```rust
impl CliConfig {
    pub fn load() -> CommonResult<Self> {
        Figment::from(Serialized::defaults(CliConfig::default()))
            .merge(Env::prefixed("SAIL_INTERNAL__").map(|p| p.as_str().replace("__", ".").into()))
            .extract()
            .map_err(|e| CommonError::InvalidArgument(e.to_string()))
    }
}
```

Rust 語法解說：
- `impl CliConfig`：為 `CliConfig` 實作方法
- `pub fn load() -> CommonResult<Self>`：
  - `CommonResult<Self>`：型別別名，等同於 `Result<CliConfig, CommonError>`
  - `Self`：代表 `CliConfig` 本身
- `Figment::from(...)`：建立設定載入器
  - `Serialized::defaults(CliConfig::default())`：使用 `CliConfig` 的預設值作為基礎層
- `.merge(...)`：合併另一個設定來源
  - `Env::prefixed("SAIL_INTERNAL__")`：讀取以 `SAIL_INTERNAL__` 開頭的環境變數
  - `.map(|p| p.as_str().replace("__", ".").into())`：將雙底線替換為點（例如 `SAIL_INTERNAL__RUN_PYTHON` 變成 `run_python`）
- `.extract()`：提取並反序列化為 `CliConfig`
- `.map_err(...)`：將錯誤轉換為 `CommonError`

這個函數會檢查環境變數 `SAIL_INTERNAL__RUN_PYTHON`。如果設為 `true`，`config.run_python` 就會是 `true`。

### 回到第一層：main.rs::main()

```rust
    if config.run_python {
        run_python_interpreter()
    } else {
        std::env::set_var(CliConfigEnv::RUN_PYTHON, "true");
        Python::initialize();
        let args = std::env::args().collect();
        match sail_cli::runner::main(args) {
            Ok(()) => {}
            Err(e) => {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        }
    }
    Ok(())
}
```

這是核心分支邏輯：
- 如果 `config.run_python` 是 `true`：這是一個被 fork 的子程序，呼叫 `run_python_interpreter()` 直接啟動 Python 直譯器（詳見下一節）
- 如果是 `false`（正常情況）：
  - `std::env::set_var(CliConfigEnv::RUN_PYTHON, "true")`：設定環境變數 `SAIL_INTERNAL__RUN_PYTHON=true`，讓未來 fork 的子程序知道要以 Python 模式運行
  - `Python::initialize()`：初始化嵌入式 Python 直譯器（pyo3 crate 提供，詳見下方說明）
  - `std::env::args().collect()`：收集命令列參數為 `Vec<String>`
  - `sail_cli::runner::main(args)`：呼叫主要的 CLI 邏輯
  - `match` 處理結果：成功就繼續，失敗就印錯誤並 `exit(1)`

### Python::initialize() 說明

```rust
Python::initialize();
```

這是 pyo3 crate 提供的函數（非 sail repo 源碼），用於初始化嵌入式 Python 直譯器。

主要作用：
- 初始化 Python 運行時，讓 Rust 程式可以執行 Python 程式碼
- 設定 `sys.executable` 指向當前執行檔（`sail` 二進位檔）
- 初始化後可以使用 `Python::attach(|py| { ... })` 執行 Python 程式碼

為什麼需要？
- Sail CLI 需要執行 Python 程式碼（例如啟動 PySpark Shell）
- `sys.executable` 指向 `sail` 是為什麼 multiprocessing fork 時需要 `run_python_interpreter()` 的原因

Rust 語法解說：
- `eprintln!(...)`：印到標準錯誤輸出的巨集
- `std::process::exit(1)`：以狀態碼 1 結束程序
- `{e}`：格式化字串，等同於 `{}`，但變數名稱更清楚

為什麼需要這個機制？當 Python 的 `multiprocessing` 模組 fork 子程序時，它會用 `sys.executable`（指向 sail 二進位檔）來啟動新程序。這個機制讓 fork 出來的程序能正確地以 Python 模式運行。

### run_python_interpreter() 分支詳解

當 `config.run_python` 為 `true` 時（子程序模式），會呼叫 `run_python_interpreter()`：

```rust
fn run_python_interpreter() -> ! {
    let args = std::env::args();

    let argc = args.len() as i32;
    let Ok(mut argv) = args
        .into_iter()
        .map(|arg| {
            let arg = std::ffi::CString::new(arg)?;
            let arg = unsafe {
                let obj = PyUnicode_FromString(arg.as_ptr());
                PyUnicode_AsWideCharString(obj, std::ptr::null_mut())
            };
            Ok(arg)
        })
        .collect::<Result<Vec<_>, NulError>>()
    else {
        eprintln!("Error: null bytes found in command line argument strings");
        std::process::exit(1);
    };
    argv.push(std::ptr::null_mut());

    let code = unsafe { Py_Main(argc, argv.as_mut_ptr()) };
    std::process::exit(code)
}
```

Rust 語法解說：
- `fn run_python_interpreter() -> !`：函數回傳型別是 `!`（Never type）
  - Never type 表示這個函數永遠不會正常回傳
  - 只會透過 `std::process::exit()` 結束程序
- `std::env::args()`：取得命令列參數的迭代器
- `args.len() as i32`：型別轉換，將 `usize` 轉為 `i32`
  - Python C API 需要 `i32` 型別的 argc

參數轉換過程：
```rust
let Ok(mut argv) = args
    .into_iter()
    .map(|arg| {
        let arg = std::ffi::CString::new(arg)?;
        let arg = unsafe {
            let obj = PyUnicode_FromString(arg.as_ptr());
            PyUnicode_AsWideCharString(obj, std::ptr::null_mut())
        };
        Ok(arg)
    })
    .collect::<Result<Vec<_>, NulError>>()
else {
    eprintln!("Error: null bytes found in command line argument strings");
    std::process::exit(1);
};
```

Rust 語法解說：
- `.into_iter()`：消耗迭代器，取得每個參數的所有權
- `.map(|arg| { ... })`：閉包，轉換每個參數
  - `|arg|`：閉包參數
  - `{ ... }`：閉包主體
- `std::ffi::CString::new(arg)?`：將 Rust String 轉為 C 字串
  - C 字串以 null (`\0`) 結尾
  - `?` 會在字串包含 null 字元時回傳錯誤
- `unsafe { ... }`：不安全區塊，因為要呼叫 FFI 函數
  - Rust 無法保證 C 函數的安全性，所以需要明確標記
- `PyUnicode_FromString(arg.as_ptr())`：Python C API
  - 將 C 字串轉為 Python Unicode 物件
  - `arg.as_ptr()`：取得 C 字串的指標
- `PyUnicode_AsWideCharString(obj, std::ptr::null_mut())`：Python C API
  - 將 Python Unicode 轉為寬字元字串（wchar_t*）
  - Windows 和某些平台需要寬字元
  - `std::ptr::null_mut()`：可變的 null 指標
- `.collect::<Result<Vec<_>, NulError>>()`：收集結果
  - `Result<Vec<_>, NulError>`：Turbofish 語法指定型別
  - `Vec<_>`：編譯器推斷內部型別
  - 如果任何一個 `map` 回傳 `Err`，整個 `collect` 就會是 `Err`
- `let Ok(mut argv) = ... else { ... }`：let-else 語法（Rust 1.65+）
  - 如果是 `Ok`，解構出 `argv`
  - 如果是 `Err`，執行 `else` 區塊

為什麼需要寬字元轉換？
- Windows 的命令列參數是 UTF-16 編碼（寬字元）
- `PyUnicode_AsWideCharString` 確保跨平台相容性
- Python 內部會正確處理各平台的字元編碼

加入 null 終止符：
```rust
argv.push(std::ptr::null_mut());
```

Rust 語法解說：
- `std::ptr::null_mut()`：建立可變的 null 指標
- C 的 `argv` 陣列需要以 null 指標結尾
- 這是 C 語言的慣例，表示陣列的結束

啟動 Python 直譯器：
```rust
let code = unsafe { Py_Main(argc, argv.as_mut_ptr()) };
std::process::exit(code)
```

Rust 語法解說：
- `Py_Main(argc, argv.as_mut_ptr())`：Python C API，啟動 Python 主迴圈
  - `argc`：參數數量
  - `argv.as_mut_ptr()`：取得可變指標陣列的指標（`**wchar_t`）
  - 這個函數會完全接管程序，執行 Python 程式碼
- `std::process::exit(code)`：以 Python 的退出碼結束程序
  - `code` 是 Python 直譯器的退出狀態（通常 0 表示成功）

完整執行流程範例：

假設 Python `multiprocessing` 模組執行以下程式碼：
```python
from multiprocessing import Process

def worker():
    print("Hello from worker")

if __name__ == "__main__":
    p = Process(target=worker)
    p.start()
    p.join()
```

執行流程：
1. 主程序：`sail spark shell` 啟動，`SAIL_INTERNAL__RUN_PYTHON` 未設定
2. 主程序：`config.run_python = false`，初始化 Python 並設定環境變數
3. Python 執行到 `p.start()`，需要 fork 新程序
4. Python 使用 `sys.executable`（指向 `sail` 二進位檔）啟動子程序
5. 子程序：啟動時 `SAIL_INTERNAL__RUN_PYTHON=true`（繼承環境變數）
6. 子程序：`config.run_python = true`，呼叫 `run_python_interpreter()`
7. 子程序：轉換命令列參數為 Python 格式
8. 子程序：呼叫 `Py_Main()`，以純 Python 直譯器模式運行
9. 子程序：執行 `worker()` 函數，印出 "Hello from worker"
10. 子程序：完成後 `exit(0)`

### 第三層：runner::main()

```rust
use clap::{Parser, Subcommand};

use crate::spark::{
    run_pyspark_shell, run_spark_connect_server, run_spark_mcp_server, McpSettings, McpTransport,
};
use crate::worker::run_worker;
```

引入 clap 的 derive 巨集和內部模組。

```rust
#[derive(Parser)]
#[command(version, name = "sail", about = "Sail CLI")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}
```

Rust 語法解說：
- `#[derive(Parser)]`：自動實作 clap 的 `Parser` trait，能從命令列參數解析
- `#[command(version, name = "sail", about = "Sail CLI")]`：
  - `version`：自動使用 `CARGO_PKG_VERSION`
  - `name = "sail"`：命令名稱
  - `about = "..."`：說明文字
- `struct Cli`：CLI 的根結構
- `#[command(subcommand)]`：標記這個欄位是子命令

```rust
#[derive(Subcommand)]
enum Command {
    #[command(subcommand, about = "Run Spark workloads with Sail")]
    Spark(SparkCommand),
    #[command(about = "Start the Sail worker (internal use only)")]
    Worker,
}
```

Rust 語法解說：
- `#[derive(Subcommand)]`：自動實作子命令解析
- `enum Command`：列舉型別，每個變體代表一個子命令
- `Spark(SparkCommand)`：`spark` 子命令，還有更深的子命令
- `Worker`：`worker` 子命令，不帶額外參數

```rust
#[derive(Subcommand)]
enum SparkCommand {
    #[command(about = "Start the Spark Connect server")]
    Server {
        #[arg(
            long,
            default_value = "127.0.0.1",
            help = "The IP address that the server binds to"
        )]
        ip: String,
        #[arg(
            long,
            default_value_t = 50051,
            help = "The port number that the server listens on"
        )]
        port: u16,
        #[arg(
            short = 'C',
            long,
            help = "The directory to change to before starting the server"
        )]
        directory: Option<String>,
    },
    #[command(
        about = "Start the PySpark shell with a Spark Connect server running in the background"
    )]
    Shell,
    #[command(about = "Start the Spark MCP (Model Context Protocol) server")]
    McpServer {
        #[arg(
            long,
            default_value = "127.0.0.1",
            help = "The host that the MCP server binds to (ignored for the stdio transport)"
        )]
        host: String,
        #[arg(
            long,
            default_value_t = 8000,
            help = "The port number that the server listens on (ignored for the stdio transport)"
        )]
        port: u16,
        #[arg(
            long,
            default_value_t = McpTransport::Sse,
            help = "The transport to use for the MCP server"
        )]
        transport: McpTransport,
        #[arg(long, help = "The Spark remote address to connect to (if specified)")]
        spark_remote: Option<String>,
        #[arg(
            short = 'C',
            long,
            help = "The directory to change to before starting the server"
        )]
        directory: Option<String>,
    },
}
```

Rust 語法解說：
- `#[arg(long)]`：長參數格式 `--ip`
- `#[arg(short = 'C')]`：短參數格式 `-C`
- `default_value = "127.0.0.1"`：字串預設值
- `default_value_t = 50051`：型別化預設值（`t` 代表 typed），會自動轉換為 `u16`
- `Option<String>`：可選參數，沒提供時為 `None`

```rust
pub fn main(args: Vec<String>) -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse_from(args);

    match cli.command {
        Command::Worker => run_worker(),
        Command::Spark(command) => match command {
            SparkCommand::Server { ip, port, directory } => {
                if let Some(directory) = directory {
                    std::env::set_current_dir(directory)?;
                }
                run_spark_connect_server(ip.parse()?, port)
            }
            SparkCommand::Shell => run_pyspark_shell(),
            SparkCommand::McpServer { host, port, transport, spark_remote, directory } => {
                if let Some(directory) = directory {
                    std::env::set_current_dir(directory)?;
                }
                run_spark_mcp_server(McpSettings { transport, host, port, spark_remote })
            }
        },
    }
}
```

Rust 語法解說：
- `Cli::parse_from(args)`：從參數列表解析 CLI 結構，clap 會自動處理
- `match cli.command`：模式匹配，Rust 要求處理所有變體（exhaustive）
- `SparkCommand::Server { ip, port, directory }`：解構 struct 變體，取出欄位
- `if let Some(directory) = directory`：條件解構，只有 `Some` 時才執行
  - `std::env::set_current_dir(directory)?`：切換工作目錄
- `ip.parse()?`：將字串解析為 `IpAddr` 型別，`?` 傳播錯誤

對於 `sail spark server --port 50051` 這個命令，會走到 `SparkCommand::Server` 分支，然後呼叫 `run_spark_connect_server(ip.parse()?, port)`。

### 第四層：run_spark_connect_server()

```rust
use std::net::IpAddr;
use std::sync::Arc;

use log::info;
use sail_common::config::AppConfig;
use sail_common::runtime::RuntimeManager;
use sail_spark_connect::entrypoint::{serve, SessionManagerOptions};
use sail_telemetry::telemetry::init_telemetry;
use tokio::net::TcpListener;
```

引入模組：
- `Arc`：Atomic Reference Counted，原子參考計數智慧指標，用於多執行緒安全的共享資料
- `log::info`：日誌巨集
- `AppConfig`：應用程式設定
- `RuntimeManager`：tokio 執行時管理器
- `serve`：實際的 gRPC 伺服器函數
- `init_telemetry`：初始化遙測系統
- `TcpListener`：tokio 的 TCP 監聽器

```rust
async fn shutdown() {
    let _ = tokio::signal::ctrl_c().await;
    info!("Shutting down the Spark Connect server...");
}
```

Rust 語法解說：
- `async fn`：異步函數，回傳 `impl Future<Output = ()>`
- `tokio::signal::ctrl_c()`：建立一個 Future，等待 Ctrl+C 訊號
- `.await`：等待 Future 完成
- `let _ = ...`：忽略回傳值（Result），避免編譯器未使用警告
- `info!(...)`：印 info 等級的日誌

這個函數會一直阻塞直到收到 Ctrl+C，然後印日誌。

```rust
pub fn run_spark_connect_server(ip: IpAddr, port: u16) -> Result<(), Box<dyn std::error::Error>> {
    init_telemetry()?;
```

呼叫 `init_telemetry()` 初始化遙測系統。

### 第五層：init_telemetry() (sail-telemetry/src/telemetry.rs)

```rust
use std::borrow::Cow;
use std::env;
use std::io::Write;

use fastrace::collector::{Config, Reporter, SpanRecord};
use fastrace::prelude::*;
use fastrace_opentelemetry::OpenTelemetryReporter;
use opentelemetry::InstrumentationScope;
use opentelemetry_otlp::{Protocol, WithExportConfig, OTEL_EXPORTER_OTLP_TIMEOUT_DEFAULT};
use opentelemetry_sdk::Resource;

use crate::error::TelemetryResult;
```

引入模組：
- `Cow`：Clone on Write，寫入時複製的智慧指標
- `fastrace`：分散式追蹤庫
- `opentelemetry`：OpenTelemetry 協議實作

```rust
pub fn init_telemetry() -> TelemetryResult<()> {
    let use_collector = match env::var("SAIL_OPENTELEMETRY_COLLECTOR") {
        Ok(val) => !val.is_empty(),
        Err(_) => false,
    };
    // Not getting any value out of this right now. Can re-enable when we revisit telemetry.
    // init_tracer(use_collector)?;
    init_logger(use_collector)?;
    Ok(())
}
```

Rust 語法解說：
- `env::var("...")`：讀取環境變數，回傳 `Result<String, VarError>`
- `match` 處理結果：
  - `Ok(val) => !val.is_empty()`：如果有值且不為空，`use_collector` 為 `true`
  - `Err(_) => false`：如果沒有這個環境變數，`use_collector` 為 `false`
- 目前只呼叫 `init_logger(use_collector)?`，tracer 被註解掉了

```rust
pub fn init_logger(use_collector: bool) -> TelemetryResult<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format(move |buf, record| {
            if use_collector {
                let event = Event::new(record.level().as_str()).with_properties(|| {
                    [("message", record.args().to_string())]
                });
                LocalSpan::add_event(event);
            }
            let level = record.level();
            let target = record.target();
            let style = buf.default_level_style(level);
            let timestamp = buf.timestamp();
            let args = record.args();
            if let Some(span_context) = SpanContext::current_local_parent() {
                let trace_id = span_context.trace_id.0;
                let span_id = span_context.span_id.0;
                writeln!(buf, "[{timestamp} {style}{level}{style:#} {target} trace: {trace_id} span: {span_id}] {args}")
            } else {
                writeln!(buf, "[{timestamp} {style}{level}{style:#} {target}] {args}")
            }
        })
        .init();
    Ok(())
}
```

Rust 語法解說：
- `env_logger::Builder::from_env(...)`：從環境變數建立日誌建構器
- `.default_filter_or("info")`：預設日誌等級為 `info`
- `.format(move |buf, record| { ... })`：自訂日誌格式
  - `move`：閉包捕獲 `use_collector` 的所有權
  - `buf`：輸出緩衝區
  - `record`：日誌記錄
- `writeln!(buf, "...")`：格式化輸出到緩衝區
- `{style}{level}{style:#}`：帶顏色的日誌等級（`{style:#}` 重設樣式）

這個函數初始化日誌系統，格式會包含時間戳、等級、目標和訊息。如果有 span context，還會包含 trace ID 和 span ID。

### 回到第四層：run_spark_connect_server()

```rust
    let config = Arc::new(AppConfig::load()?);
```

呼叫 `AppConfig::load()` 載入應用程式設定，並用 `Arc` 包裝以便共享。

### 第六層：AppConfig::load() (sail-common/src/config/application.rs)

```rust
const APP_CONFIG: &str = include_str!("application.yaml");
```

Rust 語法解說：
- `const`：編譯時常數
- `include_str!(...)`：編譯時巨集，將檔案內容嵌入為字串常數

這行會在編譯時把 `application.yaml` 的內容嵌入到二進位檔中。

```rust
#[derive(Debug, Clone, Deserialize)]
pub struct AppConfig {
    pub mode: ExecutionMode,
    pub runtime: RuntimeConfig,
    pub cluster: ClusterConfig,
    pub execution: ExecutionConfig,
    pub kubernetes: KubernetesConfig,
    pub parquet: ParquetConfig,
    pub catalog: CatalogConfig,
    pub optimizer: OptimizerConfig,
    pub spark: SparkConfig,
    #[serde(deserialize_with = "deserialize_unknown_unit")]
    pub internal: (),
}
```

這是應用程式設定的完整結構，包含所有子設定。

```rust
struct InternalConfigPlaceholder;

impl Provider for InternalConfigPlaceholder {
    fn metadata(&self) -> Metadata {
        Metadata::named("Internal")
    }

    fn data(&self) -> Result<Map<Profile, Dict>, Error> {
        Ok(Map::from([(
            Profile::Default,
            Dict::from([(
                "internal".to_string(),
                Value::Empty(Tag::Default, Empty::Unit),
            )]),
        )]))
    }
}
```

Rust 語法解說：
- `impl Provider for InternalConfigPlaceholder`：實作 figment 的 `Provider` trait
- `fn metadata(&self)`：提供元資訊
- `fn data(&self)`：提供設定資料
  - `Map::from([(...)])`：建立 map
  - `Value::Empty(Tag::Default, Empty::Unit)`：提供一個空的 unit 值給 `internal` 欄位

這個 Provider 的作用是注入一個佔位符給 `internal` 欄位，確保環境變數 `SAIL_INTERNAL_*` 不會被應用程式設定使用。

```rust
impl AppConfig {
    pub fn load() -> CommonResult<Self> {
        Figment::from(ConfigDefinition::new(APP_CONFIG))
            .merge(InternalConfigPlaceholder)
            .merge(Env::prefixed("SAIL_").map(|p| p.as_str().replace("__", ".").into()))
            .extract()
            .map_err(|e| CommonError::InvalidArgument(e.to_string()))
    }
}
```

Rust 語法解說：
- `Figment::from(ConfigDefinition::new(APP_CONFIG))`：從內嵌的 YAML 設定開始
- `.merge(InternalConfigPlaceholder)`：合併內部設定佔位符
- `.merge(Env::prefixed("SAIL_")...)`：合併環境變數（`SAIL_` 開頭）
  - `.map(|p| p.as_str().replace("__", ".").into())`：將雙底線替換為點（例如 `SAIL_RUNTIME__STACK_SIZE` 變成 `runtime.stack_size`）
- `.extract()`：提取並反序列化為 `AppConfig`

這個函數會載入三層設定：
1. 基礎層：`application.yaml` 的預設值
2. 內部層：`internal` 欄位佔位符
3. 環境變數層：`SAIL_*` 環境變數覆蓋預設值

### 回到第四層：run_spark_connect_server()

```rust
    let runtime = RuntimeManager::try_new(&config.runtime)?;
```

呼叫 `RuntimeManager::try_new()` 建立執行時管理器。

### 第七層：RuntimeManager::try_new() (sail-common/src/runtime.rs)

```rust
#[derive(Debug)]
pub struct RuntimeManager {
    primary: Runtime,
    io: Runtime,
    io_runtime_for_object_store: bool,
}
```

Rust 語法解說：
- `Runtime`：tokio 的執行時（runtime）
- `primary`：主執行時，用於 CPU 密集任務
- `io`：IO 執行時，用於 IO 密集任務
- `io_runtime_for_object_store`：是否為 object store 使用獨立執行時

```rust
impl RuntimeManager {
    pub fn try_new(config: &RuntimeConfig) -> CommonResult<Self> {
        let primary = Self::build_runtime(config.stack_size)?;
        let io = Self::build_runtime(config.stack_size)?;
        Ok(Self {
            primary,
            io,
            io_runtime_for_object_store: config.enable_secondary,
        })
    }
```

Rust 語法解說：
- `config: &RuntimeConfig`：借用 `RuntimeConfig` 的參考
- `Self::build_runtime(...)`：呼叫關聯函數建立執行時
- `config.stack_size`：執行緒堆疊大小（從設定檔讀取）

這個函數建立兩個獨立的 tokio 執行時。

```rust
    fn build_runtime(stack_size: usize) -> CommonResult<Runtime> {
        tokio::runtime::Builder::new_multi_thread()
            .thread_stack_size(stack_size)
            .enable_all()
            .build()
            .map_err(|e| CommonError::internal(e.to_string()))
    }
```

Rust 語法解說：
- `tokio::runtime::Builder::new_multi_thread()`：建立多執行緒執行時建構器
- `.thread_stack_size(stack_size)`：設定每個執行緒的堆疊大小
- `.enable_all()`：啟用所有功能（IO、時間等）
- `.build()`：建構執行時
- `.map_err(...)`：將錯誤轉換為 `CommonError`

```rust
    pub fn handle(&self) -> RuntimeHandle {
        let primary = self.primary.handle().clone();
        let io = self.io.handle().clone();
        let io_runtime_for_object_store = self.io_runtime_for_object_store;
        RuntimeHandle {
            primary,
            io,
            io_runtime_for_object_store,
        }
    }
}
```

Rust 語法解說：
- `self.primary.handle()`：取得執行時的 handle（可 clone 和跨執行緒傳遞）
- `.clone()`：複製 handle（內部是 `Arc`，所以很便宜）

這個方法回傳 `RuntimeHandle`，可以在不同執行緒間傳遞。

```rust
#[derive(Debug, Clone)]
pub struct RuntimeHandle {
    primary: Handle,
    io: Handle,
    io_runtime_for_object_store: bool,
}

impl RuntimeHandle {
    pub fn primary(&self) -> &Handle {
        &self.primary
    }

    pub fn io(&self) -> &Handle {
        &self.io
    }

    pub fn io_runtime_for_object_store(&self) -> bool {
        self.io_runtime_for_object_store
    }
}
```

`RuntimeHandle` 提供存取兩個執行時 handle 的方法。

### 回到第四層：run_spark_connect_server()

```rust
    let options = SessionManagerOptions {
        config: Arc::clone(&config),
        runtime: runtime.handle(),
    };
```

Rust 語法解說：
- `Arc::clone(&config)`：複製 `Arc`（只增加參考計數，不複製資料）
- `runtime.handle()`：取得 `RuntimeHandle`
- `SessionManagerOptions { ... }`：結構體初始化語法

建立 `SessionManagerOptions`，包含設定和執行時 handle。

```rust
    runtime.handle().primary().block_on(async {
        let listener = TcpListener::bind((ip, port)).await?;
        info!(
            "Starting the Spark Connect server on {}...",
            listener.local_addr()?
        );
        serve(listener, shutdown(), options).await?;
        info!("The Spark Connect server has stopped.");
        <Result<(), Box<dyn std::error::Error>>>::Ok(())
    })?;

    fastrace::flush();

    Ok(())
}
```

Rust 語法解說：
- `runtime.handle().primary()`：取得主執行時的 handle
- `.block_on(async { ... })`：阻塞當前執行緒，執行 async block
- `TcpListener::bind((ip, port))`：綁定 TCP 監聽器
  - `(ip, port)`：tuple 作為參數
  - `.await?`：等待綁定完成，失敗時提早回傳
- `listener.local_addr()?`：取得實際綁定的位址
- `serve(listener, shutdown(), options).await?`：啟動 gRPC 伺服器
  - `shutdown()`：呼叫 async 函數，回傳 Future
- `<Result<(), Box<dyn std::error::Error>>>::Ok(())`：Turbofish 語法明確指定型別
  - 因為 async block 的回傳型別需要明確，編譯器無法推斷
- `fastrace::flush()`：確保所有追蹤資料都已寫出

### 第八層：serve() (sail-spark-connect/src/entrypoint.rs)

```rust
use std::future::Future;

use sail_common::config::GRPC_MAX_MESSAGE_LENGTH_DEFAULT;
use sail_server::ServerBuilder;
use tokio::net::TcpListener;
use tonic::codec::CompressionEncoding;

use crate::server::SparkConnectServer;
use crate::session_manager::SessionManager;
pub use crate::session_manager::SessionManagerOptions;
use crate::spark::connect::spark_connect_service_server::SparkConnectServiceServer;
```

引入模組：
- `Future`：標準庫的 Future trait
- `ServerBuilder`：gRPC 伺服器建構器
- `tonic`：Rust 的 gRPC 框架
- `SparkConnectServer`：Spark Connect 服務實作
- `SessionManager`：會話管理器

```rust
pub async fn serve<F>(
    listener: TcpListener,
    signal: F,
    options: SessionManagerOptions,
) -> Result<(), Box<dyn std::error::Error>>
where
    F: Future<Output = ()>,
{
```

Rust 語法解說：
- `pub async fn serve<F>(...)`：泛型異步函數
  - `<F>`：泛型參數，代表 shutdown signal 的型別
- `where F: Future<Output = ()>`：trait bound，`F` 必須是輸出為 `()` 的 Future

#### 步驟 8.1：建立 SessionManager

```rust
let session_manager = SessionManager::new(options);
```

這行呼叫 `SessionManager::new(options)` 建立會話管理器實例。

🔸 位置：`crates/sail-spark-connect/src/session_manager.rs:60-67`

```rust
impl SessionManager {
    pub fn new(options: SessionManagerOptions) -> Self {
        let mut system = ActorSystem::new();
        let handle = system.spawn::<SessionManagerActor>(options);
        Self {
            system: Arc::new(Mutex::new(system)),
            handle,
        }
    }
}
```

Rust 語法解說：
- `ActorSystem::new()`：建立一個新的 Actor 系統
  - Actor 系統是基於訊息傳遞的並發模型
  - 所有 Actor 在單獨的任務中運行，透過訊息通訊
- `system.spawn::<SessionManagerActor>(options)`：在 Actor 系統中生成一個 `SessionManagerActor`
  - `spawn` 方法會建立一個 mpsc channel，並啟動 Actor 的事件迴圈
  - 回傳 `ActorHandle` 用於向 Actor 發送訊息
- `Arc::new(Mutex::new(system))`：包裝 Actor 系統
  - `Arc`：Atomic Reference Counted，多執行緒安全的共享指標
  - `Mutex`：互斥鎖，確保同時只有一個執行緒能存取

🔸 ActorSystem::spawn 詳解（位置：`crates/sail-server/src/actor.rs:125-135`）

```rust
pub fn spawn<T: Actor>(&mut self, options: T::Options) -> ActorHandle<T> {
    let (tx, rx) = mpsc::channel(ACTOR_CHANNEL_SIZE);
    let handle = ActorHandle { sender: tx };
    let runner = ActorRunner {
        actor: T::new(options),
        ctx: ActorContext::new(&handle),
        receiver: rx,
    };
    self.tasks.spawn(runner.run());
    handle
}
```

Rust 語法解說：
- `mpsc::channel(ACTOR_CHANNEL_SIZE)`：建立多生產者單消費者通道（channel）
  - `ACTOR_CHANNEL_SIZE = 8`：通道緩衝區大小
  - `tx`：發送端（Sender），可以 clone 多個
  - `rx`：接收端（Receiver），只能有一個
- `T::new(options)`：呼叫 `SessionManagerActor::new` 建立 Actor 實例
- `ActorContext::new(&handle)`：建立 Actor 上下文
  - 提供 `spawn`、`send`、`send_with_delay` 等方法
  - 管理 Actor 生成的子任務
- `self.tasks.spawn(runner.run())`：在 tokio 執行緒池中生成 Actor 事件迴圈
  - `runner.run()` 是一個 async 函數，會一直運行直到 Actor 停止
  - `self.tasks` 是一個 `JoinSet<()>`，追蹤所有生成的任務

🔸 ActorRunner::run 事件迴圈（位置：`crates/sail-server/src/actor.rs:185-206`）

```rust
async fn run(mut self) {
    self.actor.start(&mut self.ctx).await;
    while let Some(message) = self.receiver.recv().await {
        let action = self.actor.receive(&mut self.ctx, message);
        match action {
            ActorAction::Continue => {}
            ActorAction::Warn(message) => {
                log::warn!("{message}");
            }
            ActorAction::Fail(message) => {
                log::error!("{message}");
                break;
            }
            ActorAction::Stop => {
                break;
            }
        }
        self.ctx.reap();
    }
    self.actor.stop(&mut self.ctx).await;
}
```

Rust 語法解說：
- `self.actor.start(&mut self.ctx).await`：呼叫 Actor 的啟動鉤子（SessionManagerActor 沒有覆寫這個方法）
- `while let Some(message) = self.receiver.recv().await`：事件迴圈
  - `.recv().await` 會阻塞直到收到訊息
  - 如果通道關閉（所有 Sender 都 drop 了），會回傳 `None` 並結束迴圈
- `self.actor.receive(&mut self.ctx, message)`：處理訊息
  - 這是 `SessionManagerActor::receive` 方法
  - 回傳 `ActorAction` 決定下一步動作
- `self.ctx.reap()`：清理已完成的子任務
  - 使用 `JoinSet::try_join_next()` 檢查是否有任務完成
  - 記錄任何錯誤
- `self.actor.stop(&mut self.ctx).await`：呼叫停止鉤子

🔸 SessionManagerActor::new（位置：`crates/sail-spark-connect/src/session_manager.rs:323-331`）

```rust
fn new(options: Self::Options) -> Self {
    Self {
        options,
        sessions: HashMap::new(),
        global_file_listing_cache: None,
        global_file_statistics_cache: None,
        global_file_metadata_cache: None,
    }
}
```

建立空的會話管理器狀態：
- `options`：應用程式設定和執行時 handle
- `sessions: HashMap::new()`：空的會話表，儲存 `SessionKey -> SessionContext`
- 三個全局緩存都初始化為 `None`（延遲初始化，第一次建立 session 時才會建立）

#### 步驟 8.2：建立 SparkConnectServer

```rust
let server = SparkConnectServer::new(session_manager);
```

🔸 位置：`crates/sail-spark-connect/src/server.rs:29-33`

```rust
pub fn new(session_manager: SessionManager) -> Self {
    Self { session_manager }
}
```

這只是簡單地將 `SessionManager` 包裝到 `SparkConnectServer` 結構中。`SparkConnectServer` 實作了 `SparkConnectService` trait（由 tonic 從 protobuf 自動生成），提供所有 gRPC 方法的實現：
- `execute_plan`：執行查詢計劃
- `analyze_plan`：分析計劃（schema、explain 等）
- `config`：設定管理
- `add_artifacts`：上傳 UDF/JAR
- `interrupt`：中斷操作
- 等等

#### 步驟 8.3：建立 Tonic gRPC 服務

```rust
let service = SparkConnectServiceServer::new(server)
    .max_decoding_message_size(GRPC_MAX_MESSAGE_LENGTH_DEFAULT)
    .accept_compressed(CompressionEncoding::Gzip)
    .accept_compressed(CompressionEncoding::Zstd)
    .send_compressed(CompressionEncoding::Gzip)
    .send_compressed(CompressionEncoding::Zstd);
```

Rust 語法解說：
- `SparkConnectServiceServer::new(server)`：由 tonic 從 protobuf 自動生成的結構
  - 將我們的 `SparkConnectServer` 包裝成符合 tonic 規範的 gRPC 服務
  - 處理 HTTP/2 協議、訊息序列化/反序列化
- `.max_decoding_message_size(GRPC_MAX_MESSAGE_LENGTH_DEFAULT)`：限制接收訊息大小
  - `GRPC_MAX_MESSAGE_LENGTH_DEFAULT` 通常是 4MB 或 128MB（視設定）
  - 防止客戶端發送過大的訊息導致記憶體耗盡
- `.accept_compressed(CompressionEncoding::Gzip)`：接受 Gzip 壓縮的請求
  - 客戶端可以用 `grpc-encoding: gzip` header 發送壓縮訊息
- `.accept_compressed(CompressionEncoding::Zstd)`：接受 Zstandard 壓縮的請求
  - Zstd 通常比 Gzip 更快且壓縮率更高
- `.send_compressed(...)`：回應時使用壓縮
  - 伺服器會根據客戶端的 `grpc-accept-encoding` header 協商壓縮方式

這是 builder pattern，每個方法都回傳 `self`，可以鏈式呼叫。

#### 步驟 8.4：建立並配置 ServerBuilder

```rust
ServerBuilder::new("sail_spark_connect", Default::default())
    .add_service(service, Some(crate::spark::connect::FILE_DESCRIPTOR_SET))
    .await
    .serve(listener, signal)
    .await
```

🔸 ServerBuilder::new（位置：`crates/sail-server/src/builder.rs:47-79`）

```rust
pub fn new(name: &'static str, options: ServerBuilderOptions) -> Self {
    let (health_reporter, health_server) = tonic_health::server::health_reporter();

    let reflection_server_builder = tonic_reflection::server::Builder::configure()
        .register_encoded_file_descriptor_set(tonic_health::pb::FILE_DESCRIPTOR_SET);

    let layer = ServiceBuilder::new()
        .layer(TraceLayer::new(name))
        .into_inner();

    let router = tonic::transport::Server::builder()
        .tcp_nodelay(options.nodelay)
        .tcp_keepalive(options.keepalive)
        .http2_keepalive_interval(options.http2_keepalive_interval)
        .http2_keepalive_timeout(options.http2_keepalive_timeout)
        .http2_adaptive_window(options.http2_adaptive_window)
        .layer(layer)
        .add_service(health_server);

    Self {
        name,
        options,
        health_reporter,
        reflection_server_builder,
        router,
    }
}
```

這個建構器整合了多個 Tonic 功能：

1. **健康檢查服務**：`tonic_health::server::health_reporter()`
   - 實作 gRPC Health Checking Protocol
   - `health_reporter` 用於更新服務狀態（SERVING、NOT_SERVING）
   - `health_server` 是實際的 gRPC 服務，回應 `/grpc.health.v1.Health/Check` 請求

2. **反射服務建構器**：`tonic_reflection::server::Builder::configure()`
   - 實作 gRPC Server Reflection Protocol
   - 讓客戶端（如 grpcurl）可以查詢服務的 protobuf 定義
   - 註冊健康檢查服務的描述符

3. **追蹤層**：`TraceLayer::new(name)`
   - 使用 tower middleware 記錄請求/回應
   - 整合 OpenTelemetry 追蹤

4. **Tonic 伺服器**：`tonic::transport::Server::builder()`
   - TCP 和 HTTP/2 配置（nodelay、keepalive）
   - 添加追蹤層和健康檢查服務

🔸 add_service（位置：`crates/sail-server/src/builder.rs:81-100`）

```rust
pub async fn add_service<S>(mut self, service: S, file_descriptor_set: Option<&'b [u8]>) -> Self
where
    S: Service<Request<Body>, Error = Infallible> + NamedService + Clone + Send + Sync + 'static,
    S::Response: axum::response::IntoResponse,
    S::Future: Send + 'static,
{
    self.health_reporter.set_serving::<S>().await;
    if let Some(file_descriptor_set) = file_descriptor_set {
        self.reflection_server_builder = self
            .reflection_server_builder
            .register_encoded_file_descriptor_set(file_descriptor_set);
    }
    self.router = self.router.add_service(service);
    self
}
```

Rust 語法解說：
- `self.health_reporter.set_serving::<S>().await`：將服務標記為 SERVING 狀態
  - Turbofish 語法 `::<S>` 指定服務型別
  - 健康檢查端點會回傳這個狀態
- `register_encoded_file_descriptor_set(file_descriptor_set)`：註冊 protobuf 描述符
  - `FILE_DESCRIPTOR_SET` 是編譯 protobuf 時生成的二進位資料
  - 包含所有訊息、服務、方法的定義
  - 反射服務會用它來回答客戶端的查詢
- `self.router.add_service(service)`：將 Spark Connect 服務添加到路由器
  - 路由器會根據 gRPC 路徑（如 `/spark.connect.SparkConnectService/ExecutePlan`）分發請求

🔸 serve（位置：`crates/sail-server/src/builder.rs:102-125`）

```rust
pub async fn serve<F>(
    self,
    listener: TcpListener,
    signal: F,
) -> Result<(), Box<dyn std::error::Error>>
where
    F: Future<Output = ()>,
{
    let reflection_server = self.reflection_server_builder.build_v1()?;
    let router = self.router.add_service(reflection_server);

    let incoming = TcpIncoming::from(listener)
        .with_nodelay(Some(self.options.nodelay))
        .with_keepalive(self.options.keepalive);

    router
        .serve_with_incoming_shutdown(incoming, signal)
        .await?;

    Ok(())
}
```

Rust 語法解說：
- `self.reflection_server_builder.build_v1()?`：完成反射服務的建構
  - 使用 v1 版本的反射協議
  - 包含之前註冊的所有描述符
- `self.router.add_service(reflection_server)`：添加反射服務到路由器
  - 現在路由器包含三個服務：健康檢查、反射、Spark Connect
- `TcpIncoming::from(listener)`：將 tokio 的 `TcpListener` 轉換為 Tonic 的連線流
  - `.with_nodelay(Some(true))`：禁用 Nagle 演算法，減少延遲
  - `.with_keepalive(Some(60s))`：每 60 秒發送 TCP keepalive
- `router.serve_with_incoming_shutdown(incoming, signal).await?`：啟動伺服器
  - 開始監聽 TCP 連線
  - 對每個連線處理 HTTP/2 和 gRPC 請求
  - 當 `signal` Future 完成時（如 Ctrl+C），優雅關閉：
    1. 停止接受新連線
    2. 等待現有請求完成
    3. 關閉所有連線

#### 服務器啟動完成

此時，伺服器已經完全啟動並準備接收請求：

```
1. TCP 監聽器綁定到 127.0.0.1:50051
2. Actor 系統運行中，SessionManagerActor 等待訊息
3. gRPC 伺服器運行中，等待客戶端連線
4. 三個服務已註冊：
   - grpc.health.v1.Health（健康檢查）
   - grpc.reflection.v1.ServerReflection（反射）
   - spark.connect.SparkConnectService（Spark Connect）
5. 追蹤和日誌系統已初始化
6. 等待 shutdown 訊號（Ctrl+C）
```

當客戶端連線時：
1. Tonic 接受 TCP 連線並建立 HTTP/2 連線
2. 客戶端發送 gRPC 請求（如 `ExecutePlanRequest`）
3. 路由器根據路徑分發到對應的服務（`SparkConnectServer`）
4. 追蹤層記錄請求
5. 服務方法（如 `execute_plan`）被呼叫
6. 服務透過 `SessionManager` 的 `ActorHandle` 發送訊息給 `SessionManagerActor`
7. Actor 處理訊息（建立或取得 session）
8. 回應透過 gRPC 流式返回給客戶端

這個函數會一直運行直到收到 shutdown 訊號（Ctrl+C）。

---

## 🔸 完整調用鏈詳解：sail worker

Worker 是 Sail 分散式執行模式下的工作程序，由 Driver 透過 RPC 啟動。

### 第一層：runner::main() → run_worker()

當使用者執行 `sail worker` 命令時，`runner::main()` 會匹配到 `Command::Worker` 分支並呼叫 `run_worker()`。

### 第二層：run_worker() (sail-cli/src/worker/entrypoint.rs)

```rust
use sail_common::config::AppConfig;
use sail_common::runtime::RuntimeManager;
use sail_telemetry::telemetry::init_telemetry;

pub fn run_worker() -> Result<(), Box<dyn std::error::Error>> {
    init_telemetry()?;

    let config = AppConfig::load()?;
    let runtime = RuntimeManager::try_new(&config.runtime)?;
    runtime
        .handle()
        .primary()
        .block_on(sail_execution::run_worker(&config, runtime.handle()))?;

    fastrace::flush();

    Ok(())
}
```

這個函數的執行步驟：

1. **初始化遙測系統**：`init_telemetry()?`（已在 server 章節詳細說明）
2. **載入設定**：`AppConfig::load()?`（已在 server 章節詳細說明）
3. **建立執行時**：`RuntimeManager::try_new(&config.runtime)?`（已在 server 章節詳細說明）
4. **執行 Worker 主邏輯**：`block_on(sail_execution::run_worker(...))`
5. **刷新追蹤資料**：`fastrace::flush()`

### 第三層：sail_execution::run_worker() (sail-execution/src/worker/entrypoint.rs)

```rust
use sail_common::config::AppConfig;
use sail_common::runtime::RuntimeHandle;
use sail_server::actor::ActorSystem;

use crate::worker::{WorkerActor, WorkerOptions};

pub async fn run_worker(
    config: &AppConfig,
    runtime: RuntimeHandle,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut system = ActorSystem::new();
    let options = WorkerOptions::try_new(config, runtime)?;
    let _handle = system.spawn::<WorkerActor>(options);
    system.join().await;
    Ok(())
}
```

Rust 語法解說：
- `ActorSystem::new()`：建立新的 Actor 系統（與 SessionManager 使用相同的機制）
- `WorkerOptions::try_new(config, runtime)?`：建立 Worker 配置
  - 包含執行時 handle、網路配置、儲存配置等
- `system.spawn::<WorkerActor>(options)`：生成 WorkerActor
  - 建立 mpsc channel
  - 啟動 Actor 事件迴圈
  - 回傳 `ActorHandle`（這裡用 `_handle` 忽略，因為 Worker 不需要主動發送訊息）
- `system.join().await`：等待所有 Actor 停止
  - 這會阻塞直到 Worker 收到停止訊號
  - 當 Driver 發送停止訊息時，WorkerActor 會處理並結束

### WorkerActor 的職責

WorkerActor 在 Actor 事件迴圈中處理來自 Driver 的訊息：

1. **執行任務**：接收 Driver 發來的任務（Task），執行 DataFusion 物理計劃
2. **資料 Shuffle**：與其他 Worker 交換資料（map-side shuffle、reduce-side shuffle）
3. **回報狀態**：向 Driver 回報任務執行狀態（進度、完成、失敗）
4. **資源管理**：管理本地暫存資料、記憶體使用

Worker 通常不是由使用者手動啟動，而是：
- **LocalCluster 模式**：由 Driver 在本地啟動子程序
- **Kubernetes 模式**：由 Driver 透過 Kubernetes API 建立 Pod

---

## 🔸 完整調用鏈詳解：sail spark shell

Shell 是一個整合了 Spark Connect 服務和 Python REPL 的互動式環境。

### 第一層：runner::main() → run_pyspark_shell()

當使用者執行 `sail spark shell` 命令時，`runner::main()` 會匹配到 `SparkCommand::Shell` 分支並呼叫 `run_pyspark_shell()`。

### 第二層：run_pyspark_shell() (sail-cli/src/spark/shell.rs)

```rust
use std::net::Ipv4Addr;
use std::sync::Arc;

use pyo3::prelude::PyAnyMethods;
use pyo3::{PyResult, Python};
use sail_common::config::AppConfig;
use sail_common::runtime::RuntimeManager;
use sail_spark_connect::entrypoint::{serve, SessionManagerOptions};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

use crate::python::Modules;

pub fn run_pyspark_shell() -> Result<(), Box<dyn std::error::Error>> {
    let config = Arc::new(AppConfig::load()?);
    let runtime = RuntimeManager::try_new(&config.runtime)?;
    let options = SessionManagerOptions {
        config,
        runtime: runtime.handle(),
    };
    let (_tx, rx) = oneshot::channel::<()>();
    let handle = runtime.handle().primary().clone();
    let (server_port, server_task) = handle.block_on(async move {
        let listener = TcpListener::bind((Ipv4Addr::new(127, 0, 0, 1), 0)).await?;
        let port = listener.local_addr()?.port();
        let shutdown = async {
            let _ = rx.await;
        };
        let task = async {
            let _ = serve(listener, shutdown, options).await;
        };
        <Result<_, Box<dyn std::error::Error>>>::Ok((port, task))
    })?;
    handle.spawn(server_task);
    Python::attach(|py| -> PyResult<_> {
        let shell = Modules::SPARK_SHELL.load(py)?;
        shell
            .getattr("run_pyspark_shell")?
            .call((server_port,), None)?;
        Ok(())
    })?;
    Ok(())
}
```

這個函數結合了 Spark Connect 服務和 Python Shell，執行步驟如下：

#### 步驟 1：載入設定和建立執行時

```rust
let config = Arc::new(AppConfig::load()?);
let runtime = RuntimeManager::try_new(&config.runtime)?;
let options = SessionManagerOptions {
    config,
    runtime: runtime.handle(),
};
```

與 `run_spark_connect_server` 相同，載入設定和建立執行時。

#### 步驟 2：建立 shutdown channel

```rust
let (_tx, rx) = oneshot::channel::<()>();
```

Rust 語法解說：
- `oneshot::channel::<()>()`：建立一次性通道（只能發送一個值）
  - `_tx`：發送端（Sender），用底線前綴表示刻意不使用
  - `rx`：接收端（Receiver）
- 為什麼不使用 `_tx`？
  - Shell 不需要優雅關閉服務器
  - 當 Python REPL 退出時，整個程序會結束，服務器也會自動停止
  - `rx.await` 會永遠等待（因為 `_tx` 被 drop 了，通道會關閉但不會發送值）

#### 步驟 3：綁定隨機埠並建立服務器任務

```rust
let handle = runtime.handle().primary().clone();
let (server_port, server_task) = handle.block_on(async move {
    let listener = TcpListener::bind((Ipv4Addr::new(127, 0, 0, 1), 0)).await?;
    let port = listener.local_addr()?.port();
    let shutdown = async {
        let _ = rx.await;
    };
    let task = async {
        let _ = serve(listener, shutdown, options).await;
    };
    <Result<_, Box<dyn std::error::Error>>>::Ok((port, task))
})?;
```

Rust 語法解說：
- `Ipv4Addr::new(127, 0, 0, 1)`：建立 IPv4 位址 `127.0.0.1`（localhost）
  - 只監聽本地介面，不接受外部連線（安全性考量）
- `TcpListener::bind((127.0.0.1, 0))`：綁定到隨機埠
  - 埠號 `0` 表示讓作業系統自動分配可用埠
  - 避免與其他服務衝突
- `listener.local_addr()?.port()`：取得實際綁定的埠號
  - 需要傳遞給 Python Shell，讓 PySpark 連線到這個埠
- `let shutdown = async { let _ = rx.await; }`：建立永不完成的 Future
  - `rx.await` 會一直等待（因為 `_tx` 已經 drop）
  - 這個 Future 永遠不會 resolve，所以服務器會一直運行
- `let task = async { let _ = serve(listener, shutdown, options).await; }`：建立服務器任務
  - 注意這裡只是**建立** Future，還沒有執行
  - `serve` 函數與 `run_spark_connect_server` 中使用的相同

#### 步驟 4：在背景執行服務器

```rust
handle.spawn(server_task);
```

Rust 語法解說：
- `handle.spawn(...)`：在 tokio 執行時中生成任務
  - `server_task` 是一個 Future，會在背景執行
  - 不會阻塞當前執行緒
  - 服務器會在背景持續運行，同時可以執行 Python Shell

這裡與 `run_spark_connect_server` 的關鍵差異：
- **Server 模式**：`block_on(serve(...))`，主執行緒阻塞等待服務器
- **Shell 模式**：`spawn(serve(...))`，背景執行服務器，主執行緒繼續執行 Python Shell

#### 步驟 5：啟動 Python Shell

```rust
Python::attach(|py| -> PyResult<_> {
    let shell = Modules::SPARK_SHELL.load(py)?;
    shell
        .getattr("run_pyspark_shell")?
        .call((server_port,), None)?;
    Ok(())
})?;
```

Rust 語法解說：
- `Python::attach(|py| { ... })`：附加到已初始化的 Python 直譯器
  - `py` 是 Python 執行上下文（GIL token）
  - 閉包內的程式碼會持有 Python 的 Global Interpreter Lock (GIL)
- `Modules::SPARK_SHELL.load(py)?`：載入內嵌的 Python 模組
  - `SPARK_SHELL` 是常數，定義在 `python.rs`

🔸 Modules::SPARK_SHELL 定義（位置：`crates/sail-cli/src/python.rs:42-45`）

```rust
pub const SPARK_SHELL: Module<()> = Module::new(
    "_sail_cli_spark_shell",
    include_str!("python/spark_shell.py"),
);
```

Rust 語法解說：
- `Module::new(...)`：建立模組定義
  - 第一個參數：模組名稱 `_sail_cli_spark_shell`
  - 第二個參數：模組源碼（透過 `include_str!` 巨集嵌入）
- `include_str!("python/spark_shell.py")`：編譯時將 Python 檔案內容嵌入為字串
  - 這樣 Sail 二進位檔不需要外部 Python 檔案就能運行
- `Module<()>`：泛型參數 `()` 表示沒有額外的初始化器
  - `NativeLogging` 使用 `Module<NativeLogging>` 因為需要註冊 Python class

🔸 Module::load 方法（位置：`crates/sail-cli/src/python.rs:23-32`）

```rust
pub fn load<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyModule>> {
    let m = PyModule::from_code(
        py,
        CString::new(self.source)?.as_c_str(),
        CString::new("")?.as_c_str(),
        CString::new(self.name)?.as_c_str(),
    )?;
    I::init(&m)?;
    Ok(m)
}
```

Rust 語法解說：
- `PyModule::from_code(...)`：從 Python 源碼建立模組
  - 第一個參數：Python 上下文
  - 第二個參數：Python 源碼（C 字串）
  - 第三個參數：檔案名稱（空字串，因為是內嵌模組）
  - 第四個參數：模組名稱
- `I::init(&m)?`：呼叫初始化器
  - 對於 `Module<()>`，這是空操作
  - 對於 `Module<NativeLogging>`，會註冊 `NativeLogging` class

載入模組後，繼續執行：

```rust
shell
    .getattr("run_pyspark_shell")?
    .call((server_port,), None)?;
```

Rust 語法解說：
- `.getattr("run_pyspark_shell")?`：取得 Python 函數
  - 等同於 Python 的 `shell.run_pyspark_shell`
- `.call((server_port,), None)?`：呼叫函數
  - 第一個參數：位置參數 tuple `(server_port,)`
    - 注意尾隨逗號，表示單元素 tuple
  - 第二個參數：關鍵字參數（`None` 表示沒有）
  - 等同於 Python 的 `shell.run_pyspark_shell(server_port)`

### 第三層：run_pyspark_shell() (Python 程式碼)

🔸 位置：`crates/sail-cli/src/python/spark_shell.py:10-29`

```python
import code
import platform
import readline
from rlcompleter import Completer

import pyspark
from pyspark.sql import SparkSession


def run_pyspark_shell(port: int):
    spark = SparkSession.builder.remote(f"sc://localhost:{port}").getOrCreate()
    namespace = {"spark": spark}
    readline.parse_and_bind("tab: complete")
    readline.set_completer(Completer(namespace).complete)

    python_version = platform.python_version()
    (build_number, build_date) = platform.python_build()
    banner = rf"""Welcome to
      ____              __
     / __/__  ___ _____/ /__
    _\ \/ _ \/ _ `/ __/  '_/
   /__ / .__/\_,_/_/ /_/\_\   version {pyspark.__version__}
      /_/

Using Python version {python_version} ({build_number}, {build_date})
Client connected to the Sail Spark Connect server at localhost:{port}
SparkSession available as 'spark'."""
    code.interact(local=namespace, banner=banner, exitmsg="")
```

這段 Python 程式碼的執行流程：

1. **建立 SparkSession**：`SparkSession.builder.remote(f"sc://localhost:{port}").getOrCreate()`
   - 連線到背景運行的 Spark Connect 服務器
   - `sc://` 是 Spark Connect 協議的 URL scheme
   - 這會建立 gRPC 連線到 `localhost:{port}`

2. **設定命名空間**：`namespace = {"spark": spark}`
   - Shell 中可用的變數
   - 使用者可以直接使用 `spark` 變數

3. **啟用 Tab 自動完成**：
   ```python
   readline.parse_and_bind("tab: complete")
   readline.set_completer(Completer(namespace).complete)
   ```
   - 使用 `readline` 和 `rlcompleter` 提供 Tab 自動完成
   - 可以按 Tab 鍵補全 `spark.` 的屬性和方法

4. **顯示歡迎訊息**：建立 PySpark 風格的 banner
   - 顯示 PySpark 版本、Python 版本
   - 提示連線到 Sail Spark Connect 服務器

5. **啟動互動式 REPL**：`code.interact(local=namespace, banner=banner, exitmsg="")`
   - `code.interact` 是 Python 標準庫的互動式 REPL
   - `local=namespace`：提供 `spark` 變數
   - `banner=banner`：顯示歡迎訊息
   - `exitmsg=""`：退出時不顯示訊息
   - 使用者可以輸入 Python 程式碼，例如：
     ```python
     >>> spark.sql("SELECT 1+1").show()
     +-------+
     |(1 + 1)|
     +-------+
     |      2|
     +-------+
     ```

### Shell 模式的完整架構

```
┌─────────────────────────────────────────────────────────┐
│                     sail spark shell                     │
│                                                           │
│  ┌───────────────────────────────────────────────────┐  │
│  │              Tokio Runtime (Background)           │  │
│  │                                                    │  │
│  │  ┌──────────────────────────────────────────┐   │  │
│  │  │    Spark Connect gRPC Server             │   │  │
│  │  │    - SessionManager (Actor)              │   │  │
│  │  │    - TCP Listener (127.0.0.1:random)     │   │  │
│  │  └──────────────────────────────────────────┘   │  │
│  └───────────────────────────────────────────────────┘  │
│                          ↑ gRPC                          │
│                          │                               │
│  ┌───────────────────────────────────────────────────┐  │
│  │         Python REPL (Foreground)                  │  │
│  │                                                    │  │
│  │  >>> spark.sql("SELECT 1")                        │  │
│  │  ┌──────────────────────────────────────────┐   │  │
│  │  │   PySpark Client (SparkSession)          │   │  │
│  │  │   - gRPC client to sc://localhost:port   │   │  │
│  │  └──────────────────────────────────────────┘   │  │
│  └───────────────────────────────────────────────────┘  │
└─────────────────────────────────────────────────────────┘
```

當使用者執行 `spark.sql("SELECT 1+1").show()` 時：
1. PySpark 客戶端將 SQL 封裝為 `ExecutePlanRequest` gRPC 訊息
2. 透過 localhost 的 gRPC 連線發送到背景的 Spark Connect 服務器
3. 服務器處理請求（解析 SQL、執行查詢、產生結果）
4. 結果透過 gRPC 流式返回給 PySpark 客戶端
5. PySpark 將結果格式化並顯示在 REPL 中

當使用者按 Ctrl+D 或輸入 `exit()` 時：
1. `code.interact()` 退出
2. Python 函數回傳
3. Rust 的 `run_pyspark_shell()` 函數結束
4. 整個程序退出（背景的服務器任務也會被終止）

---

## 🔸 簡單範例：SELECT 1+1

當你執行 `sail spark server` 並用 PySpark 連線執行 `SELECT 1+1` 時，完整流程如下：

```
1. 啟動伺服器：sail spark server --port 50051
   │
   ├─ main.rs::main()
   │  ├─ rustls::crypto::aws_lc_rs::default_provider().install_default()
   │  ├─ CliConfig::load()
   │  │  └─ Figment 從環境變數 SAIL_INTERNAL__RUN_PYTHON 載入設定
   │  ├─ Python::initialize()
   │  └─ runner::main(args)
   │     ├─ Cli::parse_from(args) 解析命令列參數
   │     └─ run_spark_connect_server(ip, port)
   │
   ├─ run_spark_connect_server()
   │  ├─ init_telemetry()
   │  │  ├─ env::var("SAIL_OPENTELEMETRY_COLLECTOR") 檢查環境變數
   │  │  └─ init_logger() 初始化日誌系統
   │  ├─ AppConfig::load()
   │  │  └─ Figment 從 application.yaml 和環境變數載入設定
   │  ├─ RuntimeManager::try_new()
   │  │  ├─ build_runtime() 建立 primary 執行時
   │  │  └─ build_runtime() 建立 io 執行時
   │  ├─ runtime.handle().primary().block_on(async { ... })
   │  │  ├─ TcpListener::bind((ip, port)) 綁定 127.0.0.1:50051
   │  │  └─ serve(listener, shutdown(), options)
   │  │     ├─ SessionManager::new() 建立會話管理器
   │  │     ├─ SparkConnectServer::new() 建立 Spark Connect 伺服器
   │  │     ├─ SparkConnectServiceServer::new() 建立 gRPC 服務
   │  │     └─ ServerBuilder::new().serve() 啟動伺服器
   │  │
   │  └─ fastrace::flush() 確保追蹤資料寫出
   │
   └─ 伺服器運行中，等待連線...

2. PySpark 連線
   spark = SparkSession.builder.remote("sc://localhost:50051").getOrCreate()
   │
   └─ gRPC 建立連線到 127.0.0.1:50051
      └─ SessionManager 建立新的 Session

3. 執行查詢
   spark.sql("SELECT 1+1").show()
   │
   ├─ PySpark 將 SQL 透過 gRPC ExecutePlanRequest 發送
   ├─ SparkConnectServer 接收請求
   ├─ sail-sql-parser 解析 SQL "SELECT 1+1"
   ├─ sail-plan 轉換為 DataFusion 邏輯計劃
   ├─ DataFusion 執行查詢（local mode）
   ├─ 結果以 Arrow RecordBatch 格式產生
   └─ 透過 gRPC ExecutePlanResponse 回傳給 PySpark
      └─ PySpark 顯示結果：
          +-------+
          |(1 + 1)|
          +-------+
          |      2|
          +-------+
```

---
