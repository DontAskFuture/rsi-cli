#[tokio::main]
async fn main() {
    if let Err(err) = rsi_cli::cli::run_cli().await {
        eprintln!("Error: {err:#}");
        std::process::exit(1);
    }
}
