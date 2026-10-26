#[tokio::main]
async fn main() -> anyhow::Result<()> {
    home_camera::persistent::dispatch().await
}
