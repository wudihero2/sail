# 從 array_insert 學習如何為 Sail 添加 Spark 函數

為 Sail 添加新的 Spark 函數並不複雜，但需要理解整個流程。本文以 PR #638 為例，解釋如何實作 `array_insert` 函數，讓你未來也能輕鬆為 Sail 擴充功能。

## 為什麼需要手動實作函數？

Sail 是基於 DataFusion 建構的 Spark-compatible 引擎。雖然 DataFusion 提供了許多內建函數，但 Spark 的函數語義往往與 DataFusion 不同。因此 Sail 需要：

🔸 **語義轉換層**：將 Spark 函數語義轉換成 DataFusion 的執行邏輯

🔸 **相容性保證**：確保行為與 Spark 完全一致，包括邊界情況

🔸 **錯誤處理**：Spark 和 DataFusion 的錯誤處理方式不同

---

# 添加函數的完整流程

整個流程只需要修改兩個地方：

```
1. 實作函數邏輯 (crates/sail-plan/src/function/scalar/array.rs)
   |
   +-- 定義函數行為
   +-- 處理邊界情況
   +-- 註冊到函數列表

2. 更新測試預期 (crates/sail-spark-connect/tests/gold_data/function/array.json)
   |
   +-- 將狀態從 "not implemented" 改為 "success: ok"
```

---

# 步驟 1：實作函數邏輯

## 🔸 找到對應的函數分類檔案

所有 scalar 函數都放在 `crates/sail-plan/src/function/scalar/` 目錄下，依照類別分類：

```
scalar/
├── array.rs         # 陣列相關函數
├── string.rs        # 字串函數
├── math.rs          # 數學函數
├── datetime.rs      # 日期時間函數
├── map.rs           # Map 函數
└── ...
```

`array_insert` 屬於陣列操作，所以要在 `array.rs` 中實作。

---

## 🔸 實作函數本體

在 `crates/sail-plan/src/function/scalar/array.rs:101-161` 中，完整實作如下：

```rust
fn array_insert(
    array: expr::Expr,
    position: expr::Expr,
    value: expr::Expr,
) -> PlanResult<expr::Expr> {
    let array_len = cast(expr_fn::array_length(array.clone()), DataType::Int64);

    let pos_from_zero = when(position.clone().gt(lit(0)), position.clone() - lit(1))
        .when(
            position.clone().lt(lit(0)),
            array_len.clone() + position + lit(1),
        )
        .end()?;

    let zero_index_error = ScalarUDF::from(RaiseError::new()).call(vec![lit(
        "array_insert: the index 0 is invalid. An index shall be either < 0 or > 0 (the first element has index 1)"
    )]);

    Ok(when(array.clone().is_null(), array.clone())
        .when(pos_from_zero.clone().is_null(), zero_index_error)
        .when(
            pos_from_zero.clone().lt(lit(0)),
            expr_fn::array_concat(vec![
                expr_fn::array_repeat(value.clone(), lit(1)),
                expr_fn::array_repeat(lit(ScalarValue::Null), -pos_from_zero.clone()),
                array.clone(),
            ]),
        )
        .when(
            pos_from_zero.clone().eq(lit(0)),
            expr_fn::array_prepend(value.clone(), array.clone()),
        )
        .when(
            pos_from_zero
                .clone()
                .between(lit(1), array_len.clone() - lit(1)),
            expr_fn::array_concat(vec![
                expr_fn::array_slice(array.clone(), lit(1), pos_from_zero.clone(), None),
                expr_fn::array_repeat(value.clone(), lit(1)),
                expr_fn::array_slice(
                    array.clone(),
                    pos_from_zero.clone() + lit(1),
                    array_len.clone(),
                    None,
                ),
            ]),
        )
        .when(
            pos_from_zero.clone().eq(array_len.clone()),
            expr_fn::array_append(array.clone(), value.clone()),
        )
        .when(
            pos_from_zero.clone().gt(array_len.clone()),
            expr_fn::array_concat(vec![
                array.clone(),
                expr_fn::array_repeat(lit(ScalarValue::Null), pos_from_zero - array_len),
                expr_fn::array_repeat(value, lit(1)),
            ]),
        )
        .end()?)
}
```

### 程式碼解釋

🔸 **函數簽名**
```rust
fn array_insert(
    array: expr::Expr,      // 輸入陣列
    position: expr::Expr,   // 插入位置（Spark 是 1-based）
    value: expr::Expr,      // 要插入的值
) -> PlanResult<expr::Expr>
```

這是一個三參數函數，返回類型是 `PlanResult<expr::Expr>`，代表可能失敗的 DataFusion 表達式。

🔸 **索引轉換（Spark 1-based → 0-based）**
```rust
let array_len = cast(expr_fn::array_length(array.clone()), DataType::Int64);

let pos_from_zero = when(position.clone().gt(lit(0)), position.clone() - lit(1))
    .when(
        position.clone().lt(lit(0)),
        array_len.clone() + position + lit(1),
    )
    .end()?;
```

Spark 的陣列索引是從 1 開始，而 DataFusion 是從 0 開始。這段程式碼處理：
- 正數索引：`position - 1`（例如 position=1 變成 0）
- 負數索引：從陣列末尾往前算（例如 position=-1 是最後一個位置）

🔸 **錯誤處理：index 0 不合法**
```rust
let zero_index_error = ScalarUDF::from(RaiseError::new()).call(vec![lit(
    "array_insert: the index 0 is invalid. An index shall be either < 0 or > 0 (the first element has index 1)"
)]);
```

在 Spark 中，索引 0 是不合法的，因為索引從 1 開始（正數）或從 -1 開始（負數）。如果使用者傳入 0，就會觸發錯誤。

🔸 **各種插入情境**

整個函數用 `when().when().when()...` 的條件鏈處理所有情境：

**情境 1：陣列為 null**
```rust
when(array.clone().is_null(), array.clone())
```
直接返回 null。

**情境 2：position 為 0（不合法）**
```rust
.when(pos_from_zero.clone().is_null(), zero_index_error)
```
拋出錯誤訊息。

**情境 3：負數索引超出範圍（在陣列前面插入）**
```rust
.when(
    pos_from_zero.clone().lt(lit(0)),
    expr_fn::array_concat(vec![
        expr_fn::array_repeat(value.clone(), lit(1)),           // 插入的值
        expr_fn::array_repeat(lit(ScalarValue::Null), -pos_from_zero.clone()), // 填充 null
        array.clone(),                                          // 原陣列
    ]),
)
```
例如：`array_insert([1,2,3], -5, 99)` → `[99, null, 1, 2, 3]`

**情境 4：插入到最前面**
```rust
.when(
    pos_from_zero.clone().eq(lit(0)),
    expr_fn::array_prepend(value.clone(), array.clone()),
)
```
例如：`array_insert([1,2,3], 1, 99)` → `[99, 1, 2, 3]`

**情境 5：插入到陣列中間**
```rust
.when(
    pos_from_zero
        .clone()
        .between(lit(1), array_len.clone() - lit(1)),
    expr_fn::array_concat(vec![
        expr_fn::array_slice(array.clone(), lit(1), pos_from_zero.clone(), None),
        expr_fn::array_repeat(value.clone(), lit(1)),
        expr_fn::array_slice(
            array.clone(),
            pos_from_zero.clone() + lit(1),
            array_len.clone(),
            None,
        ),
    ]),
)
```
例如：`array_insert([1,2,3,4], 3, 99)` → `[1, 2, 99, 3, 4]`

這裡使用 `array_slice` 切割陣列，然後用 `array_concat` 拼接：
- 前半段：`[1, 2]`
- 插入值：`[99]`
- 後半段：`[3, 4]`

**情境 6：插入到最後**
```rust
.when(
    pos_from_zero.clone().eq(array_len.clone()),
    expr_fn::array_append(array.clone(), value.clone()),
)
```
例如：`array_insert([1,2,3], 4, 99)` → `[1, 2, 3, 99]`

**情境 7：索引超出範圍（在陣列後面插入）**
```rust
.when(
    pos_from_zero.clone().gt(array_len.clone()),
    expr_fn::array_concat(vec![
        array.clone(),
        expr_fn::array_repeat(lit(ScalarValue::Null), pos_from_zero - array_len),
        expr_fn::array_repeat(value, lit(1)),
    ]),
)
```
例如：`array_insert([1,2,3], 6, 99)` → `[1, 2, 3, null, null, 99]`

---

## 🔸 註冊函數到函數列表

在 `crates/sail-plan/src/function/scalar/array.rs:219-253` 的 `list_built_in_array_functions()` 中加入：

```rust
pub(super) fn list_built_in_array_functions() -> Vec<(&'static str, ScalarFunction)> {
    use crate::function::common::ScalarFunctionBuilder as F;

    vec![
        ("array", F::udf(SparkArray::new())),
        ("array_append", F::binary(array_append)),
        // ... 其他函數
        ("array_insert", F::ternary(array_insert)),  // <-- 新增這一行
        // ... 更多函數
    ]
}
```

🔸 **ScalarFunctionBuilder 的工作原理**

`F::ternary(array_insert)` 是一個包裝器，它會：

1. 驗證參數數量（必須是 3 個）
2. 從參數列表中提取 `(first, second, third)`
3. 呼叫你的 `array_insert(first, second, third)` 函數
4. 將結果轉換成統一的錯誤處理格式

其他可用的 builder：
- `F::nullary(f)` - 無參數函數
- `F::unary(f)` - 單參數函數
- `F::binary(f)` - 雙參數函數
- `F::ternary(f)` - 三參數函數
- `F::quaternary(f)` - 四參數函數
- `F::var_arg(f)` - 可變參數函數
- `F::udf(impl)` - 自訂 UDF 實作（更複雜的情況）

這些 builder 定義在 `crates/sail-plan/src/function/common.rs:40-129`。

---

# 步驟 2：更新測試預期

測試檔案位於 `crates/sail-spark-connect/tests/gold_data/function/array.json`。

🔸 **修改前**
```json
{
  "input": {
    "query": "SELECT array_insert(array(1, 2, 3, 4), 5, 5);",
    "result": ["[1,2,3,4,5]"],
    "schema": { ... }
  },
  "output": {
    "error": "not implemented: function: array_insert"
  }
}
```

🔸 **修改後**
```json
{
  "input": {
    "query": "SELECT array_insert(array(1, 2, 3, 4), 5, 5);",
    "result": ["[1,2,3,4,5]"],
    "schema": { ... }
  },
  "output": {
    "success": "ok"
  }
}
```

總共有 3 個 `array_insert` 測試案例需要更新：
- 正數索引測試：`array_insert(array(1, 2, 3, 4), 5, 5)`
- 負數索引測試：`array_insert(array(5, 3, 2, 1), -4, 4)`
- 負數插入到最後：`array_insert(array(5, 4, 3, 2), -1, 1)`

---

# 函數註冊的完整調用鏈

讓我們追蹤函數是如何被註冊和使用的：

```
1. list_built_in_array_functions()
   (crates/sail-plan/src/function/scalar/array.rs:219)
   |
   v
2. list_built_in_scalar_functions()
   (crates/sail-plan/src/function/scalar/mod.rs:23)
   |
   v
3. 註冊到 Sail 的函數註冊表
   (用於 SQL 解析時查找函數)
   |
   v
4. PySpark 客戶端發送 "SELECT array_insert(...)"
   |
   v
5. Sail 解析 SQL，找到 array_insert 函數
   |
   v
6. 呼叫你實作的 array_insert() 函數
   |
   v
7. 轉換成 DataFusion 的表達式樹
   |
   v
8. DataFusion 執行並返回結果
```

---

# 實作函數的關鍵 Rust 語法解釋

因為你是 Rust 新手，讓我解釋程式碼中的關鍵語法：

## 🔸 `expr::Expr` 是什麼？

```rust
fn array_insert(
    array: expr::Expr,
    position: expr::Expr,
    value: expr::Expr,
) -> PlanResult<expr::Expr>
```

`expr::Expr` 是 DataFusion 的表達式類型，代表一個**尚未執行**的計算。它是一個抽象語法樹（AST）節點。

例如：
- `lit(1)` → 字面值 1 的表達式
- `array.clone()` → 引用陣列的表達式
- `position + lit(1)` → 加法表達式

這些表達式會在查詢執行時才被計算。

## 🔸 `.clone()` 為什麼這麼多？

```rust
when(array.clone().is_null(), array.clone())
```

在 Rust 中，變數預設會被**移動**（move），不能重複使用。但 `Expr` 實作了 `Clone` trait，所以可以複製。

因為 `array` 要在多個 `when` 條件中使用，所以需要 `.clone()` 來複製它。

## 🔸 `when().when().end()?` 是什麼模式？

```rust
when(condition1, result1)
    .when(condition2, result2)
    .when(condition3, result3)
    .end()?
```

這是 DataFusion 的 **builder pattern**，用來建構 SQL 的 `CASE WHEN` 表達式：

```sql
CASE
    WHEN condition1 THEN result1
    WHEN condition2 THEN result2
    WHEN condition3 THEN result3
END
```

最後的 `.end()?` 做兩件事：
1. `.end()` 完成 builder 並返回 `Result<Expr>`
2. `?` 是錯誤傳播運算子，如果有錯誤就提前返回

## 🔸 `lit()` 是什麼？

```rust
lit(1)
lit(ScalarValue::Null)
lit("error message")
```

`lit()` 是 "literal" 的縮寫，用來建立字面值表達式：
- `lit(1)` → 整數 1
- `lit(false)` → 布林值 false
- `lit(ScalarValue::Null)` → SQL 的 NULL

## 🔸 `expr_fn::` 是什麼？

```rust
expr_fn::array_concat(...)
expr_fn::array_slice(...)
expr_fn::array_repeat(...)
```

這些是 DataFusion 提供的內建函數，以 Rust 函數的形式封裝。你可以像搭積木一樣組合它們來實作更複雜的邏輯。

---

# 總結：添加新函數的 Checklist

要為 Sail 添加新的 Spark 函數，你需要：

🔸 **步驟 1：確定函數類別**
- 找到對應的檔案（array.rs, string.rs, math.rs 等）

🔸 **步驟 2：實作函數邏輯**
```rust
fn your_function(arg1: expr::Expr, arg2: expr::Expr) -> PlanResult<expr::Expr> {
    // 使用 when().when().end()? 處理各種情況
    // 使用 expr_fn:: 的內建函數組合邏輯
    // 處理 null 值和錯誤情況
}
```

🔸 **步驟 3：註冊函數**
```rust
pub(super) fn list_built_in_xxx_functions() -> Vec<(&'static str, ScalarFunction)> {
    vec![
        ("your_function", F::binary(your_function)),  // 根據參數數量選擇 builder
        // ...
    ]
}
```

🔸 **步驟 4：更新測試**
- 在 `tests/gold_data/function/xxx.json` 中
- 將 `"error": "not implemented"` 改為 `"success": "ok"`

🔸 **步驟 5：執行測試**
```bash
cargo nextest run -p sail-spark-connect
```

---

# 常見的實作模式

## 🔸 處理 null 值

```rust
when(input.is_null(), lit(ScalarValue::Null))
    .when(condition, result)
    .end()?
```

## 🔸 拋出錯誤

```rust
let error = ScalarUDF::from(RaiseError::new()).call(vec![lit("error message")]);
when(error_condition, error)
```

## 🔸 類型轉換

```rust
cast(expr, DataType::Int64)
cast(expr, DataType::Utf8)
```

## 🔸 條件判斷

```rust
expr.gt(lit(0))          // expr > 0
expr.lt(lit(0))          // expr < 0
expr.eq(lit(0))          // expr == 0
expr.is_not_null()       // expr IS NOT NULL
expr.between(a, b)       // expr BETWEEN a AND b
```

## 🔸 使用 DataFusion 內建函數

```rust
expr_fn::array_length(array)
expr_fn::array_concat(vec![a, b, c])
expr_fn::array_slice(array, start, end, None)
expr_fn::coalesce(vec![expr1, expr2])
```

---

# 下一步

現在你已經理解了如何為 Sail 添加函數，可以嘗試：

🔸 **找一個 "not implemented" 的函數**
```bash
grep -r "not implemented" crates/sail-spark-connect/tests/gold_data/
```

🔸 **參考類似函數的實作**
- 看看 `array_append`, `array_prepend` 是怎麼實作的
- 參考同類別函數的錯誤處理方式

🔸 **閱讀 Spark 官方文件**
- 確認函數的正確語義
- 注意邊界情況和錯誤處理

🔸 **測試驅動開發**
- 先寫測試案例
- 再實作函數邏輯
- 確保所有測試通過
