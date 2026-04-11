# clippy.toml 詳細解說

這份文件配置 Rust linter Clippy 的進階設定，包含閾值調整和禁用特定 API。

## 基本設定

```toml
avoid-breaking-exported-api = false
```

🔸 預設 Clippy 會避免建議可能破壞公開 API 的修改
🔸 設為 false 因為 Sail 是應用程式而非函式庫，不需要維護 API 相容性

```toml
too-many-arguments-threshold = 8
```

🔸 函數參數超過此數量會觸發 `too_many_arguments` 警告
🔸 預設是 7，這裡放寬到 8

```toml
large-error-threshold = 264
```

🔸 Result<T, E> 中 E 超過此 bytes 會觸發 `result_large_err`
🔸 調高是因為 tonic 和 DataFusion 的錯誤型別較大
🔸 相關 issues 已在註解中標註

## 禁用的 DataFusion 型別

```toml
disallowed-types = [
    { path = "datafusion_catalog::table::TableProviderFactory" },
    { path = "datafusion_datasource::file_format::FileFormatFactory" },
    { path = "datafusion_expr::logical_plan::DdlStatement" },
    { path = "datafusion_expr::logical_plan::DescribeTable" },
    { path = "datafusion_expr::logical_plan::DmlStatement" },
    { path = "datafusion_expr::logical_plan::Statement" },
    { path = "datafusion_expr::logical_plan::dml::CopyTo" },
]
```

🔸 禁止直接使用這些 DataFusion 型別
🔸 原因: Sail 有自己的 Spark 相容實作，應使用 Sail 版本而非 DataFusion 原生版本
🔸 使用這些型別會觸發編譯錯誤

## 禁用的 DataFusion 方法

```toml
disallowed-methods = [
    # SessionContext 方法
    { path = "datafusion::execution::context::SessionContext::catalog" },
    { path = "datafusion::execution::context::SessionContext::catalog_names" },
    { path = "datafusion::execution::context::SessionContext::sql" },
    { path = "datafusion::execution::context::SessionContext::table" },
    { path = "datafusion::execution::context::SessionContext::register_table" },
    ...

    # SessionState 方法
    { path = "datafusion::execution::session_state::SessionState::catalog_list" },
    { path = "datafusion::execution::session_state::SessionState::create_logical_plan" },
    { path = "datafusion::execution::session_state::SessionState::sql_to_statement" },
    ...

    # LogicalPlanBuilder 方法
    { path = "datafusion_expr::logical_plan::LogicalPlanBuilder::insert_into" },
    { path = "datafusion_expr::logical_plan::LogicalPlanBuilder::prepare" },
]
```

🔸 禁止直接呼叫 DataFusion 的這些方法
🔸 原因:

| 分類 | 原因 |
|------|------|
| Catalog 相關 | Sail 有自己的 catalog 管理層 |
| SQL 解析 | Sail 使用自己的 Spark SQL parser |
| Table 操作 | Sail 需要 Spark 語義的 table 處理 |
| Plan 建構 | Sail 有自己的 plan 轉換邏輯 |

🔸 應使用 sail-plan、sail-session 等 crate 提供的對應方法

## 為什麼需要這些限制

```
                    ┌─────────────────────┐
                    │   PySpark Client    │
                    └──────────┬──────────┘
                               │ Spark SQL
                               ▼
┌──────────────────────────────────────────────────────┐
│                     Sail Layer                        │
│  ┌─────────────────┐    ┌─────────────────────────┐  │
│  │ sail-sql-parser │    │ sail-plan (Spark 語義)  │  │
│  │ (Spark SQL)     │    │                         │  │
│  └────────┬────────┘    └────────────┬────────────┘  │
│           │                          │               │
│           └──────────┬───────────────┘               │
│                      ▼                               │
│         ┌────────────────────────┐                   │
│         │   Sail Catalog Layer   │  ← 必須經過這層   │
│         └───────────┬────────────┘                   │
└─────────────────────┼────────────────────────────────┘
                      ▼
          ┌────────────────────────┐
          │      DataFusion        │  ← 不能直接使用
          │  (底層執行引擎)         │
          └────────────────────────┘
```

直接使用 DataFusion API 會:
- 繞過 Spark SQL 語法解析
- 繞過 Spark 語義轉換
- 繞過 Sail 的 catalog 管理
- 導致行為與 Spark 不一致

## 使用方式

```bash
# 執行 Clippy 檢查
cargo clippy --all-targets

# 修正可自動修復的問題
cargo clippy --fix

# 檢查特定 crate
cargo clippy -p sail-plan
```
