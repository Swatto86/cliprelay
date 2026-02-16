use clap::Parser;
use cliprelay_relay::{AppState, serve};
use tracing::{error, info, warn};

#[derive(Parser, Debug)]
#[command(name = "cliprelay-relay")]
struct RelayArgs {
    #[arg(long, default_value = "0.0.0.0:8080")]
    bind_address: String,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let args = RelayArgs::parse();
    let listener = match tokio::net::TcpListener::bind(&args.bind_address).await {
        Ok(listener) => listener,
        Err(err) => {
            error!("failed to bind {}: {}", args.bind_address, err);
            std::process::exit(1);
        }
    };

    info!("relay starting on {}", args.bind_address);
    if let Err(err) = serve(listener, AppState::new()).await {
        warn!("relay server exited: {}", err);
    }
}
