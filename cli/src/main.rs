#![allow(missing_docs)] // binary entry point; docs live in the library crate

#[tokio::main]
async fn main() {
    if let Err(e) = ccache::run().await {
        eprintln!("ccache: {e:#}");
        std::process::exit(1);
    }
}
