# Conduit

## Project Overview

Conduit is a Matrix homeserver written in Rust. It aims to be an efficient and easy-to-set-up server, suitable for personal or small-scale deployments. It uses the Axum web framework, with Tokio as the asynchronous runtime. It supports both RocksDB and SQLite as database backends. The project uses the Ruma crates for Matrix API types and functionality. The project is built using Nix, which provides a reproducible build environment.

## Building and Running

### Prerequisites

*   Rust toolchain (see `rust-toolchain.toml`)
*   A C++ compiler (for RocksDB)
*   Nix package manager

### Building

To build the OCI image for the container using Nix, run the following command:

```bash
nix build .#oci-image --extra-experimental-features "nix-command flakes"
```

For a standard development build, you can use Cargo:

```bash
cargo build --release
```

### Deployment

The project includes a script to build and deploy a Docker image. The script `build_and_load_docker.sh` builds the Docker image using the `nix build .#oci-image ...` command, loads it into Docker, tags it, and pushes it to a container registry.

### Configuration

Conduit is configured using a TOML file. An example configuration file is provided as `conduit-example.toml`. The main configuration options include:

*   `server_name`: The domain name of the homeserver.
*   `database_backend`: The database backend to use (`rocksdb` or `sqlite`).
*   `database_path`: The path to the database directory.
*   Network settings (address, port, TLS).
*   Federation, registration, and encryption settings.

### Running

To run the server, set the `CONDUIT_CONFIG` environment variable to the path of your configuration file and run the binary:

```bash
CONDUIT_CONFIG=/path/to/conduit.toml ./target/release/conduit
```

### Testing

The project uses the Nix development shell to ensure a reproducible testing environment. The primary command to run the test suite is:

```bash
nix --extra-experimental-features "nix-command flakes" develop -c cargo test
```

Alternatively, you can enter the development shell first and then run the tests:

```bash
nix --extra-experimental-features "nix-command flakes" develop
# You are now in the Nix shell
cargo test
```

## Development Conventions

*   The project follows standard Rust conventions and uses `rustfmt` for code formatting.
*   It uses `tracing` for logging and supports `opentelemetry` for distributed tracing.
*   The code is organized into several modules, including `api`, `config`, `database`, `service`, and `utils`.
*   The `api` module handles the web server routes and uses the `ruma` crate for Matrix API types.
*   The `service` module contains the core logic of the homeserver.
*   The `database` module provides an abstraction over the supported database backends.

## Federation `m.typing` Implementation

The implementation of `m.typing` federation is partially complete. Here's a breakdown of the current status and the plan to complete it:

### Current Status

*   **Client to Server:** Typing events from local clients are correctly handled, updating the server's internal state.
*   **Incoming Federation:** The server correctly processes `m.typing` EDUs from other homeservers, updating the typing status of remote users in a room.
*   **Outgoing Federation (`typing: true`):** The federation sender (`select_edus` function) correctly queries the current state and sends `m.typing` events with `typing: true` for users who are actively typing.

### Missing Functionality

*   **Outgoing Federation (`typing: false`):** The federation sender does **not** currently send `m.typing` events with `typing: false` when a user stops typing. This means that remote servers will not be notified when a local user stops typing, and their typing indicator may remain active indefinitely.

### Plan for Completion

To complete the implementation, the following changes are required:

1.  **Track Typing State Changes:** The server needs to track which users have stopped typing since the last successful transaction to each destination server.
2.  **Modify `select_edus`:** The `select_edus` function in `src/service/sending/mod.rs` must be updated to:
    *   Query for users who have recently stopped typing.
    *   Create `Edu::Typing` events with `typing: false` for these users.
    *   Include these "stop typing" events in the EDUs to be sent in the next transaction.
    *   Reset the "stopped typing" state for that server once the transaction is sent.
