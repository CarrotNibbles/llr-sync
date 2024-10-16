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

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenv().ok();
    tracing_subscriber::fmt::init();

    let address = "[::]:8080".parse().unwrap();
    Server::builder()
        .accept_http1(true)
        .layer(TraceLayer::new_for_http())
        .layer(
            CorsLayer::new()
                .allow_origin(AllowOrigin::mirror_request())
                .allow_headers(AllowHeaders::mirror_request()),
        )
        .layer(GrpcWebLayer::new())
        .add_service(service::build_stratsync().await)
        .serve(address)
        .await?;

    Ok(())
}
