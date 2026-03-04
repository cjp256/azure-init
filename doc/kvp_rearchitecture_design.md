# KVP Rearchitecture – Proposed Interfaces

This document describes the layered redesign of the Hyper-V KVP subsystem in libazureinit. The goal is to decouple storage, diagnostics, tracing, and provisioning-report concerns behind a common trait so that each layer can be tested in isolation against a simple in-memory dictionary.

## Layer 0 – Core KvpStore Trait

The fundamental storage abstraction. Implementations handle encoding, persistence, and concurrency internally.

```rust
pub trait KvpStore: Send + Sync {
    /// Write a key-value pair into the store.
    fn write(&self, key: &str, value: &str) -> io::Result<()>;

    /// Read the value for a given key, returning None if absent.
    fn read(&self, key: &str) -> io::Result<Option<String>>;

    /// Return all key-value pairs currently in the store.
    fn entries(&self) -> io::Result<Vec<(String, String)>>;

    /// Remove a key. Returns true if the key existed.
    fn delete(&self, key: &str) -> io::Result<bool>;
}
```

## Layer 0a – HyperVKvpStore (production)

Reads and writes the binary Hyper-V KVP pool-file format (512-byte key + 2048-byte value fixed-size records) with flock-based concurrency control.

```rust
pub struct HyperVKvpStore {
    path: PathBuf,
}

impl HyperVKvpStore {
    /// Open (or create) the pool file at the given path.
    pub fn new(path: impl Into<PathBuf>) -> Self;

    /// Truncate the file when its mtime predates the
    /// current boot (stale-data guard).
    pub fn truncate_if_stale(&self) -> io::Result<()>;
}

impl KvpStore for HyperVKvpStore { /* ... */ }
```

### Key implementation notes

- `write()` acquires an exclusive flock, appends one fixed-size record, flushes, and releases the lock — matching the existing `write_kvps()` behaviour that already passes the 200K-record concurrency tests.
- `read()` scans all records sequentially and returns the last match (append-only semantics).
- `entries()` returns every record, including duplicates.
- `delete()` rewrites the file without the matching record(s) while holding an exclusive flock.
- `write()` writes exactly one fixed-size record per call. Value truncation or splitting across multiple records is **not** handled at this layer — that is the responsibility of higher layers (e.g. `DiagnosticsKvp`) that understand the semantics of their data.

### Record format

Each record in the pool file is a fixed-size block of **2,560 bytes** — the first 512 bytes hold the key name and the remaining 2,048 bytes hold the value. Any unused space is filled with zeros. Keys and values that exceed their field size are truncated.

## Layer 0b – InMemoryKvpStore (test double)

A simple HashMap-backed store with no filesystem access. Thread-safe via `Arc<Mutex<…>>`. Drop-in replacement for any layer in unit and integration tests.

```rust
#[derive(Default, Clone)]
pub struct InMemoryKvpStore {
    inner: Arc<Mutex<HashMap<String, String>>>,
}

impl KvpStore for InMemoryKvpStore {
    fn write(&self, key: &str, value: &str) -> io::Result<()>;
    fn read(&self, key: &str) -> io::Result<Option<String>>;
    fn entries(&self) -> io::Result<Vec<(String, String)>>;
    fn delete(&self, key: &str) -> io::Result<bool>;
}
```

## Layer 1 – DiagnosticsKvp

Typed access to diagnostic key-value entries. Keys are formatted per the existing `generate_event_key` convention (`prefix|vm_id|level|name|span_id`).

### DiagnosticEvent

A structured representation of a single diagnostic event. Each field maps directly to a segment of the KVP key or value.

```rust
pub struct DiagnosticEvent {
    /// Severity level (e.g. "INFO", "WARN", "ERROR", "DEBUG").
    pub level: String,
    /// Logical event name (e.g. "provision:user:create_user").
    pub name: String,
    /// Unique identifier tying the event to a span/operation.
    pub span_id: String,
    /// Human-readable message / payload.
    pub message: String,
    /// When the event occurred.
    pub timestamp: DateTime<Utc>,
}

impl DiagnosticEvent {
    pub fn new(
        level: impl Into<String>,
        name: impl Into<String>,
        message: impl Into<String>,
    ) -> Self;
}

impl fmt::Display for DiagnosticEvent { /* ... */ }
```

### DiagnosticsKvp

```rust
pub struct DiagnosticsKvp<S: KvpStore> {
    store: S,
    vm_id: String,
    event_prefix: String,   // e.g. "azure-init-0.1.1"
}

impl<S: KvpStore> DiagnosticsKvp<S> {
    pub fn new(store: S, vm_id: &str, event_prefix: &str) -> Self;

    /// Write a diagnostic event to the store.
    pub fn emit(&self, event: &DiagnosticEvent) -> io::Result<()>;

    /// Read all diagnostic entries from the store, parsed into
    /// DiagnosticEvent structs.
    pub fn entries(&self) -> io::Result<Vec<DiagnosticEvent>>;
}
```

### Value splitting

The Azure platform only reads the first **1,022 bytes** of the value field per record (UTF-16: 511 characters + null terminator). `DiagnosticsKvp::emit()` is responsible for splitting values that exceed this limit across multiple records with the same key. This keeps `HyperVKvpStore` simple (one call = one record) while preserving the chunking semantics the host expects.

## Layer 2 – TracingKvpLayer (tracing subscriber)

A `tracing_subscriber::Layer` that translates span open/close and event occurrences into `DiagnosticsKvp::emit()` calls. This is the direct replacement for the current `EmitKVPLayer`.

```rust
pub struct TracingKvpLayer<S: KvpStore + 'static> {
    diagnostics: DiagnosticsKvp<S>,
}

impl<S: KvpStore + 'static> TracingKvpLayer<S> {
    pub fn new(diagnostics: DiagnosticsKvp<S>) -> Self;
}

impl<S, Sub> tracing_subscriber::Layer<Sub>
    for TracingKvpLayer<S>
where
    S: KvpStore + 'static,
    Sub: Subscriber + for<'lookup> LookupSpan<'lookup>,
{
    fn on_event(&self, event, ctx);
    fn on_new_span(&self, attrs, id, ctx);
    fn on_close(&self, id, ctx);
}
```

- **on_event:** extracts the event message via `StringVisitor`; for `health_report` fields it writes a `ProvisioningReport` directly to the store.
- **on_new_span / on_close:** records `Instant` start time as a span extension and emits a Start/End diagnostic entry on close — identical to the current behaviour.

## Layer 3 – ProvisioningReportKvp

Typed accessor for the `PROVISIONING_REPORT` KVP key, which is read by the Azure platform to determine provisioning outcome.

### ProvisioningReport

A structured representation of a provisioning report entry.

```rust
pub struct ProvisioningReport {
    /// Outcome: "success", "error", etc.
    pub result: String,
    /// Agent identifier (e.g. "Azure-Init/0.1.1").
    pub agent: String,
    /// PPS type (e.g. "None").
    pub pps_type: String,
    /// VM identifier.
    pub vm_id: String,
    /// When the report was generated.
    pub timestamp: DateTime<Utc>,
    /// Optional extra key-value pairs (e.g. origin, error details).
    pub extra: Vec<(String, String)>,
}

impl ProvisioningReport {
    /// Create a success report.
    pub fn success(vm_id: &str) -> Self;

    /// Create a failure/error report.
    pub fn error(vm_id: &str, reason: &str) -> Self;

    /// Encode as a pipe-delimited string for KVP storage.
    pub fn encode(&self) -> String;

    /// Parse a pipe-delimited string back into a report.
    pub fn decode(s: &str) -> io::Result<Self>;
}

impl fmt::Display for ProvisioningReport { /* delegates to encode() */ }
```

A `ProvisioningReport` writes and reads itself directly against any `KvpStore` — no wrapper struct needed.

```rust
impl ProvisioningReport {
    // ... constructors and encode/decode as above ...

    /// Write this report to the store (key = "PROVISIONING_REPORT").
    pub fn write_to(&self, store: &impl KvpStore) -> io::Result<()> {
        store.write("PROVISIONING_REPORT", &self.encode())
    }

    /// Read and parse a provisioning report from the store, if present.
    pub fn read_from(store: &impl KvpStore) -> io::Result<Option<Self>> {
        store.read("PROVISIONING_REPORT")
            .map(|opt| opt.and_then(|s| Self::decode(&s).ok()))
    }
}
```

## Top-Level Kvp Client

The `Kvp` struct wires together all layers and is the only type that callers (`logging.rs`, `main.rs`) need to interact with.

```rust
pub struct Kvp<S: KvpStore> {
    pub store: S,
    pub diagnostics: DiagnosticsKvp<S>,
    pub tracing_layer: TracingKvpLayer<S>,
}

impl Kvp<HyperVKvpStore> {
    /// Production constructor.
    pub fn new() -> Result<Self, anyhow::Error>;
    pub fn with_options(opts: KvpOptions) -> Result<Self, anyhow::Error>;
}

impl<S: KvpStore + Clone> Kvp<S> {
    /// Generic / test constructor from any KvpStore.
    pub fn from_store(
        store: S, vm_id: &str, event_prefix: &str,
    ) -> Self;
}
```

## Key Differences from Current Design

| Concern | Current | Proposed |
|---------|---------|----------|
| Storage | Async channel → background writer task | Synchronous `KvpStore::write()` behind flock; no channel/task needed |
| Tracing coupling | `EmitKVPLayer` owns channel, encoding, and `Layer` impl | `TracingKvpLayer` is a thin adapter over `DiagnosticsKvp<S>` |
| Provisioning reports | Encoded inline in `EmitKVPLayer::emit_health_report` | `ProvisioningReport` struct with `write_to()`/`read_from()` on any store |
| Testability | Tests must use tempfiles and real binary format | Any layer can be tested against `InMemoryKvpStore` |
| Background writer | Tokio task + channel + `CancellationToken` | Removed — synchronous flock + write + unlock |
| close() / shutdown | Required to drain async channel | Not needed — writes are synchronous |

## Proposed Module Structure

The `kvp` module becomes a directory with sub-modules:

```
libazureinit/src/kvp/
├── mod.rs            // re-exports, KvpStore trait, KvpOptions
├── hyperv.rs         // HyperVKvpStore, encode/decode, truncate
├── memory.rs         // InMemoryKvpStore
├── diagnostics.rs    // DiagnosticsKvp<S>
├── tracing.rs        // TracingKvpLayer<S>, StringVisitor, MyInstant
├── provisioning.rs   // ProvisioningReport struct
└── tests.rs          // shared test helpers, all #[cfg(test)] tests
```
