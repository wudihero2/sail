## 三種執行模式對 Cache 設計的影響

Sail 有三種執行模式，每種模式的 Worker 生命週期和資源共享方式不同，
對 cache 設計有直接影響。

```rust
// crates/sail-common/src/config/application.rs (line 67-81)
pub enum ExecutionMode {
    Local,               // 單 process，所有查詢在同一個 process 執行
    LocalCluster,        // 同一個 process 內跑 Driver + Workers（actor threads）
    KubernetesCluster,   // Driver pod + Worker pods（K8s 上不同 container）
}
```

🔸 三種模式一覽

```
+---------------------+---------------------------------------------------+
| 模式                | 架構                                              |
+---------------------+---------------------------------------------------+
| Local               | [ Main Process                                  ] |
|                     | [  DataFusion execute_stream() 直接跑             ] |
|                     | [  單一 RuntimeEnv, 單一 ObjectStoreRegistry     ] |
+---------------------+---------------------------------------------------+
| LocalCluster        | [ Main Process                                  ] |
|                     | [  Driver (actor thread)                         ] |
|                     | [  Worker 0 (actor thread, 共用 SessionContext)  ] |
|                     | [  Worker 1 (actor thread, 共用 SessionContext)  ] |
|                     | [  Worker 2 (actor thread, 共用 SessionContext)  ] |
|                     | [  Worker 3 (actor thread, 共用 SessionContext)  ] |
|                     | [  全部共用同一個 RuntimeEnv + ObjectStoreRegistry] |
+---------------------+---------------------------------------------------+
| KubernetesCluster   | +----------+  +----------+  +----------+         |
|                     | | Driver   |  | Worker 0 |  | Worker 1 |         |
|                     | | Pod      |  | Pod      |  | Pod      |         |
|                     | | (own     |  | (own     |  | (own     |         |
|                     | |  Runtime |  |  Runtime |  |  Runtime |         |
|                     | |  Env)    |  |  Env)    |  |  Env)    |         |
|                     | +----+-----+  +----+-----+  +----+-----+         |
|                     |      |   gRPC       |              |              |
|                     |      +------+-------+--------------+              |
+---------------------+---------------------------------------------------+
```

🔸 Worker 生命週期

```
+---------------------+------------------+-----------------------------------+
| 模式                | Worker 生命週期  | 說明                              |
+---------------------+------------------+-----------------------------------+
| Local               | 無 Worker        | 沒有 Driver/Worker 分離           |
|                     |                  | 查詢直接用 execute_stream()       |
|                     |                  | process 活多久 cache 就活多久     |
+---------------------+------------------+-----------------------------------+
| LocalCluster        | 長期存活         | Worker 是 actor thread            |
|                     | (idle 60s 回收)  | 預設啟動 4 個 Worker              |
|                     |                  | idle 超過 worker_max_idle_time    |
|                     |                  | (預設 60s) 才被回收               |
|                     |                  | 有新任務會再啟動                  |
|                     |                  | process 退出才全部消失            |
+---------------------+------------------+-----------------------------------+
| KubernetesCluster   | 短期存活         | Worker 是獨立 K8s Pod             |
|                     | (Pod 級別)       | restart_policy: "Never"           |
|                     |                  | 任務做完 + idle 超時就被關         |
|                     |                  | Driver Pod 刪除時 cascading 刪除  |
|                     |                  | Pod 重啟 = cache 全部消失         |
+---------------------+------------------+-----------------------------------+
```

Worker idle 回收的源碼：

```rust
// crates/sail-execution/src/driver/worker_pool/core.rs (line 456-472)
fn schedule_idle_worker_probe(ctx, worker_id, worker, options) {
    let WorkerState::Running { updated_at, .. } = &worker.state;
    ctx.send_with_delay(
        DriverEvent::ProbeIdleWorker { worker_id, instant: *updated_at },
        options.worker_max_idle_time,   // 預設 60 秒
    );
}
```

K8s Worker Pod 設定：

```rust
// crates/sail-execution/src/worker_manager/kubernetes.rs (line 231-244)
let mut spec = PodSpec {
    containers: vec![Container {
        name: "worker".to_string(),
        command: Some(vec!["sail".to_string()]),
        args: Some(vec!["worker".to_string()]),
        env: Some(self.build_pod_env(id, options)),
        image: Some(self.options.image.clone()),
        ..Default::default()
    }],
    restart_policy: Some("Never".to_string()),  // Pod 結束不會重啟
    ..Default::default()
};
```

🔸 Driver 生命週期

Driver 在三種模式下的角色不同：

```
+---------------------+------------------+-----------------------------------+
| 模式                | Driver 生命週期  | 說明                              |
+---------------------+------------------+-----------------------------------+
| Local               | 無 Driver        | 沒有 Driver/Worker 分離           |
|                     |                  | process 本身就是唯一的執行單元    |
+---------------------+------------------+-----------------------------------+
| LocalCluster        | 長期存活         | Driver 是 actor thread            |
|                     | (process 級別)   | process 活多久 Driver 就活多久    |
+---------------------+------------------+-----------------------------------+
| KubernetesCluster   | 長期存活         | Driver 是 gRPC server Pod         |
|                     | (Pod 級別)       | 跑 `sail spark server`            |
|                     |                  | 一直監聽 PySpark 連線             |
|                     |                  | 直到 Pod 被刪除或手動 shutdown    |
|                     |                  | Worker Pod 的 OwnerReference 指向 |
|                     |                  | Driver Pod，Driver 被刪時         |
|                     |                  | K8s cascading 自動刪所有 Worker   |
+---------------------+------------------+-----------------------------------+
```

K8s Driver 的入口：

```rust
// crates/sail-spark-connect/src/entrypoint.rs (line 17-41)
pub async fn serve(config: Arc<AppConfig>) -> Result<(), Box<dyn std::error::Error>> {
    let addr = config.spark_connect().address();
    let listener = TcpListener::bind(&addr).await?;
    // ... 建立 gRPC server
    server
        .serve_with_incoming_shutdown(
            TcpListenerStream::new(listener),
            signal::ctrl_c().map(|_| ()),    // 等到 Ctrl+C 或 SIGTERM 才關
        )
        .await?;
    Ok(())
}
```

Driver Pod 跑的是 `sail spark server`，這是一個長駐的 gRPC 服務。
它不會因為 query 結束就關閉，會一直等待新的 PySpark 連線。

Worker Pod 透過 K8s OwnerReference 綁定到 Driver Pod：

```rust
// crates/sail-execution/src/worker_manager/kubernetes.rs (line 262-270)
spec.owner_references = Some(vec![OwnerReference {
    api_version: "v1".to_string(),
    kind: "Pod".to_string(),
    name: self.options.driver_pod_name.clone(),   // 指向 Driver Pod
    uid: self.options.driver_pod_uid.clone(),
    controller: Some(true),
    block_owner_deletion: Some(true),             // 刪 Driver 前先刪 Worker
    ..Default::default()
}]);
```

這表示：
- Driver Pod 是所有 Worker Pod 的「擁有者」
- `kubectl delete pod <driver-pod>` 會自動刪除所有 Worker Pod
- Driver Pod 不會自動關閉，必須手動刪除或 Deployment 管理

對 cache 設計的影響：Driver Pod 長期存活，適合放 metadata cache（Moka，已有）。
如果未來 Driver 也參與資料掃描，可以考慮在 Driver Pod 上也啟用 Foyer disk cache。

🔸 Cache 資源共享 vs 隔離

```
+---------------------+-------------------+-----------------------------------+
| 資源                | Local /           | KubernetesCluster                 |
|                     | LocalCluster      |                                   |
+---------------------+-------------------+-----------------------------------+
| RuntimeEnv          | 共享（同 process）| 隔離（每個 Pod 獨立）             |
| ObjectStoreRegistry | 共享              | 隔離                              |
| Moka Caches         | 共享（global）    | 隔離（Pod 內 global）             |
| Memory Pool         | 共享              | 隔離                              |
| Disk Manager        | 共享 temp dir     | 各自的 ephemeral storage          |
| Foyer Cache (新增)  | 共享              | 隔離                              |
+---------------------+-------------------+-----------------------------------+
```

源碼確認：LocalCluster 的 Worker 共用同一個 SessionContext：

```rust
// crates/sail-session/src/session_factory/server.rs (line 155-163)
ExecutionMode::LocalCluster => {
    let worker_manager = Arc::new(LocalWorkerManager::new(
        self.runtime.clone(),
        // 這個 session 被所有 local workers 共用
        WorkerSessionFactory::new(self.config.clone(), self.runtime.clone())
            .create(())?,
    ));
    // ...
}
```

KubernetesCluster 的 Worker 各自建立 SessionContext：

```rust
// crates/sail-session/src/session_factory/worker.rs (line 23-34)
impl SessionFactory<()> for WorkerSessionFactory {
    fn create(&mut self, _info: ()) -> Result<SessionContext> {
        // 每次 create() 都建立獨立的 RuntimeEnv
        let runtime = self.runtime_env.create(Ok)?;
        let state = SessionStateBuilder::new()
            .with_config(SessionConfig::default())
            .with_runtime_env(runtime)   // 獨立的 RuntimeEnv -> 獨立的 ObjectStoreRegistry
            .with_default_features()
            .build();
        Ok(SessionContext::new_with_state(state))
    }
}
```

🔸 每種模式的 Cache 設計考量

Local 模式

```
+------------------------------------------+
| Main Process                             |
|                                          |
|  +------------------------------------+  |
|  | Foyer HybridCache (memory + disk)  |  |
|  | - memory: in-process heap          |  |
|  | - disk: local SSD path             |  |
|  +------------------------------------+  |
|                                          |
|  lifetime = process lifetime             |
|  所有查詢共享同一個 cache                 |
|  process 重啟 = memory cache 消失        |
|  但 disk cache 還在 (warm restart)       |
+------------------------------------------+
```

適合場景：開發、測試、單機部署
Cache 效益最大，因為所有查詢累積的 cache 都在同一個 process 裡。
Foyer disk cache 在 process 重啟後還能用（warm restart），
不需要從零開始 warm up。

LocalCluster 模式

```
+------------------------------------------------------+
| Main Process                                         |
|                                                      |
|  +------------------------------------------------+  |
|  | 共享的 Foyer HybridCache (memory + disk)       |  |
|  +------------------------------------------------+  |
|       ^         ^         ^         ^                |
|       |         |         |         |                |
|  +--------+ +--------+ +--------+ +--------+        |
|  |Worker 0| |Worker 1| |Worker 2| |Worker 3|        |
|  +--------+ +--------+ +--------+ +--------+        |
|                                                      |
|  所有 Workers 共用同一個 cache instance               |
|  Worker 0 cache 的資料，Worker 1 直接用              |
|  cache 利用率最高                                    |
|  idle 60s 回收的 Worker 不影響 cache                 |
|  （cache 活在 process 裡，不是 Worker 裡）           |
+------------------------------------------------------+
```

因為 Local Workers 都在同一個 process，共用 ObjectStoreRegistry，
所以 CachingObjectStore 被所有 Workers 共享。
一個 Worker 讀過的資料，其他 Worker 直接 cache hit。
Worker 被 idle 回收也不影響 cache。

KubernetesCluster 模式

```
+----------+     +----------+     +----------+
| Driver   |     | Worker 0 |     | Worker 1 |
| Pod      |     | Pod      |     | Pod      |
|          |     |          |     |          |
| (no      |     | +------+ |     | +------+ |
|  cache   |     | |Foyer | |     | |Foyer | |
|  needed) |     | |Cache | |     | |Cache | |
|          |     | +------+ |     | +------+ |
|          |     |   |      |     |   |      |
|          |     |  NVMe /  |     |  NVMe /  |
|          |     | emptyDir |     | emptyDir |
+----+-----+     +----+-----+     +----+-----+
     |   gRPC         |                |
     +-------+--------+----------------+
             |
        +---------+
        |   S3    |
        +---------+
```

K8s 模式的挑戰：

```
+-------------------------------+------------------------------------------+
| 問題                          | 影響                                     |
+-------------------------------+------------------------------------------+
| Pod 是短暫的                  | restart_policy: "Never"                  |
|                               | Pod 結束 = 所有 cache 消失               |
|                               | 新 Pod 要從零 warm up                   |
+-------------------------------+------------------------------------------+
| 每個 Pod 獨立 cache           | Worker 0 和 Worker 1 可能 cache 同一     |
|                               | 份資料，有空間浪費                       |
+-------------------------------+------------------------------------------+
| Disk cache 用 emptyDir        | K8s emptyDir 的生命週期 = Pod 生命週期   |
|                               | Pod 被刪除 emptyDir 也消失               |
+-------------------------------+------------------------------------------+
| Driver 不需要 data cache      | Driver 只做 plan + scheduling            |
|                               | 不讀 Parquet 資料                        |
|                               | 可能需要 metadata cache 用來做 pruning  |
+-------------------------------+------------------------------------------+
```

🔸 K8s 模式的 Cache 最佳化策略

策略 1：使用 PersistentVolume（推薦）

```
+----------+
| Worker   |
| Pod      |
|          |
| +------+ |     +------------------+
| |Foyer | +---->| PersistentVolume |  <-- 獨立於 Pod 生命週期
| |Cache | |     | (NVMe SSD)       |
| +------+ |     +------------------+
+----------+
```

用 K8s PersistentVolumeClaim (PVC) 掛載 SSD：
- Pod 重啟後 PV 還在，disk cache 不消失
- Foyer 的 disk layer 重啟後自動恢復
- 需要在 worker_pod_template 裡設定 volumeMount

```json
// kubernetes.worker_pod_template 設定範例
{
  "spec": {
    "volumes": [{
      "name": "cache-volume",
      "persistentVolumeClaim": { "claimName": "worker-cache-pvc" }
    }],
    "containers": [{
      "volumeMounts": [{
        "name": "cache-volume",
        "mountPath": "/cache"
      }]
    }]
  }
}
```

策略 2：使用 hostPath（簡單但有限制）

```
Node A:
  Worker Pod 0 --> /mnt/nvme/sail-cache/  (hostPath)
  Worker Pod 1 --> /mnt/nvme/sail-cache/  (hostPath, 需避免衝突)
```

用 K8s hostPath 掛載 node 的 SSD：
- Pod 重啟後 cache 還在（只要排到同一個 node）
- 缺點：Pod 可能被排到不同 node，cache 就無法復用
- 需要 nodeAffinity 或 topology constraints 來綁定

策略 3：接受 cold start（最簡單）

```
不做任何持久化，完全用 emptyDir。
每次 Worker Pod 啟動都是 cold cache。

適用場景：
- 大部分查詢不重複
- S3 延遲可接受
- 不想增加 PV 管理成本
```

🔸 三種模式的 Cache 設定建議

```
+---------------------+------------------+----------------------------+
| 模式                | memory cache     | disk cache                 |
+---------------------+------------------+----------------------------+
| Local               | 256 MB - 1 GB    | 10-50 GB (local SSD path)  |
|                     | (long-lived      | (warm restart across       |
|                     |  process)        |  process restarts)         |
+---------------------+------------------+----------------------------+
| LocalCluster        | 256 MB - 1 GB    | 10-50 GB (local SSD path)  |
|                     | (共享，高效率)   | (所有 Workers 共享)        |
+---------------------+------------------+----------------------------+
| KubernetesCluster   | 64 MB - 256 MB   | 取決於 volume 策略：       |
|                     | (per Pod，       | - PVC: 10-50 GB (持久)    |
|                     |  較保守避免 OOM) | - emptyDir: 5-20 GB       |
|                     |                  | - hostPath: 10-50 GB      |
+---------------------+------------------+----------------------------+
```

🔸 設定介面建議

```yaml
# application.yaml 新增
cache:
  object_store:
    enabled: true
    memory_size: "256MB"      # Foyer memory layer
    disk_enabled: true
    disk_path: "/cache"       # K8s 裡由 volumeMount 決定
    disk_size: "10GB"         # Foyer disk layer
    part_size: "1MB"          # 固定 part 大小
    ttl: "1h"                 # TTL 保底
    admission_policy: "always"  # always / size_threshold / rate_limit
```

K8s 模式下 disk_path 應該對應到 PVC 或 emptyDir 的 mountPath。
Local / LocalCluster 模式下可以直接指向 SSD 路徑如 `/mnt/nvme/sail-cache`。

🔸 Driver 的 Cache 需求

```
+---------------------+-------------------------------------------+
| 模式                | Driver 需要 cache 嗎？                    |
+---------------------+-------------------------------------------+
| Local               | 不適用（沒有 Driver/Worker 分離）         |
+---------------------+-------------------------------------------+
| LocalCluster        | 共享同一個 process，自然共享 cache         |
+---------------------+-------------------------------------------+
| KubernetesCluster   | Driver 主要做：                           |
|                     | - 讀 Delta/Iceberg metadata（需要 cache）|
|                     | - 做 plan 和 scheduling（不讀資料）      |
|                     | - 分配 task 給 Worker                    |
|                     |                                           |
|                     | Driver Pod 需要：                         |
|                     | - File metadata cache (Moka, 已有)       |
|                     | - File listing cache (Moka, 已有)        |
|                     | - 不需要 Foyer data cache（不讀資料）    |
|                     |   Driver 只做 plan + scheduling          |
|                     |   資料掃描全部在 Worker Pod 上執行       |
+---------------------+-------------------------------------------+
```