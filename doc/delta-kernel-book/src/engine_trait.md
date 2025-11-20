# The Engine Trait System

The Engine trait is what allows kernel to integrate into high performace engines without undue overhead. In general, if you just want to read and write data from Delta table, you should use the [default engine](#the-default-engine).

```rust
trait Engine {
    // Handlers for specific functionality
    fn storage_handler() -> StorageHandler;
    fn json_handler() -> JsonHandler;
    fn parquet_handler() -> ParquetHandler;
    fn evaluation_handler() -> EvaluationHandler;
}
```

Each handler provides specific capabilities:

- **StorageHandler**: File system operations (listing, reading files)
- **JsonHandler**: Read JSON files and parse them in engine native data format
- **ParquetHandler**: Read Parquet files into engine native data format
- **EvaluationHandler**: Expression evaluation

This pluggable engine architecture that allows query engines to bring their own implementations of core functionalities allowing the use of a connectors existing:
- Data formats
- File system and storage implementations
- Expression evaluation systems

The kernel-core logic expresses all operations via this interface, allowing the heavy lifting of reading/writing data and manipulating it to all be done natively by a connector.

## The Default Engine
The default engine is for connectors that want a simple way to read and write delta tables. It is a reference engine implementation that:
- Uses Apache Arrow as the in-memory data format
- Implements async I/O with Tokio (connectors can pass their own tokio runtimes)
- Supports multiple Arrow versions (generally the last two versions)
- Provides object store integration for cloud storage
