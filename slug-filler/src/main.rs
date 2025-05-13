use anyhow::Result;
use deadpool_postgres::{
    ManagerConfig, Pool as PostgresPool, RecyclingMethod, Runtime as PgRuntime,
    tokio_postgres::NoTls,
};
use deadpool_redis::{
    Config as RedisConfig, Pool as RedisPool, Runtime as RedisRuntime, redis::cmd,
};
use rand::{Rng, distr::Uniform};
use std::collections::HashSet;
use std::{env, time::Duration};
use tokio::time;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

/// Base62 character set
const BASE62: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

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
    let queue_size: usize = env::var("QUEUE_SIZE")?.parse()?;
    let redis_url = env::var("REDIS_URL")?;
    let slug_len: usize = env::var("SLUG_LEN")?.parse()?;

    // Dynamic configuration
    let batch_size: usize = queue_size / 10; // 10% of the pool size

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

    // Inform startup
    tracing::debug!(
        "slug-filler connected to queue={queue_size}, batch={batch_size}, slug_len={slug_len}"
    );

    // Create a thread-local random number generator
    let mut rng = rand::rng();
    let dist = Uniform::new(0, BASE62.len())?;

    // Loop indefinitely every 250ms
    loop {
        if let Err(e) = refill(
            &redis_pool,
            &pg_pool,
            &mut rng,
            &dist,
            queue_size,
            slug_len,
            batch_size,
        )
        .await
        {
            tracing::warn!("Failed refill: {e:?}");
        }
        time::sleep(Duration::from_millis(250)).await;
    }
}

/// Slug filler, fills the Redis slug pool with random slugs, ensuring that they are unique
async fn refill<R: Rng + ?Sized>(
    redis_pool: &RedisPool,
    pg_pool: &PostgresPool,
    rng: &mut R,
    dist: &Uniform<usize>,
    queue_size: usize,
    slug_len: usize,
    batch_size: usize,
) -> Result<()> {
    // Get a Redis connection
    let mut redis_conn = redis_pool.get().await?;

    // If the pool is already large enough, do nothing
    let len: usize = cmd("LLEN")
        .arg("slug_pool")
        .query_async::<usize>(&mut redis_conn)
        .await?;
    if len >= queue_size {
        tracing::debug!("Current slug_pool size is {len}, no need to refill");
        return Ok(());
    }

    // Generate a random batch
    let mut batch: Vec<String> = Vec::with_capacity(batch_size);
    for _ in 0..batch_size {
        let slug: String = (0..slug_len)
            .map(|_| BASE62[rng.sample(dist)] as char)
            .collect();
        batch.push(slug);
    }

    // Get a PostgreSQL connection
    let pg_client = pg_pool.get().await?;

    // Validate against the database
    let slug_refs: Vec<&str> = batch.iter().map(|s| s.as_str()).collect();
    let rows = pg_client
        .query("SELECT slug FROM slugs WHERE slug = ANY($1)", &[&slug_refs])
        .await?;

    // Remove existing slugs from the batch
    if !rows.is_empty() {
        let taken: HashSet<&str> = rows.iter().map(|r| r.get::<usize, &str>(0)).collect();
        batch.retain(|s| !taken.contains(s.as_str()));
        tracing::debug!("Removed {} existing slugs from the batch", taken.len());
    }

    // If the batch is empty, do nothing
    if batch.is_empty() {
        tracing::debug!("No new slugs to add to the slug_pool");
        return Ok(());
    }

    // Push the batch to Redis
    cmd("RPUSH")
        .arg("slug_pool")
        .arg(&batch)
        .query_async::<()>(&mut redis_conn)
        .await?;
    tracing::debug!("Added {} slugs to the slug_pool", batch.len());

    Ok(())
}
