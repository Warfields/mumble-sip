use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use async_trait::async_trait;
use names::Generator;
use sqlx::SqlitePool;
use tracing::info;

/// Information about a caller retrieved from the database.
pub struct CallerInfo {
    pub phone_number: String,
    pub nickname: String,
    /// For new callers this is `now`; for returning callers this is the
    /// **previous** `last_seen` value (before updating to `now`).
    pub last_seen: u64,
    /// `true` when the caller was just created (first ever call).
    pub is_new: bool,
}

/// Trait abstracting caller storage — session code depends on this, not the concrete backend.
#[async_trait]
pub trait CallerStore: Send + Sync {
    /// Look up or create a caller. Returns info including the nickname to use as Mumble username.
    /// On first call: generates a Docker-style nickname, inserts record, returns it.
    /// On subsequent calls: updates last_seen, returns existing nickname.
    async fn get_or_create_caller(&self, phone_number: &str) -> Result<CallerInfo>;

    /// Override a caller's nickname.
    async fn set_nickname(&self, phone_number: &str, nickname: &str) -> Result<()>;

    /// Record the last Mumble channel a caller was in on a given server.
    async fn set_last_channel_id(
        &self,
        phone_number: &str,
        server_host: &str,
        channel_id: u32,
    ) -> Result<()>;

    /// Look up the last Mumble channel a caller was in on a given server.
    async fn get_last_channel_id(
        &self,
        phone_number: &str,
        server_host: &str,
    ) -> Result<Option<u32>>;
}

/// Generate a Docker-style nickname (e.g. "relaxed_babbage").
pub fn generate_nickname() -> String {
    let mut generator = Generator::default();
    generator
        .next()
        .unwrap_or_else(|| "anonymous".to_string())
        .replace('-', "_")
}

/// SQLite-backed implementation of [`CallerStore`].
pub struct SqliteCallerStore {
    pool: SqlitePool,
}

impl SqliteCallerStore {
    /// Open (or create) the SQLite database and run pending migrations.
    pub async fn new(database_url: &str) -> Result<Self> {
        let pool = SqlitePool::connect(database_url).await?;
        sqlx::migrate!().run(&pool).await?;
        info!("Database ready at {}", database_url);
        Ok(Self { pool })
    }
}

fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before epoch")
        .as_secs()
}

#[async_trait]
impl CallerStore for SqliteCallerStore {
    async fn get_or_create_caller(&self, phone_number: &str) -> Result<CallerInfo> {
        let now = now_epoch() as i64;

        // Try to find existing caller
        let existing: Option<(String, String, i64)> = sqlx::query_as(
            "SELECT phone_number, nickname, last_seen FROM callers WHERE phone_number = ?",
        )
        .bind(phone_number)
        .fetch_optional(&self.pool)
        .await?;

        if let Some((phone, nickname, previous_last_seen)) = existing {
            // Update last_seen
            sqlx::query("UPDATE callers SET last_seen = ? WHERE phone_number = ?")
                .bind(now)
                .bind(phone_number)
                .execute(&self.pool)
                .await?;

            return Ok(CallerInfo {
                phone_number: phone,
                nickname,
                last_seen: previous_last_seen as u64,
                is_new: false,
            });
        }

        // New caller — generate a unique nickname
        let nickname = loop {
            let candidate = generate_nickname();
            let conflict: Option<(i64,)> =
                sqlx::query_as("SELECT 1 FROM callers WHERE nickname = ?")
                    .bind(&candidate)
                    .fetch_optional(&self.pool)
                    .await?;
            if conflict.is_none() {
                break candidate;
            }
            // Collision — loop will generate another name
        };

        sqlx::query(
            "INSERT INTO callers (phone_number, nickname, last_seen) VALUES (?, ?, ?)
             ON CONFLICT(phone_number) DO UPDATE SET last_seen = excluded.last_seen",
        )
        .bind(phone_number)
        .bind(&nickname)
        .bind(now)
        .execute(&self.pool)
        .await?;

        info!(
            "New caller registered: {} -> {}",
            phone_number, nickname
        );

        Ok(CallerInfo {
            phone_number: phone_number.to_string(),
            nickname,
            last_seen: now as u64,
            is_new: true,
        })
    }

    async fn set_nickname(&self, phone_number: &str, nickname: &str) -> Result<()> {
        sqlx::query("UPDATE callers SET nickname = ? WHERE phone_number = ?")
            .bind(nickname)
            .bind(phone_number)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn set_last_channel_id(
        &self,
        phone_number: &str,
        server_host: &str,
        channel_id: u32,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO caller_channels (phone_number, server_host, channel_id) VALUES (?, ?, ?)
             ON CONFLICT(phone_number, server_host) DO UPDATE SET channel_id = excluded.channel_id",
        )
        .bind(phone_number)
        .bind(server_host)
        .bind(channel_id as i64)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn get_last_channel_id(
        &self,
        phone_number: &str,
        server_host: &str,
    ) -> Result<Option<u32>> {
        let row: Option<(i64,)> = sqlx::query_as(
            "SELECT channel_id FROM caller_channels WHERE phone_number = ? AND server_host = ?",
        )
        .bind(phone_number)
        .bind(server_host)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(id,)| id as u32))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_store() -> SqliteCallerStore {
        SqliteCallerStore::new("sqlite::memory:").await.unwrap()
    }

    #[tokio::test]
    async fn first_call_creates_nickname() {
        let store = test_store().await;
        let info = store.get_or_create_caller("5551234567").await.unwrap();
        assert_eq!(info.phone_number, "5551234567");
        assert!(!info.nickname.is_empty());
        assert!(!info.nickname.contains('-')); // hyphens replaced with underscores
    }

    #[tokio::test]
    async fn second_call_returns_same_nickname() {
        let store = test_store().await;
        let first = store.get_or_create_caller("5551234567").await.unwrap();
        let second = store.get_or_create_caller("5551234567").await.unwrap();
        assert_eq!(first.nickname, second.nickname);
    }

    #[tokio::test]
    async fn different_numbers_get_different_records() {
        let store = test_store().await;
        let a = store.get_or_create_caller("5551111111").await.unwrap();
        let b = store.get_or_create_caller("5552222222").await.unwrap();
        assert_ne!(a.phone_number, b.phone_number);
    }

    #[tokio::test]
    async fn set_nickname_overrides() {
        let store = test_store().await;
        store.get_or_create_caller("5551234567").await.unwrap();
        store
            .set_nickname("5551234567", "custom_name")
            .await
            .unwrap();
        let info = store.get_or_create_caller("5551234567").await.unwrap();
        assert_eq!(info.nickname, "custom_name");
    }

    #[tokio::test]
    async fn set_and_get_last_channel_id() {
        let store = test_store().await;
        store.get_or_create_caller("5551234567").await.unwrap();

        // No channel stored yet
        let ch = store
            .get_last_channel_id("5551234567", "mumble.example.com")
            .await
            .unwrap();
        assert_eq!(ch, None);

        // Store a channel
        store
            .set_last_channel_id("5551234567", "mumble.example.com", 42)
            .await
            .unwrap();
        let ch = store
            .get_last_channel_id("5551234567", "mumble.example.com")
            .await
            .unwrap();
        assert_eq!(ch, Some(42));

        // Update overwrites
        store
            .set_last_channel_id("5551234567", "mumble.example.com", 7)
            .await
            .unwrap();
        let ch = store
            .get_last_channel_id("5551234567", "mumble.example.com")
            .await
            .unwrap();
        assert_eq!(ch, Some(7));
    }

    #[tokio::test]
    async fn last_channel_is_per_server() {
        let store = test_store().await;
        store.get_or_create_caller("5551234567").await.unwrap();

        store
            .set_last_channel_id("5551234567", "server-a.example.com", 10)
            .await
            .unwrap();
        store
            .set_last_channel_id("5551234567", "server-b.example.com", 20)
            .await
            .unwrap();

        let a = store
            .get_last_channel_id("5551234567", "server-a.example.com")
            .await
            .unwrap();
        let b = store
            .get_last_channel_id("5551234567", "server-b.example.com")
            .await
            .unwrap();
        assert_eq!(a, Some(10));
        assert_eq!(b, Some(20));
    }

    #[test]
    fn generate_nickname_has_underscores() {
        let name = generate_nickname();
        assert!(!name.is_empty());
        assert!(!name.contains('-'));
    }
}
