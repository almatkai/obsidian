# Obsidian for Tezbar

A standalone Rust extension for Tezbar 0.3.0-beta.12. It preserves the 13 original Obsidian command IDs and supports macOS and Windows. The extension works with local vaults and the Obsidian URI handler. Advanced URI and Workspaces are required for workspace navigation.

`vaultPath` accepts comma-separated absolute vault paths. When it is empty, the extension discovers registered vaults in Obsidian's local profile. A chosen vault is displayed by path and actions target the exact vault and relative file. Configure capture paths relative to that vault.

Build and verify locally on Windows x64 or Apple Silicon macOS. Use `tezbar-obsidian.exe` on Windows. Set `CARGO_BUILD_JOBS=1` in the current shell.

```sh
cargo test --locked --features protocol-fixtures
cargo build --locked --features protocol-fixtures
python3 scripts/protocol-smoke.py --binary target/debug/tezbar-obsidian --report reports/fixture.json
cargo build --release --locked
python3 scripts/package.py --target aarch64-apple-darwin --binary target/release/tezbar-obsidian
python3 scripts/protocol-smoke.py --archive dist/obsidian-aarch64-apple-darwin-development.zip --report reports/production.json
python3 scripts/package.py --source
```

On Windows use the `x86_64-pc-windows-msvc` package target. The fixture build requires an explicit fixture switch, an owned marker and a recording-host context. The packager admits production executables with fixture support disabled. The extracted production smoke uses an explicitly absent vault inside its temporary root.

See [PORT-REPORT.md](PORT-REPORT.md), [UPSTREAM.json](UPSTREAM.json), and [the pinned Raycast source](https://github.com/raycast/extensions/tree/459326f4f557692a79246dba2d013746f56d724b/extensions/obsidian/).
