# pyproject.toml 詳細解說

這份文件解釋 Sail 專案的 Python 套件配置。pysail 是 Sail 的 Python 介面，使用 Maturin 將 Rust 編譯為 Python extension。

## Project 基本資訊

```toml
[project]
name = "pysail"
version = "0.4.2"
description = "Sail Python library"
authors = [
    { name = "LakeSail", email = "hello@lakesail.com" },
]
readme = "README.md"
license = { file = "LICENSE" }
requires-python = ">=3.9,<3.14"
dependencies = []
```

🔸 name = "pysail": PyPI 上的套件名稱
🔸 requires-python: 支援 Python 3.9 到 3.13
🔸 dependencies = []: 核心套件無額外依賴，Rust extension 是自包含的

## Classifiers (PyPI 分類標籤)

```toml
classifiers = [
    "Development Status :: 4 - Beta",
    "Programming Language :: Python :: 3.9",
    ...
    "Programming Language :: Rust",
    "Topic :: Scientific/Engineering",
]
```

這些標籤用於 PyPI 搜尋和分類，表明這是 Beta 階段、支援多 Python 版本、包含 Rust 程式碼

## Optional Dependencies (可選依賴)

```toml
[project.optional-dependencies]
test = [
    "pyspark-client>=4.0,<5",
    "duckdb>=1.0,<2",
    "pytest>=8.4,<9",
    "pytest-bdd>=8.1,<9",
    "jinja2>=3.1,<4",
    "pillow>=10.3.0",
    "pyiceberg[sql-sqlite,pyiceberg-core]==0.10.0",
    "pydantic>=2.11,<2.12",
]
mcp = [
    "mcp>=1.0.0,<2",
]
```

🔸 test: 測試用依賴，安裝方式: `pip install pysail[test]`
🔸 mcp: Model Context Protocol 支援，用於 AI 應用整合

## CLI 入口點

```toml
[project.scripts]
sail = "pysail.cli:main"
```

安裝後可執行 `sail` 命令，對應 `python/pysail/cli.py` 的 main 函數

## Build System (Maturin)

```toml
[build-system]
requires = ["maturin>=1.0,<2.0"]
build-backend = "maturin"
```

🔸 Maturin 是 Rust+Python 混合專案的建構工具
🔸 負責編譯 Rust 程式碼並打包成 Python wheel

```toml
[tool.maturin]
python-source = "python"
module-name = "pysail._native"
manifest-path = "crates/sail-python/Cargo.toml"
features = [
    "pyo3/extension-module",
    "pyo3/abi3-py38",
    "pyo3/generate-import-lib",
]
```

🔸 python-source: Python 原始碼目錄
🔸 module-name: 編譯後的 native module 名稱 (`pysail._native`)
🔸 manifest-path: Rust crate 的 Cargo.toml 路徑
🔸 features:
- extension-module: 編譯為 Python extension
- abi3-py38: 使用穩定 ABI，一個 wheel 支援多 Python 版本
- generate-import-lib: Windows 相容性

## Hatch 環境管理

Hatch 是現代 Python 專案管理工具，這裡定義多個虛擬環境:

### 🔸 Default 環境 (開發用)

```toml
[tool.hatch.envs.default]
python = "3.11"
installer = "pip"
skip-install = true
dependencies = [
    "pyspark[connect]==4.0.0",
    "ibis-framework>=11,<12",
    "pytest>=8.4,<9",
    ...
]
path = ".venvs/default"
```

🔸 skip-install = true: 不自動安裝專案，因為需要先用 maturin 編譯
🔸 path: 虛擬環境存放路徑

```toml
[tool.hatch.envs.default.overrides]
env.CI.installer = "uv"
```

CI 環境使用 uv (更快的套件安裝器)，本地用 pip

```toml
[tool.hatch.envs.default.scripts]
install-pysail = "\"{env:HATCH_UV}\" pip install pysail --no-index -f target/wheels --force-reinstall"
```

自訂腳本: 從本地 wheels 安裝編譯好的 pysail

### 🔸 Coverage 環境

```toml
[tool.hatch.envs.coverage]
template = "default"
env-vars = {
    RUSTC_WORKSPACE_WRAPPER = ".github/scripts/rustc-workspace-wrapper.sh",
    LLVM_PROFILE_FILE = "target/coverage/sail-%p-%m.profraw"
}
```

繼承 default 環境，設定程式碼覆蓋率收集的環境變數

### 🔸 Test 環境 (矩陣測試)

```toml
[tool.hatch.envs.test]
matrix-name-format = "{variable}-{value}"
...

[[tool.hatch.envs.test.matrix]]
spark = ["3.5.5", "4.0.0"]

[tool.hatch.envs.test.overrides]
matrix.spark.path = [
    { value = ".venvs/test.spark-3.5.5", if = ["3.5.5"] },
    { value = ".venvs/test.spark-4.0.0", if = ["4.0.0"] },
]
matrix.spark.extra-dependencies = [
    { value = "pyspark[connect]==3.5.5", if = ["3.5.5"] },
    { value = "pyspark[connect]==4.0.0", if = ["4.0.0"] },
]
```

🔸 矩陣測試: 自動建立兩個環境測試 Spark 3.5.5 和 4.0.0
🔸 每個 Spark 版本有獨立的虛擬環境和依賴

### 🔸 Test-Spark 環境 (PySpark 整合測試)

```toml
[tool.hatch.envs.test-spark]
dependencies = [
    "pytest>=8.4,<9",
    "pytest-xdist>=3.7,<4",      # 平行測試
    "pytest-timeout>=2.4,<3",    # 測試超時
    "pytest-reportlog>=0.4,<0.5", # 測試報告
]

[tool.hatch.envs.test-spark.extra-scripts]
install-pyspark = "\"{env:HATCH_UV}\" pip install --force-reinstall 'pyspark[connect] @ opt/spark/python/dist/pyspark-{matrix:spark}.tar.gz'"
```

從本地 Spark 原始碼安裝 PySpark

### 🔸 Test-Ibis 環境

```toml
[tool.hatch.envs.test-ibis]
dependencies = [
    "pyspark[connect]==3.5.5",
    "ibis-framework[pyspark]>=11,<12",
    "hypothesis>=6.58.0,<7",      # Property-based testing
    "pytest-xdist>=2.3.0,<4",     # 平行測試
    ...
]
path = ".venvs/test-ibis"
```

Ibis DataFrame 框架相容性測試環境

## Build Targets

```toml
[tool.hatch.build.targets.sdist]
packages = ["python/pysail"]

[tool.hatch.build.targets.wheel]
packages = ["python/pysail"]
```

指定 source distribution 和 wheel 的打包目錄

## Ruff Lint 設定

```toml
[tool.ruff.lint.per-file-ignores]
"crates/**/*.py" = ["INP001"]           # 忽略 implicit namespace package
"python/pysail/docs/conf.py" = ["INP001"]
"python/pysail/examples/**/*.py" = ["T201"]  # 允許 print()
"python/pysail/tests/**/*.py" = ["S101"]     # 允許 assert
"scripts/**/*.py" = ["SLF001"]               # 允許 private member access
```

Ruff 是 Python linter，這裡設定特定檔案忽略特定規則

## Pytest 設定

```toml
[tool.pytest.ini_options]
testpaths = ["python"]
```

pytest 測試目錄設定。註解說明不要在這裡加其他設定，應該用 conftest.py hook
