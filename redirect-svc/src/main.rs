use anyhow::{Result};
use axum::{extract::Path, response::{IntoResponse, Redirect}, routing::get, Router};
use deadpool_postgres::{tokio_postgres::{NoTls}, ManagerConfig, Pool as PostgresPool, RecyclingMethod, Runtime as PgRuntime};
use deadpool_redis::{redis::{cmd}, Config as RedisConfig, Pool as RedisPool, Runtime as RedisRuntime};
use moka::future::Cache;
use std::{env, time::Duration};

#[derive(Clone)]
struct AppState {
    memory_cache: Cache<String, String>,
    pg_pool: PostgresPool,
    redis_pool: RedisPool,
}

#[tokio::main]
async fn main() -> Result<()> {
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
    let memory_cache = Cache::builder()
        .max_capacity(100)
        .time_to_live(Duration::from_secs(30))
        .build();

    // Build the app state
    let state = AppState { redis_pool, pg_pool, memory_cache };

    // Register the slug handler
    let app = Router::new()
        .route("/{slug}", get(handle_redirect))
        .with_state(state);

    // Start the server
    let listener = tokio::net::TcpListener::bind("0.0.0.0:8080").await?;
    axum::serve(listener, app).await?;

    // Inform startup
    println!("redirect-svc running on 0.0.0.0:8080");
    Ok(())
}

async fn handle_redirect(
    Path(slug): Path<String>,
    axum::extract::State(state): axum::extract::State<AppState>,
) -> impl IntoResponse {
    // If slug is in the memory cache, return it
    if let Some(url) = state.memory_cache.get(&slug).await {
        println!("Slug {slug} found in memory cache");
        return Redirect::temporary(&url).into_response();
    }

    // Otherwise, look it up in Redis and Postgres
    match lookup(&slug, &state).await {
        Ok(Some(url)) => {
            state.memory_cache.insert(slug.clone(), url.clone()).await;
            Redirect::temporary(&url).into_response()
        }
        Ok(None) => axum::http::StatusCode::NOT_FOUND.into_response(),
        Err(_) => axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}

async fn lookup(slug: &str, state: &AppState) -> Result<Option<String>> {
    // Get a Redis connection
    let mut redis_conn = state.redis_pool.get().await?;

    // If slug is in Redis, return it
    if let Some(url) = cmd("GET").arg(&slug).query_async::<Option<String>>(&mut redis_conn).await? {
        println!("Slug {slug} found in Redis");
        return Ok(Some(url));
    }

    // Get a PostgreSQL connection
    let pg_client = state.pg_pool.get().await?;

    // Look up the slug in PostgreSQL
    let rows = pg_client.query("SELECT url FROM slugs WHERE slug=$1", &[&slug]).await?;

    // If not found, return None
    if rows.is_empty() {
        println!("Slug {slug} not found");
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
        println!("Stored slug {slug} in Redis");
    });
    Ok(Some(url))
}
