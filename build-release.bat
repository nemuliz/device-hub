cd C:\Users\nemuliz\Downloads\DeviceHUB-clone
rustup show

# optional checks (same as CI)
cargo check --workspace --locked
cargo test --workspace --locked

# release binaries
cargo build --release --locked -p device-hub -p devhub-obs