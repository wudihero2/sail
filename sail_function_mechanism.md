# Sail 函數機制解析

以 `format_string` bug 為例，解釋 Sail 如何將 Spark 函數轉換成 DataFusion 執行。

## 🔸 整體架構

```
┌─────────────────────────────────────────────────────────────────────────┐
│                         Sail 函數處理流程                                │
├─────────────────────────────────────────────────────────────────────────┤
│                                                                         │
│  PySpark Client                                                         │
│       │                                                                 │
│       ▼  format_string("Hello %d %s", 100, "days")                      │
│  ┌─────────────────────────────────────────────────────────────────┐    │
│  │ Spark Connect Proto                                             │    │
│  │ UnresolvedFunction { name: "format_string", args: [...] }       │    │
│  └─────────────────────────────────────────────────────────────────┘    │
│       │                                                                 │
│       ▼  Proto → Spec                                                   │
│  ┌─────────────────────────────────────────────────────────────────┐    │
│  │ Sail Spec Layer                                                 │    │
│  │ spec::Expr::UnresolvedFunction { name, arguments }              │    │
│  └─────────────────────────────────────────────────────────────────┘    │
│       │                                                                 │
│       ▼  Spec → DataFusion (這裡查找 BUILT_IN_SCALAR_FUNCTIONS)          │
│  ┌─────────────────────────────────────────────────────────────────┐    │
│  │ PlanResolver                                                    │    │
│  │ get_built_in_function("format_string") → ScalarFunction         │    │
│  │ 調用 ScalarFunction(arguments) → datafusion_expr::Expr           │    │
│  └─────────────────────────────────────────────────────────────────┘    │
│       │                                                                 │
│       ▼                                                                 │
│  ┌─────────────────────────────────────────────────────────────────┐    │
│  │ DataFusion LogicalPlan                                          │    │
│  │ Expr::ScalarFunction { func: FormatStringFunc, args: [...] }    │    │
│  └─────────────────────────────────────────────────────────────────┘    │
│                                                                         │
└─────────────────────────────────────────────────────────────────────────┘
```

## 🔸 核心元件：ScalarFunctionBuilder

位置：`crates/sail-plan/src/function/common.rs`

`ScalarFunctionBuilder`（別名 `F`）提供多種方式來註冊函數：

```rust
// 類型定義
pub(crate) type ScalarFunction =
    Arc<dyn Fn(ScalarFunctionInput) -> PlanResult<expr::Expr> + Send + Sync>;

// ScalarFunctionInput 包含
pub struct ScalarFunctionInput<'a> {
    pub arguments: Vec<expr::Expr>,      // 解析後的參數
    pub function_context: FunctionContextInput<'a>,
}
```

### 各種 Builder 方法

| 方法 | 參數數量 | 說明 |
|-----|---------|------|
| `F::nullary(f)` | 0 | 無參數函數，如 `current_date()` |
| `F::unary(f)` | 1 | 單參數函數，如 `abs(x)` |
| `F::binary(f)` | 2 | 雙參數函數，如 `concat(a, b)` |
| `F::ternary(f)` | 3 | 三參數函數 |
| `F::quaternary(f)` | 4 | 四參數函數 |
| `F::var_arg(f)` | 任意 | 可變參數，如 `coalesce(a, b, c, ...)` |
| `F::udf(impl)` | 由 UDF 定義 | 使用 DataFusion UDF |
| `F::custom(f)` | 自訂 | 完全自訂邏輯 |
| `F::unknown(name)` | - | 未實現，返回 todo 錯誤 |

### 源碼解析

```rust
// F::binary 的實現
pub fn binary<F, R>(f: F) -> ScalarFunction
where
    F: Fn(expr::Expr, expr::Expr) -> R + Send + Sync + 'static,
    R: IntoPlanResult<expr::Expr>,
{
    Arc::new(
        move |ScalarFunctionInput { arguments, .. }| {
            let (left, right) = arguments.two()?;  // 強制取 2 個參數
            f(left, right).into_plan_result()
        },
    )
}

// F::var_arg 的實現
pub fn var_arg<F, R>(f: F) -> ScalarFunction
where
    F: Fn(Vec<expr::Expr>) -> R + Send + Sync + 'static,
    R: IntoPlanResult<expr::Expr>,
{
    Arc::new(
        move |ScalarFunctionInput { arguments, .. }| {
            f(arguments).into_plan_result()  // 直接傳遞所有參數
        },
    )
}

// F::udf 的實現
pub fn udf<F>(f: F) -> ScalarFunction
where
    F: ScalarUDFImpl + Send + Sync + 'static,
{
    let func = ScalarUDF::from(f);  // 包裝成 DataFusion UDF
    Arc::new(
        move |ScalarFunctionInput { arguments, .. }| {
            Ok(func.call(arguments))  // UDF 自己處理參數驗證
        },
    )
}
```

## 🔸 函數註冊表

位置：`crates/sail-plan/src/function/mod.rs`

```rust
lazy_static! {
    pub static ref BUILT_IN_SCALAR_FUNCTIONS: HashMap<&'static str, ScalarFunction> =
        HashMap::from_iter(scalar::list_built_in_scalar_functions());
}

pub fn get_built_in_function(name: &str) -> PlanResult<ScalarFunction> {
    Ok(BUILT_IN_SCALAR_FUNCTIONS
        .get(name)
        .or_else(|| BUILT_IN_GENERATOR_FUNCTIONS.get(name))
        .ok_or_else(|| PlanError::unsupported(format!("unknown function: {name}")))?
        .clone())
}
```

## 🔸 函數註冊示例

位置：`crates/sail-plan/src/function/scalar/string.rs`

```rust
pub(super) fn list_built_in_string_functions() -> Vec<(&'static str, ScalarFunction)> {
    use crate::function::common::ScalarFunctionBuilder as F;

    vec![
        // 單參數函數
        ("length", F::unary(expr_fn::length)),

        // 雙參數函數
        ("repeat", F::binary(expr_fn::repeat)),

        // 可變參數函數
        ("coalesce", F::var_arg(expr_fn::coalesce)),
        ("concat", F::var_arg(expr_fn::concat)),

        // UDF（DataFusion 或 datafusion-spark 提供）
        ("split", F::udf(SparkSplit::new())),

        // 自訂邏輯
        ("substr", F::custom(substring)),

        // 未實現
        ("printf", F::unknown("printf")),
    ]
}
```

## 🔸 format_string Bug 分析

### 問題

```rust
// 原本的註冊（錯誤）
("format_string", F::binary(string_fn::format_string)),
```

`F::binary` 內部調用 `arguments.two()?`，強制要求剛好 2 個參數：

```rust
// ItemTaker trait 的 two() 方法
fn two(self) -> PlanResult<(T, T)> {
    let [first, second] = self.take()?;
    Ok((first, second))
}

fn take<const N: usize>(self) -> PlanResult<[T; N]> {
    let items: Vec<T> = self.into();
    items.try_into().map_err(|v: Vec<T>| {
        // 這就是錯誤訊息的來源！
        PlanError::invalid(format!("{N} values expected: {v:?}"))
    })
}
```

當傳入 3 個參數時：
```
format_string("Hello %d %s", 100, "days")
              ─────────────────────────
              3 個參數，但 binary 要求 2 個
```

錯誤訊息：
```
two values expected: [Literal(Utf8("Hello World %d %s"), None),
                      Literal(Int32(100), None),
                      Literal(Utf8("days"), None)]
```

### 修復

```rust
// 修復後的註冊
use datafusion_spark::function::string::format_string::FormatStringFunc;

("format_string", F::udf(FormatStringFunc::new())),
```

`FormatStringFunc` 是 `datafusion-spark` 提供的 Spark 相容實現：

```rust
// datafusion-spark 的實現
impl FormatStringFunc {
    pub fn new() -> Self {
        Self {
            signature: Signature::new(
                TypeSignature::VariadicAny,  // 接受任意數量參數
                Volatility::Immutable
            ),
            aliases: vec![String::from("printf")],
        }
    }
}
```

## 🔸 為什麼 F::udf 可以處理可變參數？

```rust
pub fn udf<F>(f: F) -> ScalarFunction
where
    F: ScalarUDFImpl + Send + Sync + 'static,
{
    let func = ScalarUDF::from(f);
    Arc::new(
        move |ScalarFunctionInput { arguments, .. }| {
            Ok(func.call(arguments))  // 直接傳給 UDF
        },
    )
}
```

關鍵：`F::udf` 不做參數數量檢查，而是把所有 arguments 直接傳給 `ScalarUDF::call()`。

參數驗證由 UDF 的 `Signature` 在 DataFusion planning 階段處理：

```rust
// DataFusion 的 Signature 驗證
Signature::new(TypeSignature::VariadicAny, ...)
// VariadicAny = 接受任意數量、任意類型的參數
```

## 🔸 函數分類

| 類型 | 來源 | 使用方式 |
|-----|------|---------|
| DataFusion 內建 | `datafusion::functions::expr_fn` | `F::unary(expr_fn::abs)` |
| datafusion-spark | `datafusion_spark::function` | `F::udf(SparkFunc::new())` |
| Sail 自訂 | `sail_function::scalar` | `F::udf(SailFunc::new())` |
| 簡單包裝 | 各處 | `F::custom(\|input\| { ... })` |

## 🔸 總結

```
┌─────────────────────────────────────────────────────────────────────────┐
│                    選擇正確的 Builder 方法                               │
├─────────────────────────────────────────────────────────────────────────┤
│                                                                         │
│  函數有固定參數數量？                                                    │
│      │                                                                  │
│      ├─ 是，0 個 ──→ F::nullary(f)                                      │
│      ├─ 是，1 個 ──→ F::unary(f)                                        │
│      ├─ 是，2 個 ──→ F::binary(f)                                       │
│      ├─ 是，3 個 ──→ F::ternary(f)                                      │
│      └─ 否，可變 ──→ F::var_arg(f) 或 F::udf(impl)                      │
│                                                                         │
│  需要 Spark 相容語義？                                                   │
│      │                                                                  │
│      ├─ 是 ──→ 優先用 datafusion-spark 的 UDF                           │
│      └─ 否 ──→ 用 DataFusion 原生函數                                   │
│                                                                         │
│  需要特殊處理（schema 存取、型別轉換）？                                  │
│      │                                                                  │
│      ├─ 是 ──→ F::custom(f)                                             │
│      └─ 否 ──→ 用簡單的 Builder                                         │
│                                                                         │
└─────────────────────────────────────────────────────────────────────────┘
```

---

Sources:
- `crates/sail-plan/src/function/common.rs` - ScalarFunctionBuilder 定義
- `crates/sail-plan/src/function/mod.rs` - 函數註冊表
- `crates/sail-plan/src/function/scalar/string.rs` - 字串函數註冊
- `datafusion-spark` crate - Spark 相容函數實現
