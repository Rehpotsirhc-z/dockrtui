use color_eyre::eyre::Result;

mod app;
mod docker;
mod ui;
mod theme;

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    color_eyre::install()?;
    app::run().await.map_err(|e| color_eyre::eyre::eyre!(e))
}
