# damper

damper is a macOS broker that sizes a machine-wide cargo jobserver pool from
memory pressure, so parallel builds slow down before the machine does.

**Status: pre-release, Phase 1.** The engine works and is built from source;
there is no packaged install yet. See the [usage guide](docs/usage.md) for the
build, the configuration and the commands. The design is in
[docs/superpowers/specs/2026-11-08-damper-design.md](docs/superpowers/specs/2026-11-08-damper-design.md).

```sh
cargo build --release
mkdir -p ~/.local/share/damper/shim && ln -sf "$(pwd)/target/release/damper-shim" ~/.local/share/damper/shim/cargo
export PATH="$HOME/.local/share/damper/shim:$PATH"
target/release/damperd &   # then: target/release/damper doctor
```

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your
option.
