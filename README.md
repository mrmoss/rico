# rico (Rust)

Vibe coded clanker code knockoff of pico that is more Mike.

The port keeps the same core design: a stripped-down, fast terminal text editor built for raw speed and minimal fluff, keeping file buffers simple and driving rendering directly to the terminal. Works clean, gets out of your way, and doesn't overcomplicate basic editing.

## Build

Requires a Rust toolchain with Cargo.

```bash
cargo build --release
```

The binary is:

```text
target/release/rico
```

Or:
```bash
cargo build --release --target x86_64-unknown-linux-musl
```

The binary is:

```text
target/x86_64-unknown-linux-musl/release/rico
```

## Usage

```bash
./target/release/rico [options] [filename]
```

# TODO
- having nano's multi doc support would be nice
- copy and paste to clipboard i dunno if i like tbh
- also the word wrap traversal/movement isnt good
- rmux and rico need to play nice together - they look a little odd
- need a column edit mode