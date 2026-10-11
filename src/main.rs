mod cli;

#[tokio::main]
async fn main() {
    if let Err(error) = cli::run().await {
        eprintln!("teellm: {error}");
        std::process::exit(1);
    }
}
