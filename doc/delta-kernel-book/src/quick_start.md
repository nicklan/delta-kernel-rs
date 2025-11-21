# Quick Start

This section shows how you can quickly start reading data in [arrow](https://arrow.apache.org) format using the default engine.


```rust
use delta_kernel::Table;

// just assume table path is first thing on the command line
let args: Vec<String> = env::args().collect();
let url = delta_kernel::try_parse_uri(&args[1])?;

/// First let's build a default engine instance

// we need an object store instance, we'll make one from our url with no special options
let store = store_from_url_opts(url, HashMap::new())?;
// now construct a default engine from that store
let engine = DefaultEngine::new(store);

/// With our engine in hand we can get a snapshot from our table at the most recent version
let snapshot = Snapshot::builder_for(url).build(&engine)?;

/// Our snapshot allows us to, for example, print the schema of the table
println!("The table schema is {}", snapshot.schema());

/// We can now also scan the table
let scan = snapshot.scan_builder().build()?;

// we will build a vector of arrow `RecordBatch`s from scanning the table
let mut batches = vec![];

for data in scan.execute(Arc::new(engine)) {
    // data is a boxed trait, but with the default engine we know it's arrow data so we can downcast it
    
}

```

