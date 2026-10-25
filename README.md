# Toolsy
Toolsy is a Windows app for keeping a list of installed programs and installing or upgrading them in one go. It supports winget, Scoop, uv, Chocolatey, Cargo, and custom download URLs.

## Documentation

For a comprehensive guide to using Toolsy, see the [user manual](doc/readme.md).

## Building

To build, you'll need Cargo, as well as CMake and Ninja for building wxDragon. You also need LLVM from [LLVM.org](https://llvm.org/).

```
cargo build --release
```

This generates the executable at `target/release/toolsy.exe`.

### Toolchains

* Stable Rust, edition 2024 or newer, is required.
* Nightly Rust is only required for formatting with `cargo +nightly fmt`.
* Clippy runs with `clippy::all` denied and `clippy::pedantic` and `clippy::nursery` enabled as warnings. CI treats all warnings as errors.

## Before Committing

Run the formatter before you commit changes:

```
cargo +nightly fmt
```

Set up the repository pre-commit hooks from the repository root:

```
cargo install prek
prek install
```

Run the same checks that CI expects:

```
cargo +nightly fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

## License

This project is licensed under the [MIT License](LICENSE).