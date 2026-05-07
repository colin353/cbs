use std::error::Error;

use bytes::Bytes;
use http_body_util::{BodyExt, Empty};
use hyper::Uri;
use hyper_rustls::HttpsConnectorBuilder;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;

type Result<T> = std::result::Result<T, Box<dyn Error + Send + Sync>>;

#[tokio::main]
async fn main() -> Result<()> {
    let body = download_https("https://example.com/").await?;
    assert!(!body.is_empty());
    println!("downloaded {} bytes over HTTPS", body.len());
    Ok(())
}

async fn download_https(url: &str) -> Result<Bytes> {
    let _ = rustls::crypto::ring::default_provider().install_default();

    let https = HttpsConnectorBuilder::new()
        .with_webpki_roots()
        .https_only()
        .enable_http1()
        .build();
    let client: Client<_, Empty<Bytes>> = Client::builder(TokioExecutor::new()).build(https);
    let uri: Uri = url.parse()?;
    let response = client.get(uri).await?;
    assert!(
        response.status().is_success(),
        "unexpected HTTP status: {}",
        response.status()
    );
    Ok(response.into_body().collect().await?.to_bytes())
}
