cd C:\Users\nemuliz\Downloads\DeviceHUB-clone
rustup show

REM optional checks (same as CI)
cargo check --workspace --locked
cargo test --workspace --locked

REM release binaries
cargo build --release --locked -p device-hub -p devhub-obs


start "" target\release\device-hub.exe