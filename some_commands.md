```
cargo +night fmt &&
    cargo clippy --all-targets --all-features
```
```
hatch fmt

hatch run pytest --pyargs pysail

hatch run test:pytest --pyargs pysail

hatch run maturin build
```

```
git remote add upstream git@github.com:lakehq/sail.git
git fetch upstream
git fetch upstream pull/1289/head:pr-1289
```

