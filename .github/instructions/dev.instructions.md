---
applyTo: '**/*.rs'
---
Whenever you make a code change make sure you run the following commands:

```bash
cargo fmt
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
```
