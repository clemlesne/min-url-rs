use anyhow::Result;
use axum::http::StatusCode;
use axum::{
    Router,
    extract::{Path, Query, State},
    http::header,
    response::{IntoResponse, Redirect, Response},
    routing::get,
};
use deadpool_postgres::{
    ManagerConfig, Pool as PostgresPool, RecyclingMethod, Runtime as PgRuntime,
    tokio_postgres::NoTls,
};
use deadpool_redis::{
    Config as RedisConfig, Pool as RedisPool, Runtime as RedisRuntime, redis::cmd,
};
use image::{DynamicImage, ImageFormat as ImageOutputFormat, Luma, Rgb};
use moka::future::Cache;
use qrcode::render::svg;
use qrcode::{EcLevel, QrCode, Version};
use std::collections::HashMap;
use std::io::Cursor;
use std::str::FromStr;
use std::sync::Arc;
use std::{env, time::Duration};
use strum_macros::EnumString;
use tower::ServiceBuilder;
use tower_http::{compression::CompressionLayer, decompression::RequestDecompressionLayer};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
use url::Url;

/// Web application state
struct AppState {
    memory_cache: Cache<String, Arc<Option<String>>>,
    pg_pool: PostgresPool,
    redis_pool: RedisPool,
    self_domain: String,
}

/// Image format for QR code
#[derive(Debug, EnumString)]
enum ImageFormat {
    #[strum(ascii_case_insensitive)]
    Gif,
    #[strum(ascii_case_insensitive)]
    Jpeg,
    #[strum(ascii_case_insensitive)]
    Png,
    #[strum(ascii_case_insensitive)]
    Svg,
    #[strum(ascii_case_insensitive)]
    Webp,
}

/// Entrypoint
#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                // Axum logs rejections from built-in extractors with the `axum::rejection` target, at `TRACE` level. `axum::rejection=trace` enables showing those events
                format!(
                    "{}=debug,tower_http=debug,axum::rejection=trace",
                    env!("CARGO_CRATE_NAME")
                )
                .into()
            }),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    // Load environment variables
    let db_url = env::var("DATABASE_URL")?;
    let redis_url = env::var("REDIS_URL")?;
    let self_domain = env::var("SELF_DOMAIN")?;

    // Connect Redis
    let redis_cfg = RedisConfig::from_url(&redis_url);
    let redis_pool: RedisPool = redis_cfg.create_pool(Some(RedisRuntime::Tokio1))?;

    // Connect PostgreSQL
    let mut pg_cfg = deadpool_postgres::Config::new();
    pg_cfg.manager = Some(ManagerConfig {
        recycling_method: RecyclingMethod::Fast,
    });
    pg_cfg.url = Some(db_url.clone());
    let pg_pool: PostgresPool = pg_cfg.create_pool(Some(PgRuntime::Tokio1), NoTls)?;

    // Build slug memory cache (TTL 30s)
    let memory_cache: Cache<String, Arc<Option<String>>> = Cache::builder()
        .max_capacity(100)
        .time_to_live(Duration::from_secs(30))
        .build();

    // Build the app state
    let state = Arc::new(AppState {
        memory_cache,
        pg_pool,
        redis_pool,
        self_domain,
    });

    // Register the slug handler
    let app = Router::new()
        .route("/{slug}", get(handle_redirect_get)) // Redirect to the URL
        .route("/{slug}/qr", get(handle_qrcode_get)) // Generate QR code
        .with_state(state)
        .layer(
            ServiceBuilder::new()
                .layer(RequestDecompressionLayer::new())
                .layer(CompressionLayer::new()),
        );

    // Start the server
    let listener = tokio::net::TcpListener::bind("0.0.0.0:8080").await?;
    tracing::info!("redirect-svc running on {}", listener.local_addr()?);
    axum::serve(listener, app).await?;

    Ok(())
}

/// Handle QR code generation
async fn handle_qrcode_get(
    State(state): State<Arc<AppState>>,
    Path(slug): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    // Validate size
    let size = match params.get("size") {
        Some(size) => match size.parse::<u32>() {
            Ok(size) => size.clamp(32, 512),
            Err(_) => 128, // Default to 128
        },
        None => 128, // Default to 128
    };

    // Validate format
    let format = match params.get("format") {
        Some(format) => ImageFormat::from_str(format.as_str()).unwrap_or(
            ImageFormat::Svg, // Default to SVG
        ),
        None => ImageFormat::Svg, // Default to SVG
    };

    // Get the slug from the cache or live databases
    match lookup_cached(&slug, &state).await {
        // If slug found, generate QR code
        Ok(Some(_)) => {
            let qr_code = generate_qrcode_res(&slug, &format, size, &state);
            match qr_code {
                Ok(qr_code) => {
                    tracing::debug!(
                        "Generated QR code: slug={}, size={}, format={:?}",
                        slug,
                        size,
                        format
                    );
                    qr_code
                }
                Err(e) => {
                    tracing::error!("Failed to generate QR code: {}", e);
                    StatusCode::INTERNAL_SERVER_ERROR.into_response()
                }
            }
        }
        // If slug not found, return 404
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        // If there was an error, return 503
        Err(e) => {
            tracing::error!("Failed to lookup slug: {}", e);
            StatusCode::SERVICE_UNAVAILABLE.into_response()
        }
    }
}

/// Handle HTTP redirects
async fn handle_redirect_get(
    State(state): State<Arc<AppState>>,
    Path(slug): Path<String>,
) -> impl IntoResponse {
    match lookup_cached(&slug, &state).await {
        // If slug found, redirect to it
        Ok(Some(url)) => Redirect::to(&url).into_response(),
        // If slug not found, return 404
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        // If there was an error, return 503
        Err(e) => {
            tracing::error!("Failed to lookup slug: {}", e);
            StatusCode::SERVICE_UNAVAILABLE.into_response()
        }
    }
}

/// Get a URL from the memory cache or live databases if required
async fn lookup_cached(slug: &str, state: &AppState) -> Result<Option<String>> {
    // Check in memory cache
    if let Some(url) = state.memory_cache.get(slug).await {
        // If the URL is None, return 404
        if url.is_none() {
            tracing::debug!("Slug {slug} cached as None");
            return Ok(None);
        }
        // Otherwise, return it
        let url = url.as_ref().clone().unwrap();
        tracing::debug!("Slug {} cached as {}", slug, &url);
        return Ok(Some(url));
    }

    // Check live
    match lookup_live(slug, state).await {
        // If slug found, cache and return it
        Ok(Some(url)) => {
            // Store in memory cache
            state
                .memory_cache
                .insert(slug.to_string(), Arc::new(Some(url.clone())))
                .await;
            Ok(Some(url))
        }
        // If slug is not found, cache and return 404
        Ok(None) => {
            // Store in memory cache
            state
                .memory_cache
                .insert(slug.to_string(), Arc::new(None))
                .await;
            Ok(None)
        }
        // If there was an error, return it
        Err(e) => Err(e),
    }
}

/// Get a URL from the databases (PostgreSQL and Redis)
async fn lookup_live(slug: &str, state: &AppState) -> Result<Option<String>> {
    // Get a Redis connection
    let mut redis_conn = state.redis_pool.get().await?;

    // If slug is in Redis, return it
    if let Some(url) = cmd("GET")
        .arg(slug)
        .query_async::<Option<String>>(&mut redis_conn)
        .await?
    {
        tracing::debug!("Slug {slug} found in Redis");
        return Ok(Some(url));
    }

    // Get a PostgreSQL connection
    let pg_client = state.pg_pool.get().await?;

    // Look up the slug in PostgreSQL
    let rows = pg_client
        .query("SELECT url FROM slugs WHERE slug=$1", &[&slug])
        .await?;

    // If not found, return None
    if rows.is_empty() {
        tracing::debug!("Slug {slug} not found");
        return Ok(None);
    }

    // Store it in Redis (fire & forget) and return it
    let url: String = rows[0].get(0);
    let slug = slug.to_string();
    let url_clone = url.clone();
    tokio::spawn(async move {
        cmd("SET")
            .arg(&slug)
            .arg(&url_clone)
            .query_async::<()>(&mut redis_conn)
            .await
            .unwrap();
        tracing::debug!("Stored slug {slug} in Redis");
    });
    Ok(Some(url))
}

/// Generate a QR code for the given URL, as an image, use the public URL as QR content
fn generate_qrcode_res(
    slug: &str,
    format: &ImageFormat,
    size: u32,
    state: &AppState,
) -> Result<Response> {
    // Build the public URL
    let mut url = Url::from_str(&state.self_domain)?;
    url.set_path(slug);

    // Generate the QR code
    let code = QrCode::with_version(url.as_str().as_bytes(), Version::Normal(10), EcLevel::L)?;

    // Encode
    let res = match format {
        ImageFormat::Gif => {
            let img = code.render::<Luma<u8>>().min_dimensions(size, size).build();
            let mut buf = Vec::<u8>::new();
            let mut cursor = Cursor::new(&mut buf);
            DynamicImage::ImageLuma8(img).write_to(&mut cursor, ImageOutputFormat::Gif)?;
            Response::builder()
                .header(header::CONTENT_TYPE, "image/gif")
                .body(buf.into())?
        }
        ImageFormat::Jpeg => {
            let img = code.render::<Luma<u8>>().min_dimensions(size, size).build();
            let mut buf = Vec::<u8>::new();
            let mut cursor = Cursor::new(&mut buf);
            DynamicImage::ImageLuma8(img).write_to(&mut cursor, ImageOutputFormat::Jpeg)?;
            Response::builder()
                .header(header::CONTENT_TYPE, "image/jpeg")
                .body(buf.into())?
        }
        ImageFormat::Png => {
            let img = code.render::<Luma<u8>>().min_dimensions(size, size).build();
            let mut buf = Vec::<u8>::new();
            let mut cursor: Cursor<&mut Vec<u8>> = Cursor::new(&mut buf);
            DynamicImage::ImageLuma8(img).write_to(&mut cursor, ImageOutputFormat::Png)?;
            Response::builder()
                .header(header::CONTENT_TYPE, "image/png")
                .body(buf.into())?
        }
        ImageFormat::Webp => {
            let img = code.render::<Rgb<u8>>().min_dimensions(size, size).build();
            let mut buf = Vec::<u8>::new();
            let mut cursor = Cursor::new(&mut buf);
            DynamicImage::ImageRgb8(img).write_to(&mut cursor, ImageOutputFormat::WebP)?;
            Response::builder()
                .header(header::CONTENT_TYPE, "image/webp")
                .body(buf.into())?
        }
        ImageFormat::Svg => {
            let svg = code
                .render()
                .min_dimensions(size, size)
                .dark_color(svg::Color("#000"))
                .light_color(svg::Color("#fff"))
                .build();
            Response::builder()
                .header(header::CONTENT_TYPE, "image/svg+xml")
                .body(svg.into())?
        }
    };

    Ok(res)
}
