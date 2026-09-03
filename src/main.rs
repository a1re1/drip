// bin `drip`. The dispatch lives in drip::cli::entry so the library (and
// its tests) can drive it; this file only builds the tokio runtime, runs
// `main(argv)`, flushes stdout, and exits with the code it returns.

use std::io::Write;

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let code = runtime.block_on(drip::cli::entry::main(argv));

    // Flush stdout on exit: a pipe consumer must see every byte before the
    // process code lands.
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    std::process::exit(code);
}
