# Build & Distribution

How Skald is built for local development and how a **single portable binary** is
produced for headless servers (Ubuntu Server, AWS containers, mini-PCs).

## TLS / crypto: rustls + `ring`, no OpenSSL

Skald links **no OpenSSL and no `aws-lc-rs`**. All HTTPS/WSS traffic goes through
[`rustls`](https://crates.io/crates/rustls) with the pure-`ring` crypto backend.
This is the single most important property for a portable binary: there is no
dynamic link to a system `libssl`/`libcrypto`, so the binary does not depend on
the OpenSSL version installed (or missing) on the target machine.

How it is wired:

- Every `reqwest` dependency in the workspace is declared
  `default-features = false, features = ["rustls-no-provider", …]`. `reqwest 0.13`
  has no bundled-`ring` feature, only `rustls-no-provider` (rustls with **no**
  crypto provider selected).
- Because no provider is bundled, exactly one **process-wide** provider is
  installed at startup, before any TLS handshake, in `src/main.rs`:

  ```rust
  rustls::crypto::ring::default_provider()
      .install_default()
      .expect("install rustls ring crypto provider");
  ```

- `rustls` is pinned as a direct dependency of the root crate **only** to select
  the provider: `default-features = false, features = ["ring", "std", "tls12", "logging"]`.
  Without this, rustls' default feature would pull `aws-lc-rs` back in.
- `teloxide` (Telegram plugin) is on `default-features = false, features = ["rustls", …]`
  so it does not drag in `native-tls`/OpenSSL either.

> **Feature-unification trap.** rustls is a single shared crate across every
> consumer. If *any* dependency enables its `aws_lc_rs` feature, `aws-lc-rs`
> (a cmake/NASM C build) comes back for the whole tree. Consumers that must be
> kept on `ring`: all `reqwest` (done), `tokio-tungstenite` (relay client — it
> declares `tokio-rustls` with `default-features = false`, so it inherits `ring`),
> and the **embedded Tailscale** provider (see below). Verify with:
>
> ```sh
> cargo tree -e no-dev -i aws-lc-rs   # must print "did not match any packages"
> cargo tree -e no-dev -i ring        # must list rustls consumers
> ```

## Native (development) build

```sh
cargo build            # or: cargo run
./run.sh               # supervisor loop (rebuilds on exit -1); -d for debug
```

On macOS/dev machines this dynamically links the system libc, which is fine —
the portability concern only applies to the distributed binary.

## Portable static build (musl)

The distribution target is `x86_64-unknown-linux-musl` (or
`aarch64-unknown-linux-musl`), which produces a **fully static** binary: no
`libssl`, no `libc`, no shared libraries at all. Copy the single file to the
server and run it.

Since there is no OpenSSL/aws-lc build, the only native code left to
cross-compile is **SQLite** (bundled via `libsqlite3-sys`), the **tree-sitter C
grammars**, and `ring` — all of which the musl-cross toolchain handles out of the
box. There is **no host toolchain requirement** other than Docker:

```sh
scripts/build-musl.sh                              # x86_64 static binary
TARGET=aarch64-unknown-linux-musl \
  IMAGE=messense/rust-musl-cross:aarch64-musl \
  scripts/build-musl.sh                            # arm64 static binary
```

Output: `target/musl/<target>/release/skald`. Verify it is static:

```sh
file target/musl/x86_64-unknown-linux-musl/release/skald
# → ELF 64-bit LSB executable, x86-64, statically linked, …
```

The script builds `--no-default-features` (see the feature table below) to drop
`whisper-local`; set `FEATURES=""` to include it (needs a C++ cross-compile).

### glibc alternative

If a static musl binary is more than you need, a **glibc** binary built inside an
old base image (e.g. `debian:bullseye` / `ubuntu:22.04`) runs on any server with
a same-or-newer glibc. Now that OpenSSL is gone, this build needs no special
crypto handling — the normal gnu toolchain compiles SQLite/`ring`/tree-sitter
directly. It is not fully static (glibc stays dynamic) but is broadly compatible
and simpler to produce than musl.

## Cargo features that affect the binary

| Feature | Default | Effect | Portability cost |
| --- | --- | --- | --- |
| `whisper-local` | **on** | Local STT via whisper.cpp | Compiles whisper.cpp (**C++**) — heavy; drop for server builds (`--no-default-features`) |
| `embedded-tailscale` | **off** | Pure-Rust embedded Tailscale provider (no system `tailscaled`) | Pulls the `tailscale` crate, which **forces `aws-lc-rs`** (cmake/NASM C build) back into the tree — breaks the ring-only static binary |

The recommended Tailscale provider, `tailscale_sys` (drives the system
`tailscaled`), is always compiled and needs neither feature. `embedded-tailscale`
exists only for a self-contained mesh where installing `tailscaled` is not an
option; enabling it re-introduces the aws-lc-rs C build. See
[plugins/remote.md](plugins/remote.md).

## What the binary needs at runtime

- **Nothing dynamically linked** in the musl build — no OpenSSL, no libc.
- SQLite is **statically bundled** (compiled from source), so the target needs no
  system `libsqlite3`.
- Python is **optional** and only required for Python-based MCP servers; the app
  starts without it (see `run.sh`).
- The web UI is static assets under `web/` (`web.static_dir`); serve it from the
  same binary or point a reverse proxy at it.

## Self-restart on a server

`run.sh` is a dev supervisor: exit `255` → rebuild+restart, exit `0` → stop. On a
server, prefer **platform-native supervision** instead of the shell loop:
`systemd` with `Restart=on-failure` (map the `restart` tool's `exit(-1)` = 255 to
a restart), a container restart policy, or `launchd` on macOS. See
[self-rewriting.md](self-rewriting.md).
