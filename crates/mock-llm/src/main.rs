use mock_llm::run;

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("[mock-llm] fatal: {e}");
        std::process::exit(1);
    }
}
