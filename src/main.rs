mod rpc;
mod service;

pub mod protos;
pub mod types;
pub mod utils;

use dotenvy::dotenv;
use tonic::transport::Server;
use tonic_web::GrpcWebLayer;
use tower_http::{
    cors::{AllowHeaders, AllowOrigin, CorsLayer},
    trace::TraceLayer,
};
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenv().ok();

    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();

    tracing::info!("llr-sync starting up");
    tracing::info!("Environment loaded, initializing service");

    let address = "[::]:8080".parse()?;
    tracing::info!("Server will bind to address: {}", address);

    tracing::info!("Building StratSync service");
    let service = service::build_stratsync().await;

    tracing::info!("Starting gRPC server with HTTP/1.1 and gRPC-Web support");
    Server::builder()
        .accept_http1(true)
        .layer(TraceLayer::new_for_http())
        .layer(
            CorsLayer::new()
                .allow_origin(AllowOrigin::mirror_request())
                .allow_headers(AllowHeaders::mirror_request()),
        )
        .layer(GrpcWebLayer::new())
        .add_service(service)
        .serve(address)
        .await?;

    tracing::info!("Server shutdown complete");
    Ok(())
}
