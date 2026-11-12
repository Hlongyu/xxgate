#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [] => xxgate_server::bootstrap::run().await,
        [command] if command == "migrate" => {
            let database = std::env::var("DATABASE_URL")?;
            let store = xxgate_postgres::PgStore::open(&database).await?;
            store.migrate().await?;
            println!("Database migrations completed");
            Ok(())
        }
        [command] if command == "check-schema" => {
            xxgate_postgres::PgStore::open(&std::env::var("DATABASE_URL")?)
                .await?
                .check_schema()
                .await?;
            println!("Database schema matches this image");
            Ok(())
        }
        [command] if command == "preflight" => xxgate_server::bootstrap::preflight().await,
        _ => anyhow::bail!("Usage: xxgate-server [migrate|check-schema|preflight]"),
    }
}
