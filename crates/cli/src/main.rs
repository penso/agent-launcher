#[cfg(feature = "tui")]
#[tokio::main]
async fn main() -> Result<(), agent_launcher_tui::Error> {
    agent_launcher_tui::run().await
}

#[cfg(not(feature = "tui"))]
fn main() {
    eprintln!("agent-launcher was built without TUI support");
}
