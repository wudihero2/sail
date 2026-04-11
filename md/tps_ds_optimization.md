# Sail TPC-DS 效能優化：Join Reorder 與 DPHyp 演算法

本文整理 Sail 在 TPC-DS benchmark 效能優化方面的工作，涵蓋問題背景（Issue #747、#759）、解決方案（PR #810、#917），以及核心論文 Moerkotte & Neumann 的 DPHyp 演算法原理與 Sail 的完整實作。

# 問題背景

## Issue #747：Delta Lake 查詢崩潰

TPC-DS sf=1 benchmark 跑在 Delta Lake 表上直接 stack overflow 崩潰。

根因是 DeltaScan 這個自訂 physical plan node 沒被 DataFusion 優化器認得，導致 predicate pushdown、projection pruning 等最佳化完全失效，查詢計畫膨脹到無法執行。

修復方式是 PR #750，移除了 DeltaScan wrapper，讓 DataFusion 原生掃描節點直接處理 Delta 表。

## Issue #759：TPC-DS 查詢效能低落

修完崩潰後，許多 TPC-DS 查詢雖然能跑但極慢。以 query 72 為例，Sail 比 DuckDB 慢了數個數量級。

分析原因如下：

```
DuckDB 的執行計畫（query 72 簡化版）：
┌──────────────────┐
│  Final Aggregate  │
│  + HashJoin (5表) │    <-- 先做 predicate pushdown，再做 join reorder
│    + Filter       │        各表先過濾掉大量資料，再以最佳順序 join
│      + Scan       │
└──────────────────┘

Sail 的執行計畫（query 72 簡化版）：
┌──────────────────┐
│  Final Aggregate  │
│  + HashJoin       │    <-- 照 SQL 寫的順序逐一 join
│    + HashJoin     │        沒有 join reorder
│      + HashJoin   │        大表先 join 造成中間結果爆炸
│        + Scan ALL │
└──────────────────┘
```

核心問題：Sail 缺少 join reorder 優化器，所有 join 按照 SQL 文本順序（left-deep）執行，當小表應該先 join 時卻被排在最後。

解決方案：實作 DPHyp 演算法進行 join reorder 優化。


# DPHyp 論文解說

## 論文基本資訊

Moerkotte & Neumann, "Dynamic Programming Strikes Back" (2006)，原發表於 SIGMOD。這篇論文在 CMU 15-721 進階資料庫系統課程中被列為 optimizer 章節的核心教材。

## 為什麼需要 DPHyp？

想像你要安排 5 張表的 join 順序。可以先 join 哪兩張？中間結果再和誰 join？光是排列組合就很多種。

傳統方法有幾種做法：

```
方法         | 搜索空間      | 限制
-------------|---------------|------------------
Exhaustive   | O(n!)        | 只能處理很少的表
DPccp        | O(3^n)       | 只支援 simple edge（兩表之間的 join）
DPHyp        | O(3^n)       | 支援 hyperedge（多表同時參與的 join predicate）
Greedy       | O(n^3)       | 不保證最佳解
```

DPccp 有個限制：它假設每個 join predicate 只涉及兩張表。但實際 SQL 不一定是這樣，例如：

```sql
-- TPC-DS 常見的 pattern：一個 predicate 同時牽涉三張表
SELECT ...
FROM store_sales a
JOIN date_dim d    ON a.sold_date = d.date_id
JOIN store s       ON a.store_id = s.store_id
WHERE a.sale_date BETWEEN d.start_date AND d.end_date
  AND s.region = 'West'
```

`a.sale_date BETWEEN d.start_date AND d.end_date` 這個條件同時牽涉 a 和 d 兩張表，看起來像普通 edge。但如果再加上 `AND a.store_id = s.store_id AND d.fiscal_year = s.open_year`，一個 WHERE clause 就把 a、d、s 三張表綁在一起了。

在 graph 上，這種「一條 edge 連接三個以上節點」的東西叫 hyperedge。DPccp 處理不了，DPHyp 可以。

```
普通 graph（每條 edge 連 2 個節點）：    hypergraph（edge 可以連 3+ 個節點）：

  A --- B                                    A ---+
  |     |                                    |    |--- hyperedge
  C --- D                                    D ---+
                                             |
                                             S
```

## 核心概念：CSG-CMP Pair

DPHyp 要解決的問題可以這樣想：我有 n 張表，我想找到最好的 join 順序。

最暴力的方法是窮舉所有可能的二元分割——把 n 張表分成兩組，各自先 join 好，最後再把兩組 join 起來。但不是隨便分就行，必須滿足：

```
1. 兩組都不能是空的
2. 兩組不能重疊
3. 兩組各自內部必須「連通」（組內的表之間有 join 條件串起來）
4. 兩組之間必須有至少一個 join 條件
```

滿足這四個條件的一對 (S1, S2) 就叫做 CSG-CMP pair。

用一個實際 SQL 來看：

```sql
SELECT *
FROM orders o          -- 100 萬行
JOIN customers c ON o.cust_id = c.id       -- 1 萬行
JOIN products p  ON o.prod_id = p.id       -- 5 千行
JOIN warehouse w ON p.warehouse_id = w.id  -- 50 行
```

這 4 張表的 join 關係長這樣：

```
o --- c          o 和 c 有 join 條件
|                o 和 p 有 join 條件
p --- w          p 和 w 有 join 條件
```

哪些分法是合法的 CSG-CMP pair？

```
分法                    合法？    為什麼
──────────────────     ──────    ──────────────────────────────
({o}, {c})             ✅       o 和 c 之間有 edge (cust_id=id)
({o}, {p})             ✅       o 和 p 之間有 edge (prod_id=id)
({p}, {w})             ✅       p 和 w 之間有 edge (warehouse_id=id)
({o}, {w})             ❌       o 和 w 之間沒有直接 edge
({c}, {p})             ❌       c 和 p 之間沒有直接 edge
({c}, {w})             ❌       c 和 w 之間沒有直接 edge
({o,c}, {p})           ✅       {o,c} 連通，{p} 連通，o-p 有 edge
({o,c}, {w})           ❌       {o,c} 和 {w} 之間沒有 edge
({o,p}, {c})           ✅       {o,p} 連通，o-c 有 edge
({o,p}, {w})           ✅       {o,p} 連通，p-w 有 edge
({o,c}, {p,w})         ✅       {o,c} 連通，{p,w} 連通，o-p 有 edge
({o,p}, {c,w})         ❌       {c,w} 不連通！c 和 w 之間沒路
({o,c,p}, {w})         ✅       {o,c,p} 連通，p-w 有 edge
({o,p,w}, {c})         ✅       {o,p,w} 連通，o-c 有 edge
({o}, {c,p,w})         ❌       {c,p,w} 不連通！c 和 p 之間沒路
({o}, {p,w})           ✅       {p,w} 連通，o-p 有 edge
```

DPHyp 的工作就是系統性地列舉所有合法 CSG-CMP pair，對每一對算出 join cost，最後從 DP table 裡拿出全域最佳解。

重點是它「只列舉合法的」——不連通的分法根本不會被產生出來，避免了 cartesian product，也避免了浪費時間在不可能的方案上。

## 演算法流程

直覺上，DPHyp 的做法像是：

```
1. 先建好每張單表的「方案」（leaf plan）
2. 然後試所有「兩張表 join」的可能，記下最好的
3. 再試所有「三張表」的可能（拿已經算好的兩表結果 + 一張新表）
4. 一路往上，直到涵蓋所有表
```

但它不是笨笨地 enumerate 所有子集，而是從每個節點出發，沿著 edge 往外「長」出連通子圖。

用上面的 o-c-p-w 例子追蹤一輪（簡化版）：

```
從 w (id=3) 開始：
  w 的鄰居是 p
  → 試 {w} ⋈ {p}，記錄 cost
  → p 的鄰居是 o，把 {p} 長成 {p,o}
    → 試 {w} ⋈ {p,o}，記錄 cost
    → o 的鄰居是 c，把 {p,o} 長成 {p,o,c}
      → 試 {w} ⋈ {p,o,c}，記錄 cost

從 p (id=2) 開始：
  p 的鄰居是 o 和 w（但 w 已經處理過，放入 forbidden）
  → 試 {p} ⋈ {o}，記錄 cost
  → o 的鄰居是 c
    → 試 {p} ⋈ {o,c}，記錄 cost
    → 試 {p,o} ⋈ {c}，記錄 cost（CSG 也會被擴展）

從 c (id=1) 開始：
  c 的鄰居是 o（p 和 w 已在 forbidden）
  → 試 {c} ⋈ {o}，記錄 cost

從 o (id=0) 開始：
  所有鄰居都在 forbidden，結束
```

每次 `試 {X} ⋈ {Y}` 就是 EmitCSGCMP——從 DP table 拿出 X 和 Y 的最佳子方案，算出合併 cost，如果比 DP table 裡已有的 X∪Y 方案更好就更新。

正式的虛擬碼：

```
DPHyp 主流程:
1. 初始化：為每個單表建立 leaf plan 放入 DP table
2. 對每個 relation v（由大到小遍歷）:
   a. EmitCSG({v})      -- 以 v 為起點，找它的 CMP 對手
   b. EnumerateCSGRec({v}, forbidden)  -- 把 {v} 往外長成更大的 CSG

EmitCSG(S1):
  找 S1 的所有鄰居 N
  對每個鄰居 n:
    EmitCSGCMP(S1, {n})                 -- 試試 S1 和 {n} join
    EnumerateCMPRec(S1, {n}, forbidden) -- 把 {n} 往外長成更大的 CMP

EnumerateCSGRec(S1, forbidden):
  找 S1 的鄰居 N（排除 forbidden）
  對 N 的每個非空子集 S_ext:
    EmitCSG(S1 ∪ S_ext)                -- 長大的 CSG 去找新的 CMP 對手
  遞迴 EnumerateCSGRec(S1 ∪ S_ext, forbidden ∪ N)

EnumerateCMPRec(S1, S2, forbidden):
  找 S2 的鄰居 N（排除 forbidden）
  對 N 的每個非空子集 S_ext:
    if S1 和 (S2 ∪ S_ext) 之間有 edge:
      EmitCSGCMP(S1, S2 ∪ S_ext)       -- 長大的 CMP 和 S1 試 join
  遞迴 EnumerateCMPRec(S1, S2 ∪ S_ext, forbidden ∪ N)

EmitCSGCMP(S1, S2):
  從 DP table 取 best_plan(S1) 和 best_plan(S2)
  算 join cost
  如果比 DP table 裡 S1∪S2 的方案更好，就更新
```

forbidden set 是什麼？它是「已經當過起點的 relation」的集合。因為 DPHyp 從大 ID 到小 ID 遍歷，每個 relation 只當一次起點。forbidden 確保同一組 CSG-CMP pair 不會被列舉兩次（例如 {o}⋈{c} 和 {c}⋈{o} 只算一次）。

## 成本模型

論文建議使用 Cout（"out" cost model），概念很直覺：

join 的代價 = 把左邊算出來的代價 + 把右邊算出來的代價 + join 之後吐出來的行數

```
Cost(A ⋈ B) = Cost(A) + Cost(B) + |A ⋈ B|
```

單表（leaf）的 cost 為 0，因為只是掃描，不涉及 join。

那 `|A ⋈ B|`（join 之後有多少行）怎麼估？用 TDom（Total Domain）：

```
|A ⋈ B| = |A| * |B| / TDom(join_key)
```

TDom 就是 join key 的 distinct value 數量（取最大的那一邊）。

舉例：orders 有 100 萬行，customers 有 1 萬行，join key 是 customer_id，customers 表的 id 有 10,000 個 distinct value。

```
|orders ⋈ customers| = 1,000,000 * 10,000 / 10,000 = 1,000,000
```

直覺上也合理：每個 order 對應一個 customer，join 後行數和 orders 差不多。

如果 TDom 很大（例如 join key 很分散），分母大，join 結果小，代表這個 join 「過濾效果好」。如果 TDom 很小（例如 join key 只有幾個值），分母小，join 結果大，代表「膨脹嚴重」。DPHyp 的目標就是找到一個順序，讓每一步的中間結果盡量小。


# PR #810：Join Reorder 設定框架

PR #810 建立了 join reorder 的配置基礎設施，預設關閉（experimental）。

## 設定層：application.yaml → AppConfig

在 `crates/sail-common/src/config/application.yaml` 加入設定項：

```yaml
optimizer:
  enable_join_reorder: false
```

對應的 Rust struct：

```rust
// crates/sail-common/src/config/application.rs
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OptimizerConfig {
    pub enable_join_reorder: bool,
}
```

🔸 `#[derive(Deserialize)]` 讓 figment 框架能自動從 YAML 反序列化成這個 struct。`deny_unknown_fields` 確保 YAML 中不會有拼錯的欄位。

## 連接層：PhysicalOptimizerOptions

```rust
// crates/sail-physical-optimizer/src/lib.rs
#[derive(Debug, Clone, Default)]
pub struct PhysicalOptimizerOptions {
    pub enable_join_reorder: bool,
}

pub fn get_physical_optimizers(
    options: PhysicalOptimizerOptions,
) -> Vec<Arc<dyn PhysicalOptimizerRule + Send + Sync>> {
    let mut rules: Vec<Arc<dyn PhysicalOptimizerRule + Send + Sync>> = vec![];
    rules.push(Arc::new(OutputRequirements::new_add_mode()));
    rules.push(Arc::new(AggregateStatistics::new()));
    if options.enable_join_reorder {
        rules.push(Arc::new(JoinReorder::new()));
    }
    rules.push(Arc::new(JoinSelection::new()));
    // ... 其他規則
}
```

🔸 `get_physical_optimizers()` 根據 `enable_join_reorder` flag 決定是否插入 `JoinReorder` 規則。注意插入順序在 `JoinSelection` 之前，因為 JoinReorder 負責決定 join 順序，JoinSelection 負責決定 join 算法（hash join vs sort merge join）。

## 注入層：ServerSessionFactory

```rust
// crates/sail-session/src/session_factory/server.rs
let builder = SessionStateBuilder::new()
    .with_config(config)
    .with_runtime_env(runtime)
    .with_default_features()
    .with_physical_optimizer_rules(get_physical_optimizers(PhysicalOptimizerOptions {
        enable_join_reorder: self.config.optimizer.enable_join_reorder,
    }))
    .with_query_planner(new_query_planner());
```

🔸 `self.config` 是 `Arc<AppConfig>`，從 YAML 加載。這裡將 `AppConfig.optimizer.enable_join_reorder` 傳入 `PhysicalOptimizerOptions`，再交給 `get_physical_optimizers()` 組裝規則鏈。

資料流：

```
application.yaml
  -> figment 反序列化
    -> AppConfig.optimizer.enable_join_reorder (bool)
      -> PhysicalOptimizerOptions.enable_join_reorder
        -> get_physical_optimizers() 條件性插入 JoinReorder
          -> SessionStateBuilder.with_physical_optimizer_rules()
```


# PR #917：DPHyp 完整實作

PR #917 在 `sail-physical-optimizer` crate 新增了 `join_reorder` 模組，共 9 個檔案約 1,987 行程式碼。

## Crate 功能總覽

| 檔案 | 功能 | 行數 |
|------|------|------|
| mod.rs | JoinReorder 規則入口，遞迴尋找 join region | ~360 |
| join_set.rs | u64 bitset 表示 relation 集合，最多 64 個表 | ~345 |
| graph.rs | QueryGraph 資料結構，trie-based 鄰居查找 | ~662 |
| builder.rs | 從 ExecutionPlan 建立 QueryGraph | ~743 |
| dp_plan.rs | DP table 中的 plan 節點（Leaf / Join） | ~178 |
| enumerator.rs | DPHyp 演算法核心，CSG-CMP 列舉 | ~621 |
| cardinality_estimator.rs | 等價集合 + TDom 基數估算 | ~596 |
| cost_model.rs | Cout 成本模型 | ~69 |
| reconstructor.rs | 將 DPPlan 還原回 ExecutionPlan | ~1270 |

## 完整調用鏈

```
DataFusion optimize() 觸發規則
  -> JoinReorder::optimize()          [mod.rs]
    -> find_and_optimize_regions()     [mod.rs]
      -> try_optimize_region()         [mod.rs]
        -> GraphBuilder::build()       [builder.rs]
        -> PlanEnumerator::solve()     [enumerator.rs]
          -> init_leaf_plans()
          -> join_reorder_by_dphyp()
            -> emit_csg()
              -> emit_csg_cmp()
            -> enumerate_csg_rec()
            -> enumerate_cmp_rec()
          或 solve_greedy()（fallback）
        -> PlanReconstructor::reconstruct()  [reconstructor.rs]
        -> build_final_projection()    [mod.rs]
```

## 建議閱讀順序

```
1. join_set.rs       -- 基礎資料結構，理解 bitset 表示法
2. graph.rs          -- QueryGraph 建模，trie 結構
3. dp_plan.rs        -- DP table entry 結構
4. cost_model.rs     -- 最簡單的檔案，理解成本公式
5. cardinality_estimator.rs  -- TDom 估算
6. builder.rs        -- 從 ExecutionPlan 建圖
7. enumerator.rs     -- DPHyp 核心演算法
8. reconstructor.rs  -- 從 DPPlan 還原 ExecutionPlan
9. mod.rs            -- 入口點，串接全部
```

## 簡單例子

用一個 3 表 join 追蹤完整流程：

```sql
SELECT *
FROM orders o
JOIN customers c ON o.customer_id = c.id
JOIN products p ON o.product_id = p.id
```

假設 orders 10,000 行、customers 100 行、products 500 行。

TDom 假設：TDom(customer_id) = 100（customers 的 distinct count），TDom(product_id) = 500（products 的 distinct count）。

Cout 公式回顧：`Cost(A ⋈ B) = Cost(A) + Cost(B) + |A ⋈ B|`，其中 leaf node 的 cost 為 0。

```
方案 A：(o ⋈ c) ⋈ p（按 SQL 文本順序）

  Step 1: o ⋈ c（on o.customer_id = c.id）
    |o ⋈ c| = |o| * |c| / TDom(customer_id)
             = 10,000 * 100 / 100
             = 10,000
    Cost_1 = Cost(o) + Cost(c) + |o ⋈ c|
           = 0 + 0 + 10,000
           = 10,000

  Step 2: (o ⋈ c) ⋈ p（on o.product_id = p.id）
    |(o⋈c) ⋈ p| = |o⋈c| * |p| / TDom(product_id)
                 = 10,000 * 500 / 500
                 = 10,000
    Cost_2 = Cost_1 + Cost(p) + |(o⋈c) ⋈ p|
           = 10,000 + 0 + 10,000
           = 20,000

  總 Cost = 20,000


方案 B：(o ⋈ p) ⋈ c（先 join products）

  Step 1: o ⋈ p（on o.product_id = p.id）
    |o ⋈ p| = 10,000 * 500 / 500 = 10,000
    Cost_1 = 0 + 0 + 10,000 = 10,000

  Step 2: (o ⋈ p) ⋈ c（on o.customer_id = c.id）
    |(o⋈p) ⋈ c| = 10,000 * 100 / 100 = 10,000
    Cost_2 = 10,000 + 0 + 10,000 = 20,000

  總 Cost = 20,000（和方案 A 相同）
```

在這個簡化例子中兩種方案 cost 相同，因為 cardinality 估算公式是對稱的（乘法交換律）。

但如果 TDom 不同，差距就會出現。例如把 |products| 改成 50,000、TDom(product_id) = 50,000：

```
方案 A：(o ⋈ c) ⋈ p
  Step 1: |o ⋈ c| = 10,000 * 100 / 100 = 10,000     Cost_1 = 10,000
  Step 2: |(o⋈c) ⋈ p| = 10,000 * 50,000 / 50,000 = 10,000  Cost = 20,000

方案 B：(o ⋈ p) ⋈ c
  Step 1: |o ⋈ p| = 10,000 * 50,000 / 50,000 = 10,000   Cost_1 = 10,000
  Step 2: |(o⋈p) ⋈ c| = 10,000 * 100 / 100 = 10,000     Cost = 20,000
```

還是一樣！這是因為 3 表鏈狀 join 在 Cout 模型下，不同順序的差異來自中間結果的大小差異。真正的差距出現在更多表或扇形（star schema）的場景，例如 TPC-DS query 72 有 5+ 張表，各表 cardinality 和 selectivity 差異極大。

🔸 關於 (c ⋈ o) vs (o ⋈ c) 的差異

在 Cout 成本模型中，`(c ⋈ o)` 和 `(o ⋈ c)` 的 cost 完全一樣（乘法交換律）。兩者的差異在實作層面：HashJoinExec 的左邊是 build side（建 hash table），右邊是 probe side。讓小表當 build side（c 在左）可以節省記憶體。但選擇哪邊做 build side 是 JoinSelection 規則的責任，不是 JoinReorder 的工作。

🔸 為什麼 (c ⋈ p) ⋈ o 不會被考慮

c 和 p 之間沒有 join predicate，DPHyp 的 CSG-CMP 列舉只會產生有 edge 連接的 pair。{c} 和 {p} 之間沒有 edge，所以不會被列舉為合法的 CSG-CMP pair，避免了 cartesian product。

## 完整 SQL 追蹤：從優化前到優化後

用一個 4 表 star schema 例子，追蹤 SQL 經過每一層源碼的完整過程。

```sql
-- 4 張表：fact_sales (大表) + 3 張維度表 (小表)
-- Star schema：所有維度表都只和 fact_sales join

SELECT s.amount, c.name, p.title, d.month
FROM fact_sales s
JOIN dim_date d      ON s.date_id = d.id
JOIN dim_customer c  ON s.cust_id = c.id
JOIN dim_product p   ON s.prod_id = p.id
```

```
表的 cardinality：
  fact_sales (s): 1,000,000 行
  dim_date (d):       365 行    TDom(date_id) = 365
  dim_customer (c): 10,000 行    TDom(cust_id) = 10,000
  dim_product (p):   5,000 行    TDom(prod_id) = 5,000

Query Graph（star schema）：
       d
       |  edge0: s.date_id = d.id
       s
      / \
     /   \
    c     p
  edge1   edge2
  s.cust_id = c.id    s.prod_id = p.id
```

SQL parser 按文本順序產生的原始 physical plan：

```
ProjectionExec [s.amount, c.name, p.title, d.month]
  HashJoinExec [s.prod_id = p.id]          -- 第 3 個 JOIN (最外層)
    HashJoinExec [s.cust_id = c.id]        -- 第 2 個 JOIN
      HashJoinExec [s.date_id = d.id]      -- 第 1 個 JOIN (最內層)
        DataSourceExec [fact_sales]         -- s
        DataSourceExec [dim_date]           -- d
      DataSourceExec [dim_customer]         -- c
    DataSourceExec [dim_product]            -- p
```

原始 join 順序：`((s ⋈ d) ⋈ c) ⋈ p`

```
Cost 計算（原始順序）：

  Step 1: s ⋈ d
    |s ⋈ d| = 1,000,000 * 365 / 365 = 1,000,000
    Cost_1 = 0 + 0 + 1,000,000 = 1,000,000

  Step 2: (s⋈d) ⋈ c
    |(s⋈d) ⋈ c| = 1,000,000 * 10,000 / 10,000 = 1,000,000
    Cost_2 = 1,000,000 + 0 + 1,000,000 = 2,000,000

  Step 3: ((s⋈d)⋈c) ⋈ p
    |((s⋈d)⋈c) ⋈ p| = 1,000,000 * 5,000 / 5,000 = 1,000,000
    Cost_3 = 2,000,000 + 0 + 1,000,000 = 3,000,000

  總 Cost = 3,000,000
```

假設 dim_date 有一個 filter `d.year = 2024` 把 365 行過濾成 30 行，同時 dim_customer 有 `c.country = 'TW'` 過濾成 500 行。這些 filter 已經被 predicate pushdown 推到 scan 中，所以進入 join reorder 時各表的 cardinality 是：

```
修正後的 cardinality（filter 後）：
  fact_sales (s): 1,000,000 行（沒有 filter）
  dim_date (d):        30 行（year = 2024）  TDom(date_id) = 30
  dim_customer (c):   500 行（country = TW）  TDom(cust_id) = 500
  dim_product (p):  5,000 行                  TDom(prod_id) = 5,000
```

```
Cost 計算（原始順序，filter 後）：

  Step 1: s ⋈ d
    |s ⋈ d| = 1,000,000 * 30 / 30 = 1,000,000
    Cost_1 = 1,000,000

  Step 2: (s⋈d) ⋈ c
    |(s⋈d) ⋈ c| = 1,000,000 * 500 / 500 = 1,000,000
    Cost_2 = 1,000,000 + 1,000,000 = 2,000,000

  Step 3: ((s⋈d)⋈c) ⋈ p
    |((s⋈d)⋈c) ⋈ p| = 1,000,000 * 5,000 / 5,000 = 1,000,000
    Cost_3 = 2,000,000 + 1,000,000 = 3,000,000

  總 Cost = 3,000,000（和沒 filter 一樣，因為 TDom 也隨 filter 縮小了）
```

等等，這樣看起來 star schema 的 Cout 不會有差異？

關鍵在於：如果 TDom 不等於過濾後的行數（實際情況），差異就出來了。實務中 TDom 來自 DataFusion 的 column statistics（distinct count），它反映的是原始表的 NDV 而非過濾後的。假設 statistics 回報的是原始值：

```
修正的 TDom（來自 column statistics，不受 filter 影響）：
  TDom(date_id) = 365    （dim_date 原始 NDV）
  TDom(cust_id) = 10,000  （dim_customer 原始 NDV）
  TDom(prod_id) = 5,000   （dim_product 原始 NDV）
```

```
原始順序 ((s ⋈ d) ⋈ c) ⋈ p，TDom 用原始 NDV：

  Step 1: s ⋈ d
    |s ⋈ d| = 1,000,000 * 30 / 365 = 82,192
    Cost_1 = 82,192

  Step 2: (s⋈d) ⋈ c
    |(s⋈d) ⋈ c| = 82,192 * 500 / 10,000 = 4,110
    Cost_2 = 82,192 + 4,110 = 86,302

  Step 3: ((s⋈d)⋈c) ⋈ p
    |((s⋈d)⋈c) ⋈ p| = 4,110 * 5,000 / 5,000 = 4,110
    Cost_3 = 86,302 + 4,110 = 90,412

  總 Cost = 90,412
```

如果換成先 join 最有選擇性的維度表（d 只有 30 行且 TDom 365）：

```
最佳順序 ((s ⋈ c) ⋈ d) ⋈ p：

  Step 1: s ⋈ c
    |s ⋈ c| = 1,000,000 * 500 / 10,000 = 50,000
    Cost_1 = 50,000

  Step 2: (s⋈c) ⋈ d
    |(s⋈c) ⋈ d| = 50,000 * 30 / 365 = 4,110
    Cost_2 = 50,000 + 4,110 = 54,110

  Step 3: ((s⋈c)⋈d) ⋈ p
    |((s⋈c)⋈d) ⋈ p| = 4,110 * 5,000 / 5,000 = 4,110
    Cost_3 = 54,110 + 4,110 = 58,220

  總 Cost = 58,220  ← 比 90,412 少了 35%!
```

差異的來源：第一步的中間結果大小。先 join c（500 行 / TDom 10,000）比先 join d（30 行 / TDom 365）產生更小的中間結果（50,000 < 82,192）。

現在追蹤源碼如何找到這個最佳順序。

---

🔸 Step 0: 觸發優化

DataFusion 依序執行 physical optimizer rules，輪到 JoinReorder。

```rust
// mod.rs - JoinReorder::optimize()
fn optimize(&self, plan: Arc<dyn ExecutionPlan>, _config: &ConfigOptions) -> Result<Arc<dyn ExecutionPlan>> {
    self.find_and_optimize_regions(plan)
}
```

`find_and_optimize_regions` 從 ProjectionExec 開始，發現它不是 join，遞迴到 children。到達最外層 HashJoinExec 時呼叫 `try_optimize_region`。

---

🔸 Step 1: 建立 Query Graph（builder.rs）

```rust
// mod.rs - try_optimize_region()
let mut graph_builder = GraphBuilder::new();
let Some((query_graph, target_column_map)) = graph_builder.build(plan.clone())? else {
    return Ok(None);
};
```

`GraphBuilder::build()` 呼叫 `visit_plan()`，遞迴走進 plan tree：

```
visit_plan(HashJoinExec [s.prod_id = p.id])        -- Inner Join → visit_inner_join
  visit_plan(HashJoinExec [s.cust_id = c.id])      -- Inner Join → visit_inner_join
    visit_plan(HashJoinExec [s.date_id = d.id])    -- Inner Join → visit_inner_join
      visit_plan(DataSourceExec [fact_sales])       -- Leaf → create_relation_node(id=0)
      visit_plan(DataSourceExec [dim_date])         -- Leaf → create_relation_node(id=1)
    visit_plan(DataSourceExec [dim_customer])       -- Leaf → create_relation_node(id=2)
  visit_plan(DataSourceExec [dim_product])          -- Leaf → create_relation_node(id=3)
```

`create_relation_node` 為每張表分配 relation_id 並讀取 cardinality：

```rust
// builder.rs
fn create_relation_node(&mut self, plan: Arc<dyn ExecutionPlan>) -> Result<ColumnMap> {
    let relation_id = self.relation_counter;   // s=0, d=1, c=2, p=3
    self.relation_counter += 1;
    let stats = plan.partition_statistics(None)?;
    let initial_cardinality = match stats.num_rows {
        Precision::Exact(count) => count as f64,   // s=1000000, d=30, c=500, p=5000
        Precision::Inexact(count) => count as f64,
        Precision::Absent => 1000.0,
    };
    // ...
}
```

`visit_inner_join` 為每個 HashJoinExec 建立 JoinEdge：

```rust
// builder.rs - visit_inner_join() 對每個 ON 條件
for (left_on, right_on) in join_plan.on() {
    // 解析 equi_pairs: (s.date_id, d.id), (s.cust_id, c.id), (s.prod_id, p.id)
}
let edge = JoinEdge::new(all_relations_in_condition, filter_expr, JoinType::Inner, equi_pairs);
self.graph.add_edge(edge)?;
```

建好的 QueryGraph：

```
Relations: [s(id=0, card=1000000), d(id=1, card=30), c(id=2, card=500), p(id=3, card=5000)]
Edges:
  edge0: {0,1} s.date_id = d.id    equi_pairs: [(R0.C_date_id, R1.C_id)]
  edge1: {0,2} s.cust_id = c.id    equi_pairs: [(R0.C_cust_id, R2.C_id)]
  edge2: {0,3} s.prod_id = p.id    equi_pairs: [(R0.C_prod_id, R3.C_id)]

Trie 結構（由 add_edge 自動建立）：
  root
  ├── 0 → neighbors: [{1}, {2}, {3}]     s 的鄰居是 d, c, p
  ├── 1 → neighbors: [{0}]               d 的鄰居是 s
  ├── 2 → neighbors: [{0}]               c 的鄰居是 s
  └── 3 → neighbors: [{0}]               p 的鄰居是 s
```

ColumnMap（target_column_map）記錄原始 plan 的輸出對應：

```
[Stable(R0.C_amount), Stable(R2.C_name), Stable(R3.C_title), Stable(R1.C_month)]
```

---

🔸 Step 2: 初始化 Cardinality Estimator（cardinality_estimator.rs）

```rust
// enumerator.rs
let mut enumerator = PlanEnumerator::new(query_graph);
```

PlanEnumerator::new 內部建立 CardinalityEstimator：

```rust
// cardinality_estimator.rs - new()
estimator.populate_initial_distinct_counts();   // 從 statistics 讀 NDV
estimator.init_equivalence_sets();              // 從 equi_pairs 建等價集合
```

`init_equivalence_sets` 遍歷 3 條 edge 的 equi_pairs，建立 3 個 EquivalenceSet：

```
EquivalenceSet 0: {R0.C_date_id, R1.C_id}  TDom = max(NDV) = 365
EquivalenceSet 1: {R0.C_cust_id, R2.C_id}  TDom = max(NDV) = 10,000
EquivalenceSet 2: {R0.C_prod_id, R3.C_id}  TDom = max(NDV) = 5,000
```

---

🔸 Step 3: DPHyp 列舉（enumerator.rs）

```rust
// enumerator.rs - solve()
self.init_leaf_plans()?;
self.join_reorder_by_dphyp()?;
```

`init_leaf_plans` 建立 4 個 leaf DPPlan：

```
dp_table:
  {0} → DPPlan(Leaf, cost=0, card=1,000,000)   -- s
  {1} → DPPlan(Leaf, cost=0, card=30)           -- d
  {2} → DPPlan(Leaf, cost=0, card=500)          -- c
  {3} → DPPlan(Leaf, cost=0, card=5,000)        -- p
```

`join_reorder_by_dphyp` 從大到小遍歷 relation：

```rust
// enumerator.rs
fn join_reorder_by_dphyp(&mut self) -> Result<bool> {
    for idx in (0..self.query_graph.relation_count()).rev() {  // idx = 3, 2, 1, 0
        self.process_node_as_start(idx)?;
    }
    Ok(true)
}
```

以 idx=3 (p) 為例追蹤 `process_node_as_start(3)`：

```
process_node_as_start(3):
  nodes = {3}
  emit_csg({3}):
    neighbors({3}, forbidden={3}) = [0]    -- p 只連到 s
    nbr = 0:
      edge_indices = get_connecting_edge_indices({3}, {0}) = [2]  -- edge2
      emit_csg_cmp({3}, {0}, [2]):
        left_plan = dp_table[{3}] → card=5000, cost=0
        right_plan = dp_table[{0}] → card=1000000, cost=0
        new_cardinality = estimate_join_cardinality(5000, 1000000, [2])
          edge2 的 TDom = 5000
          selectivity = 1/5000
          = 5000 * 1000000 * (1/5000) = 1,000,000
        new_cost = 0 + 0 + 1,000,000 = 1,000,000
        dp_table[{0,3}] = DPPlan(Join, cost=1000000, card=1000000)

      enumerate_cmp_rec({3}, {0}, forbidden):
        neighbors({0}, forbidden) = [1, 2]  -- s 連到 d 和 c（3 已在 forbidden）
        -- 擴展 CMP：{0} -> {0,1}, {0,2}, {0,1,2}
        -- 每個擴展都檢查是否和 {3} 有 connecting edge
        ...
```

類似地，idx=2 (c)、idx=1 (d)、idx=0 (s) 各自觸發列舉。

關鍵的 `emit_csg_cmp` 呼叫（只列出重要的）：

```
emit_csg_cmp 結果摘要（cost 由小到大排序）：

兩表 join：
  {0,1} s⋈d:  card = 1M * 30 / 365 = 82,192      cost = 82,192
  {0,2} s⋈c:  card = 1M * 500 / 10000 = 50,000    cost = 50,000     ← 最小
  {0,3} s⋈p:  card = 1M * 5000 / 5000 = 1,000,000  cost = 1,000,000

三表 join（以兩表結果為基礎，列舉所有合法 CSG-CMP pair）：
  {0,1,2} 最佳方案：{0,2}⋈{1}
    card = 50,000 * 30 / 365 = 4,110
    cost = 50,000 + 0 + 4,110 = 54,110              ← 比 {0,1}⋈{2} 的 86,302 好

  {0,1,3} 最佳方案：{0,1}⋈{3}
    card = 82,192 * 5000 / 5000 = 82,192
    cost = 82,192 + 0 + 82,192 = 164,384

  {0,2,3} 最佳方案：{0,2}⋈{3}
    card = 50,000 * 5000 / 5000 = 50,000
    cost = 50,000 + 0 + 50,000 = 100,000

四表 join（最終結果）：
  {0,1,2,3} 候選方案：
    {0,1,2}⋈{3}: cost = 54,110 + 0 + 4,110 = 58,220    ← 最佳!
    {0,2,3}⋈{1}: cost = 100,000 + 0 + (50000*30/365) = 104,110
    {0,1,3}⋈{2}: cost = 164,384 + 0 + (82192*500/10000) = 168,494
    {0,1}⋈{2,3}: 不合法！{2} 和 {3} 之間沒有 edge，{2,3} 不連通
```

最終 dp_table[{0,1,2,3}] 的最佳方案：

```
DPPlan {
  join_set: {0,1,2,3},
  cost: 58,220,
  plan_type: Join {
    left_set: {0,1,2},     -- ((s ⋈ c) ⋈ d)
    right_set: {3},         -- p
    edge_indices: [2],      -- s.prod_id = p.id
  }
}
  └── left: DPPlan {
        join_set: {0,1,2},
        cost: 54,110,
        plan_type: Join {
          left_set: {0,2},  -- (s ⋈ c)
          right_set: {1},   -- d
          edge_indices: [0],
        }
      }
        └── left: DPPlan {
              join_set: {0,2},
              cost: 50,000,
              plan_type: Join {
                left_set: {0},  -- s
                right_set: {2}, -- c
                edge_indices: [1],
              }
            }
```

最佳 join tree: `((s ⋈ c) ⋈ d) ⋈ p`，Cost = 58,220（原始 90,412 的 64%）

---

🔸 Step 4: 重建 ExecutionPlan（reconstructor.rs）

```rust
// mod.rs - try_optimize_region()
let mut reconstructor = PlanReconstructor::new(&enumerator.dp_table, &enumerator.query_graph);
let (join_tree, final_map) = reconstructor.reconstruct(&best_plan)?;
```

`reconstruct` 遞迴展開 DPPlan tree：

```
reconstruct({0,1,2,3}) → Join
  reconstruct({0,1,2}) → Join
    reconstruct({0,2}) → Join
      reconstruct_leaf(0) → DataSourceExec[fact_sales]
      reconstruct_leaf(2) → DataSourceExec[dim_customer]
      → build_join_conditions([1]) → s.cust_id = c.id
      → HashJoinExec(s, c, on=[cust_id=id])

    reconstruct_leaf(1) → DataSourceExec[dim_date]
    → build_join_conditions([0]) → s.date_id = d.id
    → HashJoinExec((s⋈c), d, on=[date_id=id])

  reconstruct_leaf(3) → DataSourceExec[dim_product]
  → build_join_conditions([2]) → s.prod_id = p.id
  → HashJoinExec(((s⋈c)⋈d), p, on=[prod_id=id])
```

`build_join_conditions` 用 ColumnMap 把 stable column 對應回實際 index：

```rust
// reconstructor.rs - build_join_conditions()
for (col1_stable, col2_stable) in &edge.equi_pairs {
    let col1_left_idx = find_physical_index(col1_stable, left_map);   // 在左表 schema 中的位置
    let col2_right_idx = find_physical_index(col2_stable, right_map); // 在右表 schema 中的位置
    // 建立 (Column::new(left_name, left_idx), Column::new(right_name, right_idx))
}
```

---

🔸 Step 5: 最終 Projection（mod.rs）

```rust
// mod.rs - try_optimize_region()
let final_plan = self.build_final_projection(join_tree, &final_map, &target_column_map, &target_names)?;
```

reorder 後的 join tree schema 是 `[s的所有欄位, c的所有欄位, d的所有欄位, p的所有欄位]`（按 join 順序拼接），但原始 SELECT 要的是 `[s.amount, c.name, p.title, d.month]`。

`build_final_projection` 用 target_column_map 把 stable column 映射到新 schema 中的位置：

```rust
// mod.rs - build_final_projection()
for (output_idx, target_entry) in target_map.iter().enumerate() {
    match target_entry {
        ColumnMapEntry::Stable { relation_id, column_index } => {
            // target_map[0] = R0.C_amount → 在 final_map 中找 R0.C_amount 的 physical index
            // target_map[1] = R2.C_name   → 在 final_map 中找 R2.C_name
            // target_map[2] = R3.C_title  → 在 final_map 中找 R3.C_title
            // target_map[3] = R1.C_month  → 在 final_map 中找 R1.C_month
            let physical_idx = find_physical_index(&stable_target, final_map)?;
            projection_exprs.push((Column::new(input_name, physical_idx), output_name));
        }
    }
}
// → ProjectionExec 重新排列欄位順序，確保和原始 plan 的 schema 完全一致
```

---

🔸 最終結果

優化前：

```
ProjectionExec [amount, name, title, month]
  HashJoinExec [prod_id = id]           -- Step 3: cost += 1,000,000
    HashJoinExec [cust_id = id]         -- Step 2: cost += 1,000,000
      HashJoinExec [date_id = id]       -- Step 1: cost += 82,192
        DataSourceExec [fact_sales]
        DataSourceExec [dim_date]
      DataSourceExec [dim_customer]
    DataSourceExec [dim_product]

總 Cost = 90,412
Join 順序：((s ⋈ d) ⋈ c) ⋈ p
```

優化後：

```
ProjectionExec [amount, name, title, month]    -- Step 5: 重排欄位
  HashJoinExec [prod_id = id]                  -- Step 3: cost += 4,110
    HashJoinExec [date_id = id]                -- Step 2: cost += 4,110
      HashJoinExec [cust_id = id]              -- Step 1: cost += 50,000
        DataSourceExec [fact_sales]
        DataSourceExec [dim_customer]
      DataSourceExec [dim_date]
    DataSourceExec [dim_product]

總 Cost = 58,220（減少 35%）
Join 順序：((s ⋈ c) ⋈ d) ⋈ p
```

差異的關鍵：先 join dim_customer（500 行 / TDom 10,000）讓第一步中間結果只有 50,000 行，比原始方案先 join dim_date（30 行 / TDom 365）的 82,192 行小了 39%。這個差異在後續每一步都被累積放大。

---

## 程式碼逐檔解說

---

## 1. join_set.rs — Relation 集合的 Bitset 表示

路徑：`crates/sail-physical-optimizer/src/join_reorder/join_set.rs`

JoinSet 是整個 join reorder 模組的基礎資料結構，用一個 u64 表示最多 64 個 relation 的集合。

```rust
const MAX_RELATIONS: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct JoinSet(u64);
```

🔸 `#[derive(Copy)]` 讓 JoinSet 可以像 integer 一樣直接複製，不需要 clone。`Hash` 是因為它會作為 HashMap 的 key（DP table）。內部只包一個 `u64`，每個 bit 代表一個 relation。

建立 singleton（單表集合）：

```rust
pub fn new_singleton(relation_idx: usize) -> Result<Self> {
    if relation_idx >= MAX_RELATIONS {
        return Err(DataFusionError::Internal(format!(
            "Relation index {} must be less than {}",
            relation_idx, MAX_RELATIONS
        )));
    }
    Ok(Self(1 << relation_idx))
}
```

🔸 `1 << relation_idx` 把第 relation_idx 個 bit 設為 1。例如 relation 3 → `0b1000` → `u64` 值 8。

集合運算透過 operator overloading 實作：

```rust
impl BitOr for JoinSet {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self::Output {
        self.union(&rhs)  // self.0 | rhs.0
    }
}

impl Sub for JoinSet {
    type Output = Self;
    fn sub(self, rhs: Self) -> Self::Output {
        self.difference(&rhs)  // self.0 & !rhs.0
    }
}
```

🔸 `BitOr` 讓你可以寫 `set_a | set_b` 來做聯集。`Sub` 讓你可以寫 `set_a - set_b` 來做差集。Rust 的 trait 系統讓自訂型別也能用運算子。

迭代器使用 trailing_zeros 技巧高效遍歷所有 set bit：

```rust
impl Iterator for JoinSetIter {
    type Item = usize;
    fn next(&mut self) -> Option<Self::Item> {
        if self.bits == 0 {
            None
        } else {
            let index = self.bits.trailing_zeros() as usize;
            self.bits &= self.bits - 1;  // 清除最低位的 1
            Some(index)
        }
    }
}
```

🔸 `trailing_zeros()` 是 CPU 內建指令（x86 的 `tzcnt`），O(1) 找到最低位的 1。`bits & (bits - 1)` 是經典 bit trick，清除最低位的 set bit。例如 `0b1010 & 0b1001 = 0b1000`。

`from_iter` 從任意迭代器建立集合：

```rust
pub fn from_iter<T: IntoIterator<Item = usize>>(iter: T) -> Result<Self> {
    let mut bits = 0u64;
    for relation_idx in iter {
        if relation_idx >= MAX_RELATIONS {
            return Err(DataFusionError::Internal(format!(
                "Relation index {} must be less than {}",
                relation_idx, MAX_RELATIONS
            )));
        }
        bits |= 1 << relation_idx;
    }
    Ok(Self(bits))
}
```

🔸 逐一把每個 relation 的 bit 設為 1。如果任何 index >= 64 就回傳錯誤。

cardinality（集合大小）：

```rust
pub fn cardinality(&self) -> u32 {
    self.0.count_ones()  // CPU 內建 popcnt 指令
}
```

🔸 `count_ones()` 計算 u64 中有多少個 bit 是 1，對應 CPU 的 `popcnt` 指令，O(1) 完成。

---

## 2. graph.rs — Query Graph 與 Trie-Based 鄰居查找

路徑：`crates/sail-physical-optimizer/src/join_reorder/graph.rs`

QueryGraph 是 join reorder 的核心資料結構，用來表示哪些表之間有 join 關係。

StableColumn 是跨 reorder 的穩定欄位標識：

```rust
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct StableColumn {
    pub relation_id: usize,
    pub column_index: usize,
    pub name: String,
}
```

🔸 `relation_id` + `column_index` 唯一標識一個欄位。`Hash` 和 `Eq` 讓它能作為 HashSet/HashMap 的 key，用在等價集合（EquivalenceSet）中。

RelationNode 代表 query graph 中的一個節點（一張表）：

```rust
pub struct RelationNode {
    pub plan: Arc<dyn ExecutionPlan>,
    pub relation_id: usize,
    pub initial_cardinality: f64,
    pub statistics: Statistics,
}
```

🔸 `plan` 保存原始的 DataFusion 執行計畫節點（例如 DataSourceExec）。`initial_cardinality` 是表的預估行數，來自 DataFusion 統計資訊或預設 1000。

JoinEdge 代表兩個或多個表之間的 join predicate：

```rust
pub struct JoinEdge {
    pub join_set: JoinSet,
    pub filter: Arc<dyn PhysicalExpr>,
    pub join_type: JoinType,
    pub equi_pairs: Vec<(StableColumn, StableColumn)>,
}
```

🔸 `join_set` 標記這條 edge 涉及哪些 relation。`filter` 是完整的 join expression。`equi_pairs` 是解析出的等值 join 對，用於基數估算。一條 edge 可以涉及 2 個以上的 relation（hyperedge）。

QueryEdge 是 trie 結構的節點：

```rust
#[derive(Debug, Clone, Default)]
pub struct QueryEdge {
    pub neighbors: Vec<NeighborInfo>,
    pub children: HashMap<usize, QueryEdge>,
}
```

🔸 `children` 以 relation_id 為 key 建立 trie 樹。`neighbors` 存放從這個 trie 路徑可達的鄰居資訊。

QueryGraph 主結構：

```rust
pub struct QueryGraph {
    pub relations: Vec<RelationNode>,
    pub edges: Vec<JoinEdge>,
    root_edge: QueryEdge,          // trie 根節點
    neighbor_cache: HashMap<JoinSet, Vec<usize>>,  // 鄰居查找快取
}
```

新增 edge 時自動更新 trie：

```rust
pub fn add_edge(&mut self, edge: JoinEdge) -> Result<(), DataFusionError> {
    let edge_index = self.edges.len();
    self.edges.push(edge);
    self.neighbor_cache.clear();
    self.update_trie_for_edge(edge_index).map_err(|e| {
        DataFusionError::Internal(format!("Failed to update trie for edge: {}", e))
    })?;
    Ok(())
}
```

🔸 每加一條 edge 就清空 neighbor_cache 並重建 trie 路徑。因為 graph 建構階段只跑一次，效能不是瓶頸。

Trie 路徑建立的核心邏輯 `create_trie_path_and_add_neighbor`：

```rust
fn create_trie_path_and_add_neighbor(
    &mut self,
    subset: &[usize],
    neighbor: JoinSet,
    edge_index: usize,
) {
    let mut sorted_subset = subset.to_vec();
    sorted_subset.sort_unstable();

    let mut current = &mut self.root_edge;
    for &relation_id in &sorted_subset {
        current = current.children.entry(relation_id).or_default();
    }

    if let Some(existing) = current
        .neighbors
        .iter_mut()
        .find(|n| n.neighbor == neighbor)
    {
        existing.edge_indices.push(edge_index);
    } else {
        current
            .neighbors
            .push(NeighborInfo::new(neighbor, vec![edge_index]));
    }
}
```

🔸 把 subset（例如 `[0, 1]`）排序後，沿 trie 路徑走（root → 0 → 1），在終點加入 neighbor 資訊。這樣查詢 "集合 {0,1} 的鄰居是誰" 就只要沿 trie 走一遍。

鄰居查找 `get_neighbors`：

```rust
pub fn get_neighbors(&mut self, nodes: JoinSet) -> Vec<usize> {
    if let Some(cached) = self.neighbor_cache.get(&nodes) {
        return cached.clone();
    }

    let mut neighbors = std::collections::HashSet::new();
    let relations: Vec<usize> = nodes.iter().collect();

    for subset_size in 1..=relations.len() {
        self.find_neighbors_for_subsets(&relations, subset_size, &mut neighbors);
    }

    let mut result: Vec<usize> = neighbors.into_iter().collect();
    result.sort_unstable();
    self.neighbor_cache.insert(nodes, result.clone());
    result
}
```

🔸 對 nodes 的所有子集（從 size 1 到 size n），都去 trie 中查找鄰居。結果會被 cache，相同的 JoinSet 下次查就不用重算。

連接 edge 查找 `get_connecting_edge_indices`：

```rust
pub fn get_connecting_edge_indices(&self, left: JoinSet, right: JoinSet) -> Vec<usize> {
    if !left.is_disjoint(&right) {
        return vec![];
    }
    let mut edge_indices = std::collections::HashSet::new();
    let left_relations: Vec<usize> = left.iter().collect();
    for subset_size in 1..=left_relations.len() {
        self.find_connecting_edges_for_subsets(
            &left_relations, subset_size, right, &mut edge_indices,
        );
    }
    edge_indices.into_iter().collect()
}
```

🔸 找出連接 left 和 right 兩個不相交集合的所有 edge。這在 `emit_csg_cmp` 時會用到，判斷兩個子圖是否可以 join。

---

## 3. dp_plan.rs — DP Table Entry

路徑：`crates/sail-physical-optimizer/src/join_reorder/dp_plan.rs`

DPPlan 是 DP table 中的值，代表一個 join 子方案：

```rust
pub struct DPPlan {
    pub join_set: JoinSet,
    pub cost: f64,
    pub cardinality: f64,
    pub plan_type: PlanType,
}

pub enum PlanType {
    Leaf { relation_id: usize },
    Join {
        left_set: JoinSet,
        right_set: JoinSet,
        edge_indices: Vec<usize>,
    },
}
```

🔸 `PlanType::Leaf` 代表單表，cost 為 0。`PlanType::Join` 記錄左右子方案的 JoinSet 和使用的 edge indices，讓 reconstructor 能遞迴還原完整計畫。

建立 leaf：

```rust
pub fn new_leaf(relation_id: usize, cardinality: f64) -> Result<Self> {
    Ok(Self {
        join_set: JoinSet::new_singleton(relation_id)?,
        cost: 0.0,
        cardinality,
        plan_type: PlanType::Leaf { relation_id },
    })
}
```

🔸 leaf node 的 cost 為 0，因為 Cout 模型中掃描單表不計 join cost。

建立 join：

```rust
pub fn new_join(
    left_set: JoinSet,
    right_set: JoinSet,
    edge_indices: Vec<usize>,
    cost: f64,
    cardinality: f64,
) -> Self {
    Self {
        join_set: left_set.union(&right_set),
        cost,
        cardinality,
        plan_type: PlanType::Join { left_set, right_set, edge_indices },
    }
}
```

🔸 `join_set` 是 `left_set | right_set` 的聯集。reconstructor 會從最終的全集合 DPPlan 遞迴往下拆分。

---

## 4. cost_model.rs — Cout 成本模型

路徑：`crates/sail-physical-optimizer/src/join_reorder/cost_model.rs`

整個檔案只有一個函式，實作論文的 Cout 模型：

```rust
pub struct CostModel;

impl CostModel {
    pub fn new() -> Self { Self }

    pub fn compute_cost(
        &self,
        left_plan: &DPPlan,
        right_plan: &DPPlan,
        new_cardinality: f64,
    ) -> f64 {
        left_plan.cost + right_plan.cost + new_cardinality
    }
}
```

🔸 Cost(A ⋈ B) = Cost(A) + Cost(B) + |A ⋈ B|。new_cardinality 是 join 的輸出行數估計值。這個模型假設 join 的執行成本和輸出大小成正比。

---

## 5. cardinality_estimator.rs — 基數估算

路徑：`crates/sail-physical-optimizer/src/join_reorder/cardinality_estimator.rs`

CardinalityEstimator 負責估算 join 結果的行數，是決定 join 順序品質的關鍵。

常數定義：

```rust
const HEURISTIC_FILTER_SELECTIVITY: f64 = 0.1;
```

🔸 對於無法精確估算的非等值過濾條件，假設 selectivity 為 0.1（即保留 10% 的行）。

EquivalenceSet 表示一組等價的欄位（透過 equi-join 連接）：

```rust
pub struct EquivalenceSet {
    pub columns: HashSet<StableColumn>,
    pub t_dom_count: f64,
}
```

🔸 例如 `a.id = b.id = c.id`，則 a.id、b.id、c.id 都在同一個 EquivalenceSet 中，TDom 取其中最大的 distinct count。

初始化流程：

```rust
pub fn new(graph: QueryGraph) -> Self {
    let mut estimator = Self {
        graph,
        cardinality_cache: HashMap::new(),
        equivalence_sets: vec![],
        initial_distinct_counts: HashMap::new(),
    };
    estimator.populate_initial_distinct_counts();
    estimator.init_equivalence_sets();
    estimator
}
```

🔸 兩步初始化：先從 DataFusion 統計資訊收集各欄位的 distinct count，再根據 join edge 的 equi_pairs 建立等價集合。

收集 distinct count：

```rust
fn populate_initial_distinct_counts(&mut self) {
    for relation in &self.graph.relations {
        let relation_id = relation.relation_id;
        let column_stats = &relation.statistics.column_statistics;
        for (column_index, stats) in column_stats.iter().enumerate() {
            let distinct_count = stats.distinct_count;
            let stable_col = StableColumn {
                relation_id,
                column_index,
                name: format!("col_{}", column_index),
            };
            let count_val = match distinct_count {
                Precision::Exact(c) => c as f64,
                Precision::Inexact(c) => c as f64,
                Precision::Absent => continue,
            };
            self.initial_distinct_counts.insert(stable_col, count_val);
        }
    }
}
```

🔸 DataFusion 的統計資訊有三種精度：`Exact`（精確）、`Inexact`（近似）、`Absent`（缺失）。缺失的直接跳過。

等價集合初始化使用 Union-Find 邏輯：

```rust
fn init_equivalence_sets(&mut self) {
    let mut sets: Vec<EquivalenceSet> = vec![];
    for edge in &self.graph.edges {
        for (left_col, right_col) in &edge.equi_pairs {
            self.merge_columns_into_sets(&mut sets, left_col.clone(), right_col.clone());
        }
    }
    for set in &mut sets {
        self.estimate_tdom_for_set(set);
    }
    self.equivalence_sets = sets;
}
```

🔸 遍歷所有 edge 的 equi_pairs，用 Union-Find 方式合併欄位到同一個等價集合。

merge 邏輯：

```rust
fn merge_columns_into_sets(
    &self,
    sets: &mut Vec<EquivalenceSet>,
    col1: StableColumn,
    col2: StableColumn,
) {
    let mut idx1 = None;
    let mut idx2 = None;
    for (i, set) in sets.iter().enumerate() {
        if set.contains(&col1) { idx1 = Some(i); }
        if set.contains(&col2) { idx2 = Some(i); }
    }
    match (idx1, idx2) {
        (Some(i1), Some(i2)) => {
            if i1 != i2 {
                let (smaller_idx, larger_idx) = if i1 < i2 { (i1, i2) } else { (i2, i1) };
                let set_to_merge = sets.remove(larger_idx);
                for col in set_to_merge.columns {
                    sets[smaller_idx].add_column(col);
                }
            }
        }
        (Some(i), None) => { sets[i].add_column(col2); }
        (None, Some(i)) => { sets[i].add_column(col1); }
        (None, None) => {
            let mut new_set = EquivalenceSet::new();
            new_set.add_column(col1);
            new_set.add_column(col2);
            sets.push(new_set);
        }
    }
}
```

🔸 四種情況：兩個欄位都已在某個集合中（合併集合）、一個在一個不在（加入現有集合）、都不在（新建集合）。`remove(larger_idx)` 先移除較大 index 的集合，避免 index 錯位。

TDom 估算：

```rust
fn estimate_tdom_for_set(&self, set: &mut EquivalenceSet) {
    let mut max_distinct_count = 1.0;
    for stable_col in &set.columns {
        if let Some(distinct_count) = self.initial_distinct_counts.get(stable_col) {
            if *distinct_count > max_distinct_count {
                max_distinct_count = *distinct_count;
            }
        } else {
            if let Some(relation) = self.graph.get_relation(stable_col.relation_id) {
                let card = relation.initial_cardinality;
                if card > max_distinct_count {
                    max_distinct_count = card;
                }
            }
        }
    }
    set.set_t_dom_count(max_distinct_count);
}
```

🔸 TDom 取集合中所有欄位的最大 distinct count。如果沒有統計資訊，fallback 到表的總行數作為保守估計。

多表 join 基數估算：

```rust
fn estimate_multi_relation_cardinality(&self, join_set: JoinSet) -> f64 {
    let numerator = join_set
        .iter()
        .map(|id| {
            self.graph.get_relation(id)
                .map(|r| r.initial_cardinality)
                .unwrap_or(1.0)
        })
        .product::<f64>();

    let mut denominator = 1.0;
    let contained_edges = self.get_edges_contained_in_set(join_set);
    for edge in contained_edges {
        let tdom = self.get_tdom_for_edge(edge);
        if tdom > 1.0 {
            denominator *= tdom;
        }
    }
    numerator / denominator
}
```

🔸 公式：`|R1 ⋈ R2 ⋈ ... ⋈ Rn| = (|R1| * |R2| * ... * |Rn|) / (TDom_1 * TDom_2 * ...)`。分子是所有表行數的乘積，分母是所有完全包含在 join_set 內的 edge 的 TDom 乘積。

兩表 join 的基數估算（enumerator 使用）：

```rust
pub fn estimate_join_cardinality(
    &self,
    left_card: f64,
    right_card: f64,
    connecting_edge_indices: &[usize],
) -> f64 {
    let mut selectivity = 1.0;
    for &index in connecting_edge_indices {
        let edge = &self.graph.edges[index];
        let tdom = self.get_tdom_for_edge(edge);
        if tdom > 1.0 {
            selectivity *= 1.0 / tdom;
        } else {
            selectivity *= HEURISTIC_FILTER_SELECTIVITY;
        }
        if self.has_non_equi_filter(edge) {
            selectivity *= HEURISTIC_FILTER_SELECTIVITY;
        }
    }
    left_card * right_card * selectivity
}
```

🔸 `selectivity = 1/TDom` 是 equi-join 的選擇率公式。如果 edge 還有非等值條件（例如 `a.price > 100`），額外乘以 0.1 的啟發式 selectivity。

非等值 filter 偵測：

```rust
fn has_non_equi_filter(&self, edge: &JoinEdge) -> bool {
    fn count_conditions(expr: &Arc<dyn PhysicalExpr>) -> usize {
        if let Some(binary_expr) = expr.as_any().downcast_ref::<BinaryExpr>() {
            if binary_expr.op() == &Operator::And {
                return count_conditions(binary_expr.left())
                    + count_conditions(binary_expr.right());
            }
        }
        1
    }
    let condition_count = count_conditions(&edge.filter);
    condition_count > edge.equi_pairs.len()
}
```

🔸 遞迴數 AND 連接的條件數量，如果大於 equi_pairs 的數量，代表有額外的非等值條件。

---

## 6. builder.rs — 從 ExecutionPlan 建立 Query Graph

路徑：`crates/sail-physical-optimizer/src/join_reorder/builder.rs`

GraphBuilder 負責把 DataFusion 的 physical plan tree 轉換成 QueryGraph。

ColumnMap 追蹤每個欄位的來源：

```rust
pub type ColumnMap = Vec<ColumnMapEntry>;

pub enum ColumnMapEntry {
    Stable {
        relation_id: usize,
        column_index: usize,
    },
    Expression {
        expr: Arc<dyn PhysicalExpr>,
        input_map: ColumnMap,
    },
}
```

🔸 `Stable` 表示直接來自某張 base table 的欄位。`Expression` 表示透過運算產生的欄位（例如 `a + b`），需要保存 expression 和它的 input context，以便 reorder 後能正確重寫。

GraphBuilder 結構：

```rust
pub struct GraphBuilder {
    graph: QueryGraph,
    relation_counter: usize,
    expr_to_stable_id: HashMap<Column, (usize, usize)>,
}
```

🔸 `relation_counter` 用來分配唯一的 relation_id。`expr_to_stable_id` 快取已知的 column → stable ID 對應。

入口 `build()`：

```rust
pub fn build(
    &mut self,
    plan: Arc<dyn ExecutionPlan>,
) -> Result<Option<(QueryGraph, ColumnMap)>> {
    let result = self.visit_plan(plan);
    if let Ok(original_column_map) = result {
        if self.graph.relation_count() >= 2 {
            Ok(Some((self.graph.clone(), original_column_map)))
        } else {
            Ok(None)
        }
    } else {
        Ok(None)
    }
}
```

🔸 從根節點開始遞迴遍歷。如果找到 >= 2 個 relation（有 join 可以重排），回傳 graph 和 column map。否則回傳 None。

核心遞迴 `visit_plan()`：

```rust
fn visit_plan(&mut self, plan: Arc<dyn ExecutionPlan>) -> Result<ColumnMap> {
    let any_plan = plan.as_any();

    if let Some(join_plan) = any_plan.downcast_ref::<HashJoinExec>() {
        if join_plan.join_type() == &JoinType::Inner {
            return self.visit_inner_join(join_plan);
        }
    }

    if let Some(proj_plan) = any_plan.downcast_ref::<ProjectionExec>() {
        return self.visit_projection(proj_plan);
    }

    if any_plan.is::<AggregateExec>() {
        return Err(DataFusionError::Internal(
            "AggregateExec is not part of a reorderable join region".to_string(),
        ));
    }

    self.visit_boundary_or_leaf(plan)
}
```

🔸 `as_any().downcast_ref::<T>()` 是 Rust 的型別轉換模式。只有 Inner join 可以 reorder，其他 join type 作為邊界停止。AggregateExec 也是邊界。其他節點（DataSourceExec 等）作為 leaf 加入 graph。

處理 Inner Join：

```rust
fn visit_inner_join(&mut self, join_plan: &HashJoinExec) -> Result<ColumnMap> {
    let left_map = self.visit_plan(join_plan.left().clone())?;
    let right_map = self.visit_plan(join_plan.right().clone())?;

    let mut all_relations_in_condition = JoinSet::default();
    let mut equi_pairs = Vec::new();

    for (left_on, right_on) in join_plan.on() {
        let left_stable_ids = self.resolve_expr_to_relations(left_on, &left_map)?;
        let right_stable_ids = self.resolve_expr_to_relations(right_on, &right_map)?;

        for rel_id in left_stable_ids.iter().chain(right_stable_ids.iter()) {
            all_relations_in_condition =
                all_relations_in_condition.union(&JoinSet::new_singleton(*rel_id)?);
        }

        if let (Some(left_stable_col), Some(right_stable_col)) = (
            self.resolve_to_single_stable_col(left_on, &left_map)?,
            self.resolve_to_single_stable_col(right_on, &right_map)?,
        ) {
            equi_pairs.push((left_stable_col, right_stable_col));
        }
    }

    // 處理 ON clause 和額外 filter
    let mut filter_expr = self.build_conjunction_from_on(join_plan.on())?;
    if let Some(join_filter) = join_plan.filter() {
        let extra = self.rewrite_join_filter_to_stable(
            join_plan, join_filter.expression(), &left_map, &right_map,
        )?;
        filter_expr = Arc::new(BinaryExpr::new(filter_expr, Operator::And, extra));
    }

    let edge = JoinEdge::new(all_relations_in_condition, filter_expr, *join_plan.join_type(), equi_pairs);
    self.graph.add_edge(edge)?;

    // 輸出 ColumnMap 是左右合併
    let mut output_map = left_map;
    output_map.extend(right_map);

    // 處理 HashJoinExec 的 projection（column 重排/裁剪）
    if let Some(projection) = join_plan.projection.as_ref() {
        let mut projected: ColumnMap = Vec::with_capacity(projection.len());
        for &idx in projection.iter() {
            projected.push(output_map.get(idx).cloned().ok_or_else(|| {
                DataFusionError::Internal(format!(
                    "HashJoinExec projection index {} out of bounds (len {})",
                    idx, output_map.len()
                ))
            })?);
        }
        output_map = projected;
    }
    Ok(output_map)
}
```

🔸 這是最長的函式。先遞迴建左右子圖，然後解析 join 條件建立 JoinEdge。`join_plan.on()` 回傳 equi-join pairs，`join_plan.filter()` 回傳非等值條件。特別注意 `join_plan.projection` 的處理——HashJoinExec 可以內嵌一個 projection 來裁剪輸出欄位，ColumnMap 必須相應調整。

非等值 filter 的 stable name 重寫：

```rust
fn rewrite_join_filter_to_stable(
    &self,
    join_plan: &HashJoinExec,
    expr: &Arc<dyn PhysicalExpr>,
    left_map: &ColumnMap,
    right_map: &ColumnMap,
) -> Result<Arc<dyn PhysicalExpr>> {
    let filter = join_plan.filter().ok_or_else(|| ...)?;
    let indices = filter.column_indices();

    let transformed = expr_arc.transform(|node| {
        if let Some(col) = node.as_any().downcast_ref::<Column>() {
            let i = col.index();
            let ci = &indices[i];
            let stable_entry_opt = match ci.side {
                JoinSide::Left => left_map.get(ci.index),
                JoinSide::Right => right_map.get(ci.index),
                _ => None,
            };
            if let Some(ColumnMapEntry::Stable { relation_id, column_index }) = stable_entry_opt.cloned() {
                let stable_name = format!("R{}.C{}", relation_id, column_index);
                let new_col = Column::new(&stable_name, 0);
                return Ok(Transformed::yes(Arc::new(new_col)));
            }
        }
        Ok(Transformed::no(node))
    })?;
    Ok(transformed.data)
}
```

🔸 JoinFilter 中的 Column 使用 local index（在 filter 的 intermediate schema 中的位置），而非 global index。透過 `filter.column_indices()` 可以知道每個 local index 對應到左或右輸入的哪個欄位。然後把 Column name 改寫成 `R{relation_id}.C{column_index}` 的穩定格式，讓 reconstructor 在 reorder 後能正確還原。

`visit_projection` 穿透 Projection 節點：

```rust
fn visit_projection(&mut self, proj_plan: &ProjectionExec) -> Result<ColumnMap> {
    let input_map = self.visit_plan(proj_plan.input().clone())?;
    let mut output_map = Vec::with_capacity(proj_plan.expr().len());
    for proj_expr in proj_plan.expr() {
        let expr = &proj_expr.expr;
        if let Some(col) = expr.as_any().downcast_ref::<Column>() {
            let entry = input_map.get(col.index()).cloned().ok_or_else(|| ...)?;
            output_map.push(entry);
        } else {
            output_map.push(ColumnMapEntry::Expression {
                expr: expr.clone(),
                input_map: input_map.clone(),
            });
        }
    }
    Ok(output_map)
}
```

🔸 Projection 是透明的——如果 expression 是簡單的 Column 引用（例如 `SELECT a`），直接透傳 stable ID。如果是複雜 expression（例如 `SELECT a + 1`），保存為 `ColumnMapEntry::Expression` 以便之後重寫。

建立 relation node：

```rust
fn create_relation_node(&mut self, plan: Arc<dyn ExecutionPlan>) -> Result<ColumnMap> {
    let relation_id = self.relation_counter;
    self.relation_counter += 1;

    let stats = plan.partition_statistics(None)?;
    let initial_cardinality = match stats.num_rows {
        Precision::Exact(count) => count as f64,
        Precision::Inexact(count) => count as f64,
        Precision::Absent => 1000.0,
    };

    let relation_node = RelationNode::new(plan.clone(), relation_id, initial_cardinality, stats);
    self.graph.add_relation(relation_node);

    let mut output_map = Vec::with_capacity(plan.schema().fields().len());
    for i in 0..plan.schema().fields().len() {
        let entry = ColumnMapEntry::Stable { relation_id, column_index: i };
        output_map.push(entry);
        let col_expr = Column::new(plan.schema().field(i).name(), i);
        self.expr_to_stable_id.insert(col_expr, (relation_id, i));
    }
    Ok(output_map)
}
```

🔸 為每個欄位建立 stable entry 並記入 `expr_to_stable_id` 快取。`Precision::Absent` 時使用預設 1000 行。

---

## 7. enumerator.rs — DPHyp 核心演算法

路徑：`crates/sail-physical-optimizer/src/join_reorder/enumerator.rs`

PlanEnumerator 實作 DPHyp 演算法的核心迴圈。

```rust
pub struct PlanEnumerator {
    pub query_graph: QueryGraph,
    pub dp_table: HashMap<JoinSet, Arc<DPPlan>>,
    cardinality_estimator: CardinalityEstimator,
    cost_model: CostModel,
    emit_count: usize,
}

const EMIT_THRESHOLD: usize = 10000;
const RELATION_THRESHOLD: usize = 10;
```

🔸 `emit_count` 追蹤已生成的 plan 數量。超過 `EMIT_THRESHOLD`（10,000）就放棄 DP 改用 greedy。`RELATION_THRESHOLD`（10）以上會啟用啟發式剪枝。

主函式 `solve()`：

```rust
pub fn solve(&mut self) -> Result<Option<Arc<DPPlan>>> {
    let relation_count = self.query_graph.relation_count();
    if relation_count == 0 {
        return Err(DataFusionError::Internal("Cannot solve empty query graph".to_string()));
    }

    self.init_leaf_plans()?;

    if !self.join_reorder_by_dphyp()? {
        return Ok(None);  // 超過 threshold，呼叫者會 fallback 到 greedy
    }

    let all_relations_set = self.create_all_relations_set();
    if let Some(result) = self.dp_table.get(&all_relations_set).cloned() {
        Ok(Some(result))
    } else {
        let greedy_plan = self.solve_greedy()?;
        Ok(Some(greedy_plan))
    }
}
```

🔸 三步：初始化 leaf → 跑 DPHyp → 取出全集合的最佳 plan。如果 DPHyp 超限或找不到全集合的 plan，fallback 到 greedy。

初始化 leaf plans：

```rust
fn init_leaf_plans(&mut self) -> Result<()> {
    for relation in &self.query_graph.relations {
        let relation_id = relation.relation_id;
        let join_set = JoinSet::new_singleton(relation_id)?;
        let cardinality = self.cardinality_estimator.estimate_cardinality(join_set)?;
        let plan = Arc::new(DPPlan::new_leaf(relation_id, cardinality)?);
        self.dp_table.insert(join_set, plan);
    }
    Ok(())
}
```

🔸 為每個單表建立初始 DPPlan，cost 為 0，cardinality 從 estimator 取得。

DPHyp 主迴圈：

```rust
fn join_reorder_by_dphyp(&mut self) -> Result<bool> {
    for idx in (0..self.query_graph.relation_count()).rev() {
        if !self.process_node_as_start(idx)? {
            return Ok(false);
        }
    }
    Ok(true)
}
```

🔸 從最大 ID 往最小遍歷。逆序保證 forbidden set 的語義正確（每個 relation 只作為 "起始點" 被處理一次）。

以單個 relation 為起點開始列舉：

```rust
fn process_node_as_start(&mut self, idx: usize) -> Result<bool> {
    let nodes = JoinSet::new_singleton(idx)?;
    if !self.emit_csg(nodes)? {
        return Ok(false);
    }
    let forbidden = JoinSet::from_iter(0..idx)? | nodes;
    if !self.enumerate_csg_rec(nodes, forbidden)? {
        return Ok(false);
    }
    Ok(true)
}
```

🔸 `forbidden` 包含所有 ID < idx 的 relation 加上自身。這確保每對 CSG-CMP 只被列舉一次。

EmitCSG — 列舉一個 CSG 的所有 CMP：

```rust
fn emit_csg(&mut self, nodes: JoinSet) -> Result<bool> {
    if nodes.cardinality() as usize == self.query_graph.relation_count() {
        return Ok(true);
    }

    let min_idx = nodes.iter().min().unwrap_or(0);
    let forbidden = nodes | JoinSet::from_iter(0..min_idx)?;

    let neighbors = self.neighbors(nodes, forbidden);
    if neighbors.is_empty() {
        return Ok(true);
    }

    let neighbors_set = JoinSet::from_iter(neighbors.iter().copied())?;
    let mut enriched_forbidden = forbidden | neighbors_set;

    for &nbr in neighbors.iter().rev() {
        let nbr_set = JoinSet::new_singleton(nbr)?;
        let edge_indices = self.query_graph.get_connecting_edge_indices(nodes, nbr_set);

        if !edge_indices.is_empty()
            && !self.try_emit_csg_cmp(nodes, nbr_set, edge_indices.clone())?
        {
            return Ok(false);
        }

        if !self.enumerate_cmp_rec(nodes, nbr_set, enriched_forbidden)? {
            return Ok(false);
        }

        enriched_forbidden -= nbr_set;
    }
    Ok(true)
}
```

🔸 對 CSG `nodes` 的每個鄰居，嘗試直接 join（EmitCSGCMP），然後遞迴擴展 CMP（EnumerateCMPRec）。`enriched_forbidden` 避免重複列舉。

EnumerateCSGRec — 擴展 CSG：

```rust
fn enumerate_csg_rec(&mut self, nodes: JoinSet, forbidden: JoinSet) -> Result<bool> {
    let mut neighbors = self.neighbors(nodes, forbidden);
    if neighbors.is_empty() {
        return Ok(true);
    }

    // 啟發式剪枝
    if self.query_graph.relation_count() >= RELATION_THRESHOLD {
        let limit = nodes.cardinality() as usize;
        if neighbors.len() > limit {
            neighbors.truncate(limit);
        }
    }

    let all_subsets = self.generate_all_nonempty_subsets(&neighbors);
    let mut union_sets: Vec<JoinSet> = Vec::with_capacity(all_subsets.len());
    for subset in all_subsets {
        let subset_join_set = JoinSet::from_iter(subset.iter().copied())?;
        let new_set = nodes | subset_join_set;
        if self.dp_table.contains_key(&new_set)
            && new_set.cardinality() > nodes.cardinality()
            && !self.emit_csg(new_set)?
        {
            return Ok(false);
        }
        union_sets.push(new_set);
    }

    let neighbors_set = JoinSet::from_iter(neighbors.iter().copied())?;
    let new_forbidden = forbidden | neighbors_set;

    for set in union_sets {
        if !self.enumerate_csg_rec(set, new_forbidden)? {
            return Ok(false);
        }
    }
    Ok(true)
}
```

🔸 列舉鄰居的所有非空子集，加入到當前 CSG 中。如果新的擴展集合在 DP table 中已有 plan（代表它之前作為某次 join 的結果被計算過），就對它呼叫 EmitCSG。10 表以上時啟用剪枝：鄰居數量限制為當前 CSG 大小。

EnumerateCMPRec — 擴展 CMP：

```rust
fn enumerate_cmp_rec(
    &mut self,
    left: JoinSet,
    right: JoinSet,
    forbidden: JoinSet,
) -> Result<bool> {
    let mut neighbor_ids = self.neighbors(right, forbidden);
    if neighbor_ids.is_empty() {
        return Ok(true);
    }

    if self.query_graph.relation_count() >= RELATION_THRESHOLD {
        let limit = right.cardinality() as usize;
        if neighbor_ids.len() > limit {
            neighbor_ids.truncate(limit);
        }
    }

    let all_subsets = self.generate_all_nonempty_subsets(&neighbor_ids);
    let mut union_sets: Vec<JoinSet> = Vec::with_capacity(all_subsets.len());
    for subset in all_subsets {
        let subset_join_set = JoinSet::from_iter(subset.iter().copied())?;
        let combined = right | subset_join_set;
        if combined.cardinality() > right.cardinality() && self.dp_table.contains_key(&combined) {
            let edge_indices = self.query_graph.get_connecting_edge_indices(left, combined);
            if !edge_indices.is_empty()
                && !self.try_emit_csg_cmp(left, combined, edge_indices.clone())?
            {
                return Ok(false);
            }
        }
        union_sets.push(combined);
    }

    let neighbors_set = JoinSet::from_iter(neighbor_ids.iter().copied())?;
    let new_forbidden = forbidden | neighbors_set;

    for set in union_sets {
        if !self.enumerate_cmp_rec(left, set, new_forbidden)? {
            return Ok(false);
        }
    }
    Ok(true)
}
```

🔸 類似 EnumerateCSGRec，但擴展的是 right（CMP）側。每次擴展後檢查 left 和 combined 之間是否有 edge，有的話就 emit。

EmitCSGCMP — 計算一組 CSG-CMP pair 的 cost：

```rust
fn emit_csg_cmp(
    &mut self,
    left: JoinSet,
    right: JoinSet,
    edge_indices: &[usize],
) -> Result<f64> {
    let parent = left | right;

    let left_plan = match self.dp_table.get(&left) {
        Some(p) => p.clone(),
        None => return Ok(f64::INFINITY),
    };
    let right_plan = match self.dp_table.get(&right) {
        Some(p) => p.clone(),
        None => return Ok(f64::INFINITY),
    };

    let new_cardinality = self.cardinality_estimator.estimate_join_cardinality(
        left_plan.cardinality,
        right_plan.cardinality,
        edge_indices,
    );
    let new_cost = self.cost_model.compute_cost(&left_plan, &right_plan, new_cardinality);

    let new_plan = Arc::new(DPPlan::new_join(
        left, right, edge_indices.to_vec(), new_cost, new_cardinality,
    ));

    let should_update = match self.dp_table.get(&parent) {
        Some(existing) => new_plan.cost < existing.cost,
        None => true,
    };

    if should_update {
        self.dp_table.insert(parent, new_plan);
    }
    Ok(new_cost)
}
```

🔸 核心 DP 更新邏輯：算出 join cost，如果比 DP table 中已有的更好就更新。`parent = left | right` 是兩個子集合的聯集。

Greedy fallback：

```rust
pub fn solve_greedy(&mut self) -> Result<Arc<DPPlan>> {
    self.init_leaf_plans()?;
    let mut current_plans: Vec<JoinSet> = (0..relation_count)
        .map(JoinSet::new_singleton)
        .collect::<Result<Vec<_>, _>>()?;

    while current_plans.len() > 1 {
        let mut best_cost = f64::INFINITY;
        let mut best_plan: Option<Arc<DPPlan>> = None;

        for i in 0..current_plans.len() {
            for j in (i + 1)..current_plans.len() {
                if !self.are_subsets_connected(current_plans[i], current_plans[j]) {
                    continue;
                }
                // ... 計算 cost，選最小的
            }
        }

        // Cartesian product fallback（penalty = 1,000,000）
        if best_plan.is_none() && current_plans.len() >= 2 {
            // 選 cardinality 乘積最小的一對
            let cartesian_penalty = 1000000.0;
            // ...
        }

        // 合併最佳 pair
        current_plans.remove(best_right_idx);
        current_plans.remove(best_left_idx);
        current_plans.push(plan.join_set);
    }
    // ...
}
```

🔸 O(n^3) greedy 演算法：每次從所有剩餘 plan 中找 cost 最小的一對 join。如果沒有 connected pair 就做 cartesian product，加 1,000,000 懲罰分。

---

## 8. reconstructor.rs — 從 DPPlan 還原 ExecutionPlan

路徑：`crates/sail-physical-optimizer/src/join_reorder/reconstructor.rs`

PlanReconstructor 把 DP solver 找到的最佳 DPPlan 轉換回 DataFusion 的 physical ExecutionPlan。這是最長的檔案（~1270 行），因為需要處理各種 join 類型和 column mapping。

```rust
pub struct PlanReconstructor<'a> {
    dp_table: &'a HashMap<JoinSet, Arc<DPPlan>>,
    query_graph: &'a QueryGraph,
    plan_cache: HashMap<JoinSet, (Arc<dyn ExecutionPlan>, ColumnMap)>,
    pending_filters: Vec<PendingFilter>,
}

struct PendingFilter {
    expr: Arc<dyn PhysicalExpr>,
    required_relations: JoinSet,
}
```

🔸 `plan_cache` 避免重複構建同一個子計畫。`pending_filters` 存放因為依賴的 relation 還未 join 而暫時無法套用的 filter。

主函式 `reconstruct()`：

```rust
pub fn reconstruct(&mut self, dp_plan: &DPPlan) -> Result<(Arc<dyn ExecutionPlan>, ColumnMap)> {
    if let Some(cached) = self.plan_cache.get(&dp_plan.join_set) {
        return Ok(cached.clone());
    }

    let mut result = match &dp_plan.plan_type {
        PlanType::Leaf { relation_id } => self.reconstruct_leaf(*relation_id)?,
        PlanType::Join { left_set, right_set, edge_indices } =>
            self.reconstruct_join(*left_set, *right_set, edge_indices)?,
    };

    // 根節點：套用所有剩餘的 pending filters
    if dp_plan.join_set.cardinality() as usize == self.query_graph.relation_count() {
        if !self.pending_filters.is_empty() {
            let (plan, col_map) = &result;
            match self.apply_remaining_pending_filters(plan.clone(), col_map)? {
                Some(new_plan) => {
                    result = (new_plan, col_map.clone());
                    self.pending_filters.clear();
                }
                None => {}
            }
        }
    }

    self.plan_cache.insert(dp_plan.join_set, result.clone());
    Ok(result)
}
```

🔸 遞迴重建。到達根節點時，把所有 pending filter 包成 FilterExec 套在最外層，確保所有 predicate 都不遺漏。

`reconstruct_join()` 選擇 join 實作：

```rust
fn reconstruct_join(
    &mut self,
    left_set: JoinSet,
    right_set: JoinSet,
    edge_indices: &[usize],
) -> Result<(Arc<dyn ExecutionPlan>, ColumnMap)> {
    let (left_plan, left_map) = self.reconstruct(left_dp_plan)?;
    let (right_plan, right_map) = self.reconstruct(right_dp_plan)?;

    let on_conditions = self.build_join_conditions(edge_indices, &left_map, &right_map, &left_plan, &right_plan)?;
    let join_filter = self.build_join_filter(edge_indices, &left_map, &right_map, &left_plan, &right_plan, left_set, right_set)?;

    let mut join_output_map = left_map;
    join_output_map.extend(right_map);

    if on_conditions.is_empty() {
        if let Some(join_filter) = join_filter {
            // Theta join → NestedLoopJoinExec
            let join_plan = Arc::new(NestedLoopJoinExec::try_new(...)?);
            return Ok((join_plan, join_output_map));
        }
        // Cartesian product → CrossJoinExec
        let join_plan = Arc::new(CrossJoinExec::new(left_plan, right_plan));
        return Ok((join_plan, join_output_map));
    }

    // Equi-join → HashJoinExec
    let join_plan = Arc::new(HashJoinExec::try_new(
        left_plan, right_plan, on_conditions,
        join_filter, &join_type, None,
        PartitionMode::Auto, NullEquality::NullEqualsNothing,
    )?);
    Ok((join_plan, join_output_map))
}
```

🔸 三種 join 實作的選擇：有 equi-join pair → HashJoinExec，只有 theta join → NestedLoopJoinExec，完全沒 predicate → CrossJoinExec。

`build_join_conditions()` 還原 equi-join 條件：

```rust
fn build_join_conditions(
    &self,
    edge_indices: &[usize],
    left_map: &ColumnMap,
    right_map: &ColumnMap,
    left_plan: &Arc<dyn ExecutionPlan>,
    right_plan: &Arc<dyn ExecutionPlan>,
) -> Result<JoinConditionPairs> {
    let mut on_conditions = vec![];
    for &edge_index in edge_indices {
        let edge = self.query_graph.edges.get(edge_index)?;
        for (col1_stable, col2_stable) in &edge.equi_pairs {
            let col1_left_idx = find_physical_index(col1_stable, left_map);
            let col1_right_idx = find_physical_index(col1_stable, right_map);
            let col2_left_idx = find_physical_index(col2_stable, left_map);
            let col2_right_idx = find_physical_index(col2_stable, right_map);

            let (left_col_expr, right_col_expr) =
                if let (Some(left_idx), Some(right_idx)) = (col1_left_idx, col2_right_idx) {
                    // col1 在左，col2 在右
                    (Column::new(left_name, left_idx), Column::new(right_name, right_idx))
                } else if let (Some(left_idx), Some(right_idx)) = (col2_left_idx, col1_right_idx) {
                    // col2 在左，col1 在右
                    (Column::new(left_name, left_idx), Column::new(right_name, right_idx))
                } else {
                    continue;  // 不跨 left/right 的 pair，跳過
                };
            on_conditions.push((left_col_expr, right_col_expr));
        }
    }
    Ok(on_conditions)
}
```

🔸 因為 reorder 後左右表可能對調，同一組 equi pair 的兩個欄位可能換邊。所以需要同時嘗試兩種方向（col1 在左 or col2 在左）。

`build_join_filter()` 處理非等值條件：

```rust
fn build_join_filter(
    &mut self,
    edge_indices: &[usize],
    left_map: &ColumnMap,
    right_map: &ColumnMap,
    left_plan: &Arc<dyn ExecutionPlan>,
    right_plan: &Arc<dyn ExecutionPlan>,
    left_set: JoinSet,
    right_set: JoinSet,
) -> Result<Option<JoinFilter>> {
    let mut non_equi_filters: Vec<Arc<dyn PhysicalExpr>> = Vec::new();
    let current_join_set = left_set | right_set;

    // 處理當前 edge 的非等值條件
    for &edge_index in edge_indices {
        let edge = self.query_graph.edges.get(edge_index)?;
        if let Some(non_equi_expr) =
            self.remove_equi_conditions_from_filter(&edge.filter, &edge.equi_pairs)?
        {
            let sub_preds = Self::decompose_conjuncts(&non_equi_expr);
            for pred in sub_preds {
                let required = self.analyze_predicate_dependencies(&pred, ...)?;
                if required.is_subset(&current_join_set) {
                    non_equi_filters.push(pred);
                } else {
                    self.pending_filters.push(PendingFilter { expr: pred, required_relations: required });
                }
            }
        }
    }

    // 檢查之前 pending 的 filter 是否現在可以套用
    let mut applicable_pending = Vec::new();
    let mut remaining_pending = Vec::new();
    for pending in std::mem::take(&mut self.pending_filters) {
        if pending.required_relations.is_subset(&current_join_set) {
            if let Ok(rewritten_pred) = self.rewrite_pending_filter_for_current_join(...) {
                applicable_pending.push(rewritten_pred);
            }
        } else {
            remaining_pending.push(pending);
        }
    }
    self.pending_filters = remaining_pending;
    non_equi_filters.extend(applicable_pending);

    // 組裝成 JoinFilter（compact schema + 重寫 column index）
    // ...
}
```

🔸 非等值條件的處理很複雜。先從 edge filter 中移除已經作為 equi-join pair 的等值條件，剩下的用 AND 拆成獨立 predicate。每個 predicate 分析它依賴哪些 relation——如果當前 join 已包含所有依賴，就立即套用；否則存入 pending 等待後續的 join 才套用。

Stable name 解析：

```rust
fn parse_stable_name(name: &str) -> Option<(usize, usize)> {
    if !name.starts_with('R') { return None; }
    let dot = name.find('.')?;
    let rel_str = &name[1..dot];
    if !name[dot + 1..].starts_with('C') { return None; }
    let col_str = &name[dot + 2..];
    let rel = rel_str.parse::<usize>().ok()?;
    let col = col_str.parse::<usize>().ok()?;
    Some((rel, col))
}
```

🔸 解析 `R0.C1` 這種格式，取出 relation_id=0、column_index=1。builder 階段會把非等值 filter 中的 column 改寫成這種格式，reconstructor 再根據新的 join tree schema 還原成實際的 index。

---

## 9. mod.rs — 入口與最終 Projection

路徑：`crates/sail-physical-optimizer/src/join_reorder/mod.rs`

JoinReorder 實作 PhysicalOptimizerRule trait：

```rust
#[derive(Default)]
pub struct JoinReorder {}

impl PhysicalOptimizerRule for JoinReorder {
    fn optimize(
        &self,
        plan: Arc<dyn ExecutionPlan>,
        _config: &ConfigOptions,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        self.find_and_optimize_regions(plan)
    }

    fn name(&self) -> &str { "JoinReorder" }
    fn schema_check(&self) -> bool { true }
}
```

🔸 `schema_check` 回傳 true 表示 DataFusion 會在優化後驗證 schema 是否一致。

遞迴搜尋可重排的 join region：

```rust
fn find_and_optimize_regions(
    &self,
    plan: Arc<dyn ExecutionPlan>,
) -> Result<Arc<dyn ExecutionPlan>> {
    match self.try_optimize_region(plan.clone()) {
        Ok(Some(new_plan)) => return Ok(new_plan),
        Ok(None) => {}
        Err(e) => {
            warn!("JoinReorder: Optimization failed for region rooted at {} (fallback): {}", plan.name(), e);
        }
    }

    let optimized_children = plan
        .children()
        .into_iter()
        .map(|child| self.find_and_optimize_regions(child.clone()))
        .collect::<Result<Vec<_>>>()?;

    if optimized_children.is_empty() {
        Ok(plan)
    } else {
        plan.with_new_children(optimized_children)
    }
}
```

🔸 top-down 遍歷：從根節點開始嘗試在每個節點建立 join region。如果當前節點不是 reorderable join 的根，就遞迴到 children 繼續找。soft fallback 確保優化失敗不會阻斷整個查詢。

嘗試優化一個 region：

```rust
fn try_optimize_region(
    &self,
    plan: Arc<dyn ExecutionPlan>,
) -> Result<Option<Arc<dyn ExecutionPlan>>> {
    let mut graph_builder = GraphBuilder::new();
    let Some((query_graph, target_column_map)) = graph_builder.build(plan.clone())? else {
        return Ok(None);
    };

    if query_graph.relation_count() <= 2 {
        return Ok(None);
    }

    let mut enumerator = PlanEnumerator::new(query_graph);
    let best_plan = match enumerator.solve()? {
        Some(plan) => plan,
        None => enumerator.solve_greedy()?,
    };

    let mut reconstructor = PlanReconstructor::new(&enumerator.dp_table, &enumerator.query_graph);
    let (join_tree, final_map) = reconstructor.reconstruct(&best_plan)?;

    let target_names: Vec<String> = (0..plan.schema().fields().len())
        .map(|i| plan.schema().field(i).name().clone())
        .collect();

    let final_plan = self.build_final_projection(join_tree, &final_map, &target_column_map, &target_names)?;
    Ok(Some(final_plan))
}
```

🔸 完整流程：建圖 → DP solve → reconstruct → projection。只有 3 個表以上才值得 reorder（2 表只有一種順序）。

`build_final_projection` 保留原始 schema：

```rust
fn build_final_projection(
    &self,
    input_plan: Arc<dyn ExecutionPlan>,
    final_map: &ColumnMap,
    target_map: &ColumnMap,
    target_names: &[String],
) -> Result<Arc<dyn ExecutionPlan>> {
    let mut projection_exprs = vec![];

    for (output_idx, target_entry) in target_map.iter().enumerate() {
        match target_entry {
            ColumnMapEntry::Stable { relation_id, column_index } => {
                let stable_target = StableColumn { relation_id: *relation_id, column_index: *column_index, name: "".to_string() };
                let physical_idx = find_physical_index(&stable_target, final_map)?;
                let input_name = input_plan.schema().field(physical_idx).name().clone();
                let output_name = target_names.get(output_idx).cloned().unwrap_or(input_name.clone());
                projection_exprs.push((Arc::new(Column::new(&input_name, physical_idx)), output_name));
            }
            ColumnMapEntry::Expression { expr, input_map } => {
                let rewritten_expr = self.rewrite_expr_to_final_map(expr.clone(), input_map, final_map, &input_plan)?;
                let output_name = target_names.get(output_idx).cloned().unwrap_or(format!("expr_{}", output_idx));
                projection_exprs.push((rewritten_expr, output_name));
            }
        }
    }

    Ok(Arc::new(ProjectionExec::try_new(projection_exprs, input_plan)?))
}
```

🔸 reorder 後 join tree 的輸出欄位順序可能和原始計畫不同。`build_final_projection` 加一層 ProjectionExec 把欄位順序和名稱調回原樣。`target_map` 是原始計畫的 ColumnMap（builder 回傳的），`final_map` 是 reconstructor 重建的 join tree 的 ColumnMap。對每個目標欄位，在 final_map 中找到對應的 physical index，建立 Column expression。

Expression 重寫（支援巢狀 derived expression）：

```rust
fn rewrite_expr_to_final_map(
    &self,
    expr: Arc<dyn PhysicalExpr>,
    input_map: &ColumnMap,
    final_map: &ColumnMap,
    input_plan: &Arc<dyn ExecutionPlan>,
) -> Result<Arc<dyn PhysicalExpr>> {
    let mut rewritten_cache: HashMap<usize, Arc<dyn PhysicalExpr>> = HashMap::new();

    let transformed = expr.transform(|node| {
        if let Some(col) = node.as_any().downcast_ref::<Column>() {
            let original_entry = input_map.get(col.index())?;
            match original_entry {
                ColumnMapEntry::Stable { relation_id, column_index } => {
                    let new_physical_idx = find_physical_index(&stable_target, final_map)?;
                    let new_name = input_plan.schema().field(new_physical_idx).name().to_string();
                    return Ok(Transformed::yes(Arc::new(Column::new(&new_name, new_physical_idx))));
                }
                ColumnMapEntry::Expression { expr: nested_expr, input_map: nested_input_map } => {
                    if let Some(cached) = rewritten_cache.get(&col.index()) {
                        return Ok(Transformed::yes(cached.clone()));
                    }
                    let rewritten_nested = self.rewrite_expr_to_final_map(nested_expr.clone(), nested_input_map, final_map, input_plan)?;
                    rewritten_cache.insert(col.index(), rewritten_nested.clone());
                    return Ok(Transformed::yes(rewritten_nested));
                }
            }
        }
        Ok(Transformed::no(node))
    })?;
    Ok(transformed.data)
}
```

🔸 `transform()` 是 DataFusion TreeNode trait 的方法，bottom-up 遍歷 expression tree。對每個 Column，在 input_map 中查找來源——如果是 Stable 直接重寫 index，如果是 Expression 就遞迴處理。`rewritten_cache` 避免同一個 derived expression 被重寫多次。


# 總結

```
問題 → 方案 → 效果
--------------------------------------------------------------
Issue #747: Delta crash       → PR #750: 移除 DeltaScan wrapper → 可以執行
Issue #759: TPC-DS 慢         → PR #810: 設定框架               → 基礎設施
                              → PR #917: DPHyp join reorder     → 最佳 join 順序
```

Sail 的 DPHyp 實作包含以下特色：

🔸 完整的 hypergraph 支援：透過 trie-based 鄰居查找結構處理 hyperedge
🔸 安全的 fallback 機制：DP 超過 10,000 plan 閾值自動切換 greedy
🔸 Pending filter 機制：非等值條件在 join tree 重建過程中被正確延遲套用
🔸 Schema 保真：最終 ProjectionExec 確保輸出和原始計畫完全一致
🔸 Soft error handling：優化失敗不阻斷查詢，自動 fallback 到原始計畫
