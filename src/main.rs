#![allow(dead_code, elided_named_lifetimes)]

use aws_credential_types::provider;
use aws_credential_types::Credentials;
use aws_sdk_s3::config::{Builder, Region};
use futures::stream::{futures_unordered::FuturesUnordered, StreamExt};
use lambda_http::{run, service_fn, tracing, Error, Body, Request, Response};
use multipart::server::Multipart;
use serde_json::json;
use std::env;
use std::io::Read;
use std::time;
use tokio;

#[derive(Debug)]
struct MyCustomProvider;
impl MyCustomProvider {
    async fn load_credentials(&self) -> provider::Result {
        let access_key = env::var("MY_AWS_ACCESS_KEY_ID").expect("NO creds found");
        let secret_key = env::var("MY_AWS_SECRET_ACCESS_KEY").expect("NO creds found");

        Ok(Credentials::new(
            access_key,
            secret_key,
            None,
            None,
            "My Provider",
        ))
    }
}

impl provider::ProvideCredentials for MyCustomProvider {
    fn provide_credentials<'a>(&'a self) -> provider::future::ProvideCredentials
    where
        Self: 'a,
    {
        provider::future::ProvideCredentials::new(self.load_credentials())
    }
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), Error> {
    tracing::init_default_subscriber();
    dotenv::dotenv().ok();
    run(service_fn(function_handler)).await
}

async fn upload_file(
    s3: aws_sdk_s3::Client,
    file_name: String,
    content_type: String,
    body: Vec<u8>,
) -> serde_json::Value {
    let bucket = env::var("MY_AWS_BUCKET_NAME").unwrap();
    let region = env::var("MY_AWS_REGION").unwrap();
    let file_name = file_name.replace(" ", "_");

    let res = match s3
        .put_object()
        .bucket(bucket.clone())
        .key(file_name.clone())
        .body(body.into())
        .send()
        .await
    {
        Ok(res) => res,
        Err(e) => return json!({"error": format!("{:?}", e)}),
    };

    let server_side_encryption = res.server_side_encryption.map(|x| x.as_str().to_string());
    let checksum_type = res.checksum_type.map(|x| x.as_str().to_string());
    let checksum_sha256 = res.checksum_sha256;

    json!({
        "message": format!("Single File Upload: Content-Type: {}, File name: {}", content_type, file_name),
        "s3_response": {
            "url": format!("https://{}.s3.{}.amazonaws.com/{}", bucket, region, file_name),
            "bucket": bucket,
            "key": file_name,
            "size": res.size,
            "e_tag": res.e_tag,
            "bucket_key_enabled": res.bucket_key_enabled,
            "version_id": res.version_id,
            "content_type": content_type,
            "file_name": file_name,
            "server_side_encryption": server_side_encryption,
            "checksum_type": checksum_type,
            "checksum_sha256": checksum_sha256
        }
    })
}

pub(crate) async fn function_handler(event: Request) -> Result<Response<Body>, Error> {
    let config = Builder::new()
        .behavior_version_latest()
        .region(Region::new(env::var("MY_AWS_REGION")?))
        .credentials_provider(MyCustomProvider)
        .build();
    let s3 = aws_sdk_s3::Client::from_conf(config);

    let content_type = match event.headers().get("content-type") {
        Some(value) => value.to_str()?.to_string(),
        None => {
            return Ok(Response::builder()
                .status(400)
                .body("Missing Content-Type".into())?)
        }
    };

    if content_type.starts_with("multipart/form-data") {
        handle_multipart(event, content_type, s3).await
    } else if content_type.is_empty() {
        handle_single_file(event, content_type, s3).await
    } else {
        Ok(Response::builder().status(400).body(
            "Expected multipart/form-data or single file upload with Content-Type and x-file-name"
                .into(),
        )?)
    }
}

async fn handle_multipart(
    req: Request,
    content_type: String,
    s3: aws_sdk_s3::Client,
) -> Result<Response<Body>, Error> {
    let start = time::Instant::now();
    let body = match req.body() {
        Body::Text(text) => text.as_bytes().to_vec(),
        Body::Binary(bytes) => bytes.to_vec(),
        Body::Empty => {
            return Ok(Response::builder()
                .status(400)
                .body("Invalid body type".into())?)
        }
    };
    let body = &body[..];

    let boundary = content_type
        .split("boundary=")
        .nth(1)
        .ok_or("Missing boundary in Content-Type")?;

    let mut multipart = Multipart::with_body(body, boundary);
    let mut files_info = FuturesUnordered::new();

    while let Some(entry) = multipart.read_entry()? {
        let file_bytes = entry
            .data
            .bytes()
            .filter_map(|b| b.ok())
            .collect::<Vec<u8>>();
        // Spawn tasks for concurrent uploads
        files_info.push(tokio::spawn(upload_file(
            s3.clone(),
            entry.headers.filename.unwrap_or("unknown.txt".to_string()),
            entry
                .headers
                .content_type
                .map(|ct| ct.to_string())
                .unwrap_or("text/plain".to_string()),
            file_bytes,
        )));
    }

    let mut results: Vec<serde_json::Value> = Vec::new();
    while let Some(result) = files_info.next().await {
        results.push(result.unwrap());
    }

    Ok(Response::builder().status(200).body(
        json!({
            "duration": start.elapsed().as_secs(),
            "results": results})
        .to_string()
        .into(),
    )?)
}

async fn handle_single_file(
    req: Request,
    content_type: String,
    s3: aws_sdk_s3::Client,
) -> Result<Response<Body>, Error> {
    let start = time::Instant::now();
    let file_name = match req.headers().get("x-file-name") {
        Some(value) => value.to_str()?.to_string(),
        None => {
            return Ok(Response::builder()
                .status(400)
                .body("Missing x-file-name header".into())?)
        }
    };

    let body = match req.body() {
        Body::Text(text) => text.as_bytes().to_vec(),
        Body::Binary(bytes) => bytes.to_vec(),
        Body::Empty => {
            return Ok(Response::builder()
                .status(400)
                .body("Invalid body type".into())?)
        }
    };

    let mut res = upload_file(s3, file_name, content_type, body).await;
    if let Some(x) = res.as_object_mut() {
        x.insert("duration".to_string(), json!(start.elapsed().as_secs()));
    }
    Ok(Response::builder()
        .status(200)
        .body(res.to_string().into())?)
}
