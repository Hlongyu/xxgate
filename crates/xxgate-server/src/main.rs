#[tokio::main]
async fn main() -> anyhow::Result<()> {
    xxgate_server::bootstrap::run().await
}
