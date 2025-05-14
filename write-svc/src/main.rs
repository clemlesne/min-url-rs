use anyhow::Result;
use axum::{Json, Router, extract::State, http::StatusCode, response::IntoResponse, routing::post};
use deadpool_postgres::{
    ManagerConfig, Pool as PostgresPool, RecyclingMethod, Runtime as PgRuntime,
    tokio_postgres::NoTls,
};
use deadpool_redis::{
    Config as RedisConfig, Pool as RedisPool, Runtime as RedisRuntime, redis::cmd,
};
use serde::{Deserialize, Serialize};
use std::env;
use std::sync::Arc;
use tower::ServiceBuilder;
use tower_http::{compression::CompressionLayer, decompression::RequestDecompressionLayer};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
use url::Url;

/// Shorten URL JSON payload
#[derive(Deserialize, Serialize)]
struct ShortenPayload {
    #[serde(default)]
    owner: Option<String>,
    #[serde(default)]
    slug: Option<String>,
    url: Url,
}

/// Slug allocation error
struct MiniErr {
    status: Status,
}

/// Slug allocation error status
enum Status {
    NoSlug,
    DbConflict,
    Other,
}

/// Web application state
struct AppState {
    pg_pool: PostgresPool,
    redis_pool: RedisPool,
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

    // Build the app state
    let state = Arc::new(AppState {
        redis_pool,
        pg_pool,
    });

    // Register the shorten handler
    let app = Router::new()
        .route("/shorten", post(handle_shorten_post))
        .with_state(state)
        .layer(
            ServiceBuilder::new()
                .layer(RequestDecompressionLayer::new())
                .layer(CompressionLayer::new()),
        );

    // Start the server
    let listener = tokio::net::TcpListener::bind("0.0.0.0:8080").await?;
    tracing::info!("write-svc running on {}", listener.local_addr()?);
    axum::serve(listener, app).await?;

    Ok(())
}

/// Shorten URL handler
async fn handle_shorten_post(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<ShortenPayload>,
) -> Result<impl IntoResponse, StatusCode> {
    // Check if URL is HTTP(S)
    if payload.url.scheme() != "http" && payload.url.scheme() != "https" {
        return Err(StatusCode::BAD_REQUEST);
    }

    // If slug is provided, insert
    let slug = if let Some(custom) = payload.slug {
        // Check if slug has a valid length
        if custom.len() < 3 || custom.len() > 256 {
            return Err(StatusCode::BAD_REQUEST);
        }

        match insert_slug(&state, &custom, &payload.url, &payload.owner).await {
            Ok(true) => custom,
            Ok(false) => return Err(StatusCode::CONFLICT),
            Err(_) => return Err(StatusCode::SERVICE_UNAVAILABLE),
        }

    // Otherwise, allocate a mini-slug from the pool
    } else {
        allocate_mini_slug(&state, &payload)
            .await
            .map_err(|e| match e.status {
                Status::NoSlug => StatusCode::SERVICE_UNAVAILABLE,
                Status::DbConflict => StatusCode::CONFLICT,
                Status::Other => StatusCode::SERVICE_UNAVAILABLE,
            })?
    };

    // Try to get a Redis connection
    let mut redis_conn = state
        .redis_pool
        .get()
        .await
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;

    // Cache in Redis (fire & forget)
    let url_clone = payload.url.clone();
    let slug_clone = slug.clone();
    tokio::spawn(async move {
        cmd("SET")
            .arg(&slug_clone)
            .arg(url_clone.as_str())
            .query_async::<()>(&mut redis_conn)
            .await
            .unwrap();
        tracing::debug!("Cached {slug_clone} -> {url_clone} in Redis");
    });

    // Return the payload
    Ok((
        StatusCode::CREATED,
        Json(ShortenPayload {
            owner: payload.owner,
            slug: Some(slug),
            url: payload.url,
        }),
    ))
}

/// Allocate a mini-slug from the pool, retrying up to 6 times
async fn allocate_mini_slug(state: &AppState, payload: &ShortenPayload) -> Result<String, MiniErr> {
    // Retry to consume the queue up to 6 times
    for retry in 0..6 {
        // 1, pop slug from Redis list
        let mut rconn = state.redis_pool.get().await.map_err(|_| MiniErr {
            status: Status::Other,
        })?;
        let slug_opt: Option<String> = cmd("RPOP")
            .arg("slug_pool")
            .query_async(&mut rconn)
            .await
            .map_err(|_| MiniErr {
            status: Status::Other,
        })?;
        let slug = match slug_opt {
            // If we got a slug, return it
            Some(s) => s,
            // If we didn't, return an error
            None => {
                return Err(MiniErr {
                    status: Status::NoSlug,
                });
            }
        };

        // 2, try insert into Postgres
        match insert_slug(state, &slug, &payload.url, &payload.owner).await {
            // If inserted, return the slug
            Ok(true) => return Ok(slug),
            // If conflict, retry
            Ok(false) => {
                tracing::debug!("Slug {slug} already exists, retrying ({retry})");
                continue;
            }
            // If error, return error
            Err(_) => {
                return Err(MiniErr {
                    status: Status::Other,
                });
            }
        }
    }

    // 3, exhausted all retries
    Err(MiniErr {
        status: Status::DbConflict,
    })
}

/// DB insert helper (returns Ok(true) if inserted, Ok(false) on conflict)
async fn insert_slug(
    state: &AppState,
    slug: &str,
    url: &Url,
    owner: &Option<String>,
) -> Result<bool> {
    let client = state.pg_pool.get().await?;
    let rows = client
        .execute("INSERT INTO slugs (first_char, slug, url, owner) VALUES ($1, $2, $3, $4) ON CONFLICT DO NOTHING", &[&(&slug[0..1]), &slug, &url.as_str(), &owner])
        .await?;
    Ok(rows == 1)
}
