//! BuildKit metrics agent binary entrypoint.

use anyhow::Result;
use buildkit_metrics_agent::run;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("buildkit_metrics_agent=info".parse()?),
        )
        .init();

    run().await
}
