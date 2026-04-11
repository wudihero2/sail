# rustfmt.toml 詳細解說

這份文件配置 Rust 程式碼格式化工具 rustfmt。

## 完整設定

```toml
imports_granularity = "Module"
group_imports = "StdExternalCrate"
```

## 設定說明

### 🔸 imports_granularity = "Module"

控制 use 語句的合併粒度:

```rust
// 預設 "Preserve" - 保持原樣
use std::collections::HashMap;
use std::collections::HashSet;

// "Module" - 按模組合併
use std::collections::{HashMap, HashSet};

// "Crate" - 按 crate 合併
use std::{
    collections::{HashMap, HashSet},
    io::{Read, Write},
};

// "Item" - 每個 item 一行
use std::collections::HashMap;
use std::collections::HashSet;
```

Sail 使用 "Module" 級別，同一模組的 import 會合併，但不同模組分開，平衡可讀性和簡潔性。

### 🔸 group_imports = "StdExternalCrate"

控制 import 分組和排序:

```rust
// "StdExternalCrate" 分三組，空行分隔:

// 1. 標準庫
use std::collections::HashMap;
use std::sync::Arc;

// 2. 外部 crate
use arrow::array::ArrayRef;
use tokio::sync::Mutex;

// 3. 本地 crate (workspace 成員)
use sail_common::error::Result;
use sail_plan::resolver::PlanResolver;
```

其他選項:
- "Preserve": 保持原樣
- "One": 所有 import 放一組

## 使用方式

```bash
# 格式化單一檔案
rustfmt src/main.rs

# 格式化整個專案
cargo fmt

# 檢查格式 (不修改)
cargo fmt -- --check
```
