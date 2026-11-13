# Project Conventions & Guidelines for switch2d

This document defines the development conventions, architecture guidelines, and engineering standards for developers and AI coding agents working on **switch2d**.

---

## 1. Project Overview & Architecture

**switch2d** is a high-performance Linux userspace bridge and virtual gamepad daemon written in Rust (2021 edition) for Nintendo Switch 2 controllers.

### Directory Structure
- **`src/`**: Active Rust daemon codebase.
  - `bluetooth/`: Low-level Bluetooth communication and device scanning.
  - `controller.rs`: Controller lifecycle, state management, and input processing.
  - `daemon.rs`: Core daemon loop, service management, and signals.
  - `dsu.rs` / `dsu_bridge.rs`: Cemuhook / DSU motion server for UDP motion streaming.
  - `hub.rs`: Controller hub coordinating multiple active devices.
  - `protocol/`: Protocol parsers, report decoders, and vibration encoders.
  - `rumble.rs`: Haptics and rumble playback management.
  - `uinput/`: Linux uinput virtual gamepad device creation and event emission.
- **`.old/`**: Legacy Python/C reference implementation. **Read-only**: used exclusively as an authoritative reference for protocol reverse engineering. Do not edit.
- **`docs/`**: Architecture documentation, protocol analyses (`docs/analysis_controllers.md`), and test plans.

### Core Stack
- **Async Runtime**: `tokio` (full features).
- **Subsystems**: `evdev` (input events), `libc`, `tracing` (structured logging), `serde` / `serde_json` (serialization), `clap` (CLI argument parsing).

---

## 2. Rust Engineering Conventions

### Error Handling
- Never use `.unwrap()` or `.expect()` in production or daemon runtime paths.
- Propagate errors using `Result<T, E>` with appropriate error types (e.g. `std::io::Error` or domain-specific error enums).
- Unwraps are permitted exclusively in unit tests or when verifying static constants at startup.

### Async Concurrency with Tokio
- Prefer non-blocking async primitives.
- For inter-task communication, use `tokio::sync::mpsc` for point-to-point queues and `tokio::sync::broadcast` / `watch` for state changes.
- Never block a Tokio runtime worker thread with synchronous I/O or heavy loops. Use dedicated OS threads or `tokio::task::spawn_blocking` when interfacing with blocking C/kernel calls.

### Observability & Logging
- Use the `tracing` ecosystem (`trace!`, `debug!`, `info!`, `warn!`, `error!`).
- Log hardware connection events, battery state transitions, and slot assignments at `info!` level.
- Keep high-frequency per-packet logs at `trace!` or `debug!` level to avoid I/O bottlenecks during 60–1000Hz polling loops.

### Linux Input Subsystem (`evdev` / `uinput`)
- Ensure virtual gamepads mimic standard Linux gamepad layouts (e.g. standard Xbox/Switch controller button mappings).
- Properly synchronize event packets using `SYN_REPORT` to avoid dropped or desynchronized axis movements.

---

## 3. Testing & Verification Standards

### Definition of Done
Before any task, module, or PR is considered complete:
1. **Zero Compiler Warnings**: `cargo check` must pass with zero warnings.
2. **Zero Regressions**: `cargo test` must pass all unit and integration tests cleanly.
3. **Formatting & Lints**: Code must conform to `cargo fmt --check` and `cargo clippy`.

### Unit Testing Guidelines
- Separate pure logic (packet parsing, CRC32 calculations, motion sensor scaling, battery level conversions) from I/O so it can be thoroughly unit-tested without physical controllers.
- Use mock sinks or loopback sockets (e.g. binding port `0` for UDP) for network and protocol tests.
- Tests must be fast, deterministic, and free of arbitrary sleep delays.
