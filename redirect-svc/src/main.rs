use anyhow::Result;
use axum::{
    Router,
    extract::{Path, State},
    response::{IntoResponse, Redirect},
    routing::get,
};
use deadpool_postgres::{
    ManagerConfig, Pool as PostgresPool, RecyclingMethod, Runtime as PgRuntime,
    tokio_postgres::NoTls,
};
use deadpool_redis::{
    Config as RedisConfig, Pool as RedisPool, Runtime as RedisRuntime, redis::cmd,
};
use moka::future::Cache;
use std::sync::Arc;
use std::{env, time::Duration};
use tower::ServiceBuilder;
use tower_http::{compression::CompressionLayer, decompression::RequestDecompressionLayer};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

/// Web application state
struct AppState {
    memory_cache: Cache<String, Arc<Option<String>>>,
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

    // Build slug memory cache (TTL 30s)
    let memory_cache: Cache<String, Arc<Option<String>>> = Cache::builder()
        .max_capacity(100)
        .time_to_live(Duration::from_secs(30))
        .build();

    // Build the app state
    let state = Arc::new(AppState {
        redis_pool,
        pg_pool,
        memory_cache,
    });

    // Register the slug handler
    let app = Router::new()
        .route("/{slug}", get(handle_redirect))
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

async fn handle_redirect(
    State(state): State<Arc<AppState>>,
    Path(slug): Path<String>,
) -> impl IntoResponse {
    // If slug is in the memory cache, return it
    if let Some(url) = state.memory_cache.get(&slug).await {
        // If the URL is None, return 404
        if url.is_none() {
            tracing::debug!("Slug {slug} cached as None");
            return axum::http::StatusCode::NOT_FOUND.into_response();
        }
        // Otherwise, return a redirect
        let url = url.as_ref().clone().unwrap();
        tracing::debug!("Slug {} cached as {}", slug, &url);
        return Redirect::temporary(&url).into_response();
    }

    // Otherwise, look it up in Redis and Postgres
    match lookup(&slug, &state).await {
        // If slug found, cache and return it
        Ok(Some(url)) => {
            // Store in memory cache
            state
                .memory_cache
                .insert(slug, Arc::new(Some(url.clone())))
                .await;
            // Return a redirect
            Redirect::temporary(&url).into_response()
        }
        // If slug is not found, cache and return 404
        Ok(None) => {
            // Store in memory cache
            state.memory_cache.insert(slug, Arc::new(None)).await;
            // Return 404
            axum::http::StatusCode::NOT_FOUND.into_response()
        }
        // If there was an error, return 503
        Err(_) => axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}

async fn lookup(slug: &str, state: &AppState) -> Result<Option<String>> {
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
