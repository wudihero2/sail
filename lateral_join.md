# Lateral Join 實現分析

## 🔸 什麼是 Lateral Join？

Lateral Join 允許右側子查詢引用左側的列：

```sql
-- 右側的 subquery 可以使用左側 t1 的列
SELECT * FROM t1
LEFT OUTER JOIN LATERAL (SELECT c2 FROM t2 WHERE t1.c1 = t2.c1) s
```

對比普通 join：
- 普通 join：左右兩側是獨立的，不能互相引用
- Lateral join：右側可以「看到」左側的每一行

## 🔸 目前的狀態（2025 年 5 月更新）

| 層級 | 狀態 |
|-----|------|
| Spark Connect Proto | ✅ 已定義 `LateralJoin` message |
| Sail Proto 轉換 | ❌ 返回 `SparkError::todo("lateral join")` |
| Sail Spec 層 | ❌ 沒有對應類型 |
| Sail Resolver | ❌ 未實現 |
| DataFusion | ⚠️ 部分支持（PR #16015 已 merge） |

## 🔸 DataFusion PR #16015 深度分析

DataFusion 在 2025 年 5 月 16 日 merge 了 PR #16015，實現了基本的 lateral join 支持。

### 為什麼選擇 Optimizer Rule 而不是新增 Physical Operator？

實現 lateral join 有兩種主要方式：

| 方式 | 說明 | 優缺點 |
|-----|------|--------|
| 新增 Physical Operator | 實現一個 `LateralJoinExec`，對左側每一行重新執行右側 | 簡單直接，但無法並行，效能差 |
| Optimizer Rule 轉換 | 把 lateral join 轉換成標準 join | 複雜，但可利用現有優化和並行執行 |

DataFusion 選擇了 Optimizer Rule，原因：

1. 符合 DataFusion 的架構：DataFusion 的 join operators 設計上左右兩側是獨立執行的
2. 可並行：轉換後的標準 join 可以利用現有的 hash join、merge join 等並行算法
3. 可優化：轉換後的 plan 可以經過其他 optimizer rules（如 filter pushdown）

### PR #16015 修改的 6 個檔案

```
datafusion/optimizer/
├── src/
│   ├── decorrelate.rs                    # 修改：支援 scalar aggregation 追蹤
│   ├── decorrelate_lateral_join.rs       # 新增：核心轉換邏輯
│   ├── lib.rs                            # 修改：export 新 module
│   └── optimizer.rs                      # 修改：加入優化 pipeline
│
datafusion/sqllogictest/test_files/
├── explain.slt                           # 修改：更新 explain 輸出測試
└── join.slt.part                         # 修改：新增 lateral join 測試案例
```

各檔案的作用：

| 檔案 | 類型 | 作用 |
|-----|------|------|
| `decorrelate.rs` | 修改 | 新增 `pulled_up_scalar_agg` flag，追蹤 scalar aggregation 是否被轉成 group aggregation |
| `decorrelate_lateral_join.rs` | 新增 | 核心！實現 `DecorrelateLateralJoin` rule，把 lateral join 轉成標準 join |
| `lib.rs` | 修改 | 一行修改，export 新 module |
| `optimizer.rs` | 修改 | 把新 rule 加入優化 pipeline，放在 scalar subquery 轉換之後 |
| `explain.slt` | 修改 | 測試 EXPLAIN 輸出是否包含新的優化階段 |
| `join.slt.part` | 修改 | 新增 lateral join 的功能測試案例 |

### 為什麼需要修改 decorrelate.rs？

關鍵問題：scalar aggregation 的特殊處理。

```sql
SELECT * FROM t0, LATERAL (SELECT sum(v1) FROM t1 WHERE t0.v0 = t1.v0)
```

這個 subquery 是 scalar aggregation（沒有 GROUP BY，保證返回一行）。轉換時需要：

1. 把 `sum(v1)` 轉成 `GROUP BY t1.v0` 的 aggregation
2. 記住這是從 scalar aggregation 轉來的
3. 用 LEFT JOIN 而不是 INNER JOIN（確保左側沒有匹配時也返回一行 NULL）

`decorrelate.rs` 新增的 `pulled_up_scalar_agg` flag 就是用來追蹤這個狀態。

### 實現方式：Decorrelation

新增了 `DecorrelateLateralJoin` optimizer rule，把 lateral join 轉換成標準 join：

```
┌─────────────────────────────────────────────────────────────┐
│ 原始 Lateral Join                                           │
│                                                             │
│   SELECT * FROM t0, LATERAL (                               │
│       SELECT sum(v1) FROM t1 WHERE t0.v0 = t1.v0            │
│   )                                                         │
└─────────────────────────────────────────────────────────────┘
                            │
                            ▼  DecorrelateLateralJoin
┌─────────────────────────────────────────────────────────────┐
│ 轉換後的標準 Join                                           │
│                                                             │
│   SELECT * FROM t0                                          │
│   LEFT JOIN (SELECT v0, sum(v1) FROM t1 GROUP BY v0) sub    │
│   ON t0.v0 = sub.v0                                         │
└─────────────────────────────────────────────────────────────┘
```

### 轉換邏輯

1. 識別含有 outer references 的 INNER JOIN
2. 使用 `PullUpCorrelatedExpr` 提取 correlation conditions
3. 把 correlation predicates 變成 join condition
4. 決定 join 類型：
   - 如果右側是 scalar aggregate（保證返回一行）→ 轉成 LEFT JOIN
   - 否則保持 INNER JOIN

### DataFusion 目前的限制

| 支持 | 不支持 |
|-----|--------|
| INNER lateral join | LEFT/RIGHT/FULL lateral join |
| 簡單 correlation predicate | 複雜的 non-equal conditions |
| 單層 outer reference | 多層嵌套 lateral |
| 基本 aggregation | COUNT(*) 有 bug |

---

## 🔸 Spark Connect 的 LateralJoin 協議

```protobuf
// relations.proto:1240-1252
message LateralJoin {
  Relation left = 1;      // 左側 relation
  Relation right = 2;     // 右側 relation（可引用左側的列）
  Expression join_condition = 3;  // 可選的 join 條件
  Join.JoinType join_type = 4;    // INNER, LEFT, RIGHT, FULL 等
}
```

PySpark 用法：

```python
t1.lateralJoin(
    t2.where(sf.col("t1.c1").outer() == sf.col("t2.c1"))
      .select(sf.col("c2"))
      .alias("s"),
    how="left"
)
```

關鍵：`.outer()` 標記該列來自外層（左側）。

---

## 🔸 Sail 實現策略（更新）

由於 DataFusion 已經實現了基礎的 decorrelation，Sail 的策略需要調整：

### 新策略：利用 DataFusion + 補充缺失功能

```
┌────────────────────────────────────────────────────────────────┐
│                        Sail 實現架構                            │
├────────────────────────────────────────────────────────────────┤
│                                                                │
│  Spark LateralJoin Proto                                       │
│         │                                                      │
│         ▼                                                      │
│  Sail Spec Layer (QueryNode::LateralJoin)                      │
│         │                                                      │
│         ▼                                                      │
│  Sail Resolver ─────────────────────────────────┐              │
│         │                                        │              │
│         ▼                                        ▼              │
│  ┌──────────────────┐                   ┌─────────────────┐    │
│  │ INNER join_type  │                   │ LEFT/RIGHT/FULL │    │
│  │ (簡單 case)      │                   │ (複雜 case)     │    │
│  └────────┬─────────┘                   └────────┬────────┘    │
│           │                                      │              │
│           ▼                                      ▼              │
│  DataFusion LogicalPlan                 Sail 自行 decorrelate   │
│  (LATERAL syntax)                       或返回 unsupported      │
│           │                                                     │
│           ▼                                                     │
│  DecorrelateLateralJoin                                        │
│  (DataFusion optimizer)                                        │
│                                                                │
└────────────────────────────────────────────────────────────────┘
```

### 實現階段

#### 階段 1：Proto → Spec → DataFusion（簡單 case）

對於 INNER lateral join，直接生成 DataFusion 的 LATERAL 語法：

```rust
// 如果是 INNER join，可以直接用 DataFusion 的 lateral join 支持
if join_type == JoinType::Inner {
    // 生成 DataFusion LogicalPlan with LATERAL
    // DataFusion 的 DecorrelateLateralJoin 會處理轉換
}
```

涉及檔案：
```
crates/sail-spark-connect/src/proto/plan.rs     # Proto → Spec 轉換
crates/sail-common/src/spec/plan.rs             # 定義 QueryNode::LateralJoin
crates/sail-plan/src/resolver/query/mod.rs      # dispatch
crates/sail-plan/src/resolver/query/lateral_join.rs  # 新檔案
```

#### 階段 2：LEFT/RIGHT/FULL lateral join

DataFusion 目前不支持 outer lateral join，需要 Sail 自行處理：

選項 A：等待 DataFusion 支持（追蹤 issue #10048 後續）
選項 B：在 Sail resolver 層做 decorrelation
選項 C：返回 unsupported 錯誤

建議先用選項 C，等 DataFusion 支持後再升級。

---

## 🔸 實現步驟

### Step 1：定義 Spec 類型

```rust
// crates/sail-common/src/spec/plan.rs

pub enum QueryNode {
    // ... 現有的 ...

    /// Lateral join - right side can reference left side columns
    LateralJoin {
        left: Box<QueryPlan>,
        right: Box<QueryPlan>,
        join_type: JoinType,
        condition: Option<Expr>,
    },
}
```

### Step 2：Proto → Spec 轉換

```rust
// crates/sail-spark-connect/src/proto/plan.rs

RelType::LateralJoin(lj) => {
    let sc::LateralJoin { left, right, join_condition, join_type } = *lj;

    let left = (*left.required("lateral join left")?).try_into()?;
    let right = (*right.required("lateral join right")?).try_into()?;
    let join_type = convert_join_type(join_type)?;
    let condition = join_condition.map(|c| c.try_into()).transpose()?;

    Ok(RelationNode::Query(spec::QueryNode::LateralJoin {
        left: Box::new(left),
        right: Box::new(right),
        join_type,
        condition,
    }))
}
```

### Step 3：Resolver dispatch

```rust
// crates/sail-plan/src/resolver/query/mod.rs

QueryNode::LateralJoin { left, right, join_type, condition } => {
    self.resolve_query_lateral_join(*left, *right, join_type, condition, state)
        .await?
}
```

### Step 4：Resolver 實現

```rust
// crates/sail-plan/src/resolver/query/lateral_join.rs

impl PlanResolver<'_> {
    pub(super) async fn resolve_query_lateral_join(
        &self,
        left: spec::QueryPlan,
        right: spec::QueryPlan,
        join_type: spec::JoinType,
        condition: Option<spec::Expr>,
        state: &mut PlanResolverState,
    ) -> PlanResult<LogicalPlan> {
        // 1. 檢查 join type
        let df_join_type = match join_type {
            spec::JoinType::Inner => datafusion::JoinType::Inner,
            // DataFusion 目前只支持 INNER lateral join
            _ => return Err(PlanError::unsupported(
                "only INNER lateral join is currently supported"
            )),
        };

        // 2. Resolve 左側
        let left_plan = self.resolve_query_plan(left, state).await?;

        // 3. 進入 lateral scope（讓右側可以看到左側的 schema）
        let mut lateral_scope = state.enter_lateral_scope(left_plan.schema().clone());

        // 4. Resolve 右側（會產生 OuterReferenceColumn）
        let right_plan = self.resolve_query_plan(right, lateral_scope.state()).await?;

        // 5. Resolve condition（如果有）
        let join_condition = if let Some(cond) = condition {
            Some(self.resolve_expr(cond, lateral_scope.state()).await?)
        } else {
            None
        };

        // 6. 構建 DataFusion LogicalPlan
        // 注意：需要確認 DataFusion 的 API 如何表達 lateral join
        build_lateral_join_plan(left_plan, right_plan, df_join_type, join_condition)
    }
}
```

---

## 🔸 已有的相關基礎設施

Sail 已經有處理 outer references 的機制：

| 機制 | 位置 | 用途 |
|-----|------|-----|
| `OuterReferenceColumn` | `resolver/expression/attribute.rs` | 標記引用外層的列 |
| `enter_query_scope` | `resolver/state.rs` | 管理 scope 層次 |
| Subquery resolution | `resolver/expression/subquery.rs` | 處理 IN/EXISTS/Scalar subquery |

這些可以直接復用。

---

## 🔸 測試案例

從 `pyspark/sql/tests/test_subquery.py` 提取的測試案例：

```python
# Case 1: 基本 lateral join（應該可以支持）
t1.lateralJoin(t2.where(sf.col("t1.c1").outer() == sf.col("t2.c1")))

# Case 2: 多層 lateral（可能需要特殊處理）
t1.lateralJoin(t2.lateralJoin(spark.range(1).select(sf.col("c1").outer())))

# Case 3: lateral join 夾在普通 join 中間（應該可以支持）
t1.lateralJoin(...).join(t3, ...)

# Case 4: chained lateral（後面的引用前面的結果）
t1, LATERAL (SELECT c1 + c2 AS a), LATERAL (SELECT a * 2 AS b)

# Case 5: LEFT lateral join（DataFusion 不支持，需要返回 unsupported）
t1.lateralJoin(..., how="left")
```

---

## 🔸 總結

| 項目 | 說明 |
|-----|------|
| 難度 | 中（有 DataFusion 支持後變簡單） |
| 依賴 | DataFusion 57+ （含 PR #16015） |
| 限制 | 只支持 INNER lateral join |
| 風險 | LEFT/RIGHT/FULL 需要等待或自行實現 |

實現順序：
1. Proto → Spec 轉換（移除 `todo!` 錯誤）
2. 基本 resolver（INNER lateral join）
3. 測試 + 迭代
4. 追蹤 DataFusion 對 outer lateral join 的支持

---

## 🔸 待確認事項

1. DataFusion 的 LogicalPlan API 如何表達 lateral join？
   - 需要查看 `datafusion/expr/src/logical_plan/plan.rs`

2. Sail 目前使用的 DataFusion 版本是否包含 PR #16015？
   - 需要檢查 `Cargo.toml` 中的 datafusion 版本

3. DataFusion 的 `DecorrelateLateralJoin` 是否自動啟用？
   - 需要確認 optimizer rules 配置

---

Sources:
- [DataFusion Lateral Join Issue #10048](https://github.com/apache/datafusion/issues/10048)
- [DataFusion PR #16015: Support simple/cross lateral joins](https://github.com/apache/datafusion/pull/16015)
