use crate::model::{Scd30Reading, Sen66Reading, SensorKind, SensorReading};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use sqlx::postgres::{PgPool, PgPoolOptions};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use sqlx::Row;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

const FLUSH_INTERVAL: Duration = Duration::from_secs(5);
const FLUSH_BATCH_SIZE: i64 = 100;
const EVICTION_BATCH_SIZE: i64 = 1000;
const DEFAULT_SPOOL_LIMIT_BYTES: u64 = 1024 * 1024 * 1024;

const CREATE_SCD30_TABLE: &str = r#"
CREATE TABLE IF NOT EXISTS scd30_readings (
    node_id STRING NOT NULL,
    recorded_at TIMESTAMP(3) NOT NULL TIME INDEX,
    temperature FLOAT32 NOT NULL,
    humidity FLOAT32 NOT NULL,
    co2 FLOAT32 NOT NULL,
    pressure FLOAT32,
    PRIMARY KEY (node_id)
)"#;

const CREATE_SEN66_TABLE: &str = r#"
CREATE TABLE IF NOT EXISTS sen66_readings (
    node_id STRING NOT NULL,
    recorded_at TIMESTAMP(3) NOT NULL TIME INDEX,
    temperature FLOAT32,
    humidity FLOAT32,
    co2 FLOAT32,
    pressure FLOAT32,
    pm1 FLOAT32,
    pm2_5 FLOAT32,
    pm4 FLOAT32,
    pm10 FLOAT32,
    voc_index FLOAT32,
    nox_index FLOAT32,
    PRIMARY KEY (node_id)
)"#;

const CREATE_SPOOL_TABLE: &str = r#"
CREATE TABLE IF NOT EXISTS pending_readings (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    sensor_kind TEXT NOT NULL,
    node_id TEXT NOT NULL,
    timestamp_ms INTEGER NOT NULL,
    temperature REAL,
    humidity REAL,
    co2 REAL,
    pressure REAL,
    pm1 REAL,
    pm2_5 REAL,
    pm4 REAL,
    pm10 REAL,
    voc_index REAL,
    nox_index REAL
)"#;

pub struct Storage {
    remote: PgPool,
    spool: SqlitePool,
    spool_path: PathBuf,
    spool_limit_bytes: u64,
    node_id: String,
    has_backlog: bool,
    scd30_schema_ready: bool,
    sen66_schema_ready: bool,
}

#[derive(Debug)]
struct PendingReading {
    id: i64,
    reading: SensorReading,
    node_id: String,
}

impl Storage {
    pub async fn open(remote_url: &str, node_id: String, spool_path: &Path) -> Result<Self> {
        Self::open_with_limit(remote_url, node_id, spool_path, DEFAULT_SPOOL_LIMIT_BYTES).await
    }

    async fn open_with_limit(
        remote_url: &str,
        node_id: String,
        spool_path: &Path,
        spool_limit_bytes: u64,
    ) -> Result<Self> {
        if let Some(parent) = spool_path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("failed to create spool directory {}", parent.display())
            })?;
        }

        let remote = PgPoolOptions::new()
            .max_connections(5)
            .acquire_timeout(Duration::from_secs(3))
            .connect_lazy(remote_url)
            .context("invalid GREPTIMEDB_URL")?;

        let options = SqliteConnectOptions::new()
            .filename(spool_path)
            .create_if_missing(true);
        let spool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .context("failed to open local spool")?;

        sqlx::query("PRAGMA journal_mode = WAL")
            .execute(&spool)
            .await?;
        sqlx::query("PRAGMA synchronous = FULL")
            .execute(&spool)
            .await?;
        sqlx::query("PRAGMA wal_autocheckpoint = 1000")
            .execute(&spool)
            .await?;
        sqlx::query("PRAGMA journal_size_limit = 16777216")
            .execute(&spool)
            .await?;
        sqlx::query(CREATE_SPOOL_TABLE).execute(&spool).await?;

        let page_size: i64 = sqlx::query_scalar("PRAGMA page_size")
            .fetch_one(&spool)
            .await?;
        let max_pages = (spool_limit_bytes / page_size.max(1) as u64).max(1);
        sqlx::query(&format!("PRAGMA max_page_count = {max_pages}"))
            .execute(&spool)
            .await?;

        let has_backlog =
            sqlx::query_scalar::<_, i64>("SELECT EXISTS(SELECT 1 FROM pending_readings LIMIT 1)")
                .fetch_one(&spool)
                .await?
                != 0;

        Ok(Self {
            remote,
            spool,
            spool_path: spool_path.to_path_buf(),
            spool_limit_bytes,
            node_id,
            has_backlog,
            scd30_schema_ready: false,
            sen66_schema_ready: false,
        })
    }

    pub async fn run(mut self, mut readings: mpsc::Receiver<SensorReading>) -> Result<()> {
        let mut flush_tick = tokio::time::interval(FLUSH_INTERVAL);
        flush_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                reading = readings.recv() => {
                    match reading {
                        Some(reading) => self.accept(reading).await?,
                        None => {
                            debug!("sensor channel closed; storage worker stopping");
                            return Ok(());
                        }
                    }
                }
                _ = flush_tick.tick(), if self.has_backlog => {
                    if let Err(error) = self.flush_backlog().await {
                        warn!("GreptimeDB remains unavailable; readings remain spooled: {error:#}");
                    }
                }
            }
        }
    }

    async fn accept(&mut self, reading: SensorReading) -> Result<()> {
        if !self.has_backlog {
            match self.insert_remote(&reading, &self.node_id.clone()).await {
                Ok(()) => {
                    debug!(
                        sensor = reading.kind().as_str(),
                        "reading stored in GreptimeDB"
                    );
                    return Ok(());
                }
                Err(remote_error) => {
                    warn!(
                        sensor = reading.kind().as_str(),
                        "GreptimeDB write failed; switching to durable spool: {remote_error:#}"
                    );
                }
            }
        }

        self.enqueue(&reading, &self.node_id.clone())
            .await
            .context("failed to durably spool a sensor reading")?;
        self.has_backlog = true;
        Ok(())
    }

    async fn ensure_schema(&mut self, kind: SensorKind) -> Result<()> {
        let (ready, ddl) = match kind {
            SensorKind::Scd30 => (&mut self.scd30_schema_ready, CREATE_SCD30_TABLE),
            SensorKind::Sen66 => (&mut self.sen66_schema_ready, CREATE_SEN66_TABLE),
        };
        if !*ready {
            sqlx::query(ddl)
                .execute(&self.remote)
                .await
                .with_context(|| format!("failed to create {} table", kind.as_str()))?;
            *ready = true;
            info!(sensor = kind.as_str(), "GreptimeDB table is ready");
        }
        Ok(())
    }

    async fn insert_remote(&mut self, reading: &SensorReading, node_id: &str) -> Result<()> {
        self.ensure_schema(reading.kind()).await?;
        let result = match reading {
            SensorReading::Scd30(reading) => {
                sqlx::query(
                    r#"
                    INSERT INTO scd30_readings
                        (node_id, recorded_at, temperature, humidity, co2, pressure)
                    VALUES ($1, $2, $3, $4, $5, $6)
                    "#,
                )
                .bind(node_id)
                .bind(reading.timestamp.naive_utc())
                .bind(reading.temperature_c)
                .bind(reading.humidity_percent)
                .bind(reading.co2_ppm)
                .bind(reading.pressure_mbar)
                .execute(&self.remote)
                .await
            }
            SensorReading::Sen66(reading) => {
                sqlx::query(
                    r#"
                    INSERT INTO sen66_readings
                        (node_id, recorded_at, temperature, humidity, co2, pressure,
                         pm1, pm2_5, pm4, pm10, voc_index, nox_index)
                    VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
                    "#,
                )
                .bind(node_id)
                .bind(reading.timestamp.naive_utc())
                .bind(reading.temperature_c)
                .bind(reading.humidity_percent)
                .bind(reading.co2_ppm)
                .bind(reading.pressure_mbar)
                .bind(reading.pm1_ug_m3)
                .bind(reading.pm2_5_ug_m3)
                .bind(reading.pm4_ug_m3)
                .bind(reading.pm10_ug_m3)
                .bind(reading.voc_index)
                .bind(reading.nox_index)
                .execute(&self.remote)
                .await
            }
        };

        if let Err(error) = result {
            match reading.kind() {
                SensorKind::Scd30 => self.scd30_schema_ready = false,
                SensorKind::Sen66 => self.sen66_schema_ready = false,
            }
            return Err(error).context("failed to insert reading into GreptimeDB");
        }
        Ok(())
    }

    async fn enqueue(&mut self, reading: &SensorReading, node_id: &str) -> Result<()> {
        let (temperature, humidity, co2, pressure, pm1, pm2_5, pm4, pm10, voc, nox) =
            reading_values(reading);
        let insert = sqlx::query(
            r#"
            INSERT INTO pending_readings
                (sensor_kind, node_id, timestamp_ms, temperature, humidity, co2, pressure,
                 pm1, pm2_5, pm4, pm10, voc_index, nox_index)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(reading.kind().as_str())
        .bind(node_id)
        .bind(reading.timestamp().timestamp_millis())
        .bind(temperature)
        .bind(humidity)
        .bind(co2)
        .bind(pressure)
        .bind(pm1)
        .bind(pm2_5)
        .bind(pm4)
        .bind(pm10)
        .bind(voc)
        .bind(nox)
        .execute(&self.spool)
        .await;

        if let Err(error) = insert {
            if error.to_string().contains("database or disk is full") {
                self.evict_oldest(EVICTION_BATCH_SIZE).await?;
                self.checkpoint().await;
                self.enqueue_once(reading, node_id).await?;
            } else {
                return Err(error).context("SQLite spool insert failed");
            }
        }

        self.enforce_spool_limit().await?;
        Ok(())
    }

    async fn enqueue_once(&self, reading: &SensorReading, node_id: &str) -> Result<()> {
        let (temperature, humidity, co2, pressure, pm1, pm2_5, pm4, pm10, voc, nox) =
            reading_values(reading);
        sqlx::query(
            r#"
            INSERT INTO pending_readings
                (sensor_kind, node_id, timestamp_ms, temperature, humidity, co2, pressure,
                 pm1, pm2_5, pm4, pm10, voc_index, nox_index)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(reading.kind().as_str())
        .bind(node_id)
        .bind(reading.timestamp().timestamp_millis())
        .bind(temperature)
        .bind(humidity)
        .bind(co2)
        .bind(pressure)
        .bind(pm1)
        .bind(pm2_5)
        .bind(pm4)
        .bind(pm10)
        .bind(voc)
        .bind(nox)
        .execute(&self.spool)
        .await?;
        Ok(())
    }

    async fn flush_backlog(&mut self) -> Result<()> {
        let pending = self.load_pending(FLUSH_BATCH_SIZE).await?;
        if pending.is_empty() {
            self.has_backlog = false;
            return Ok(());
        }

        let mut flushed = 0_u64;
        for pending in pending {
            self.insert_remote(&pending.reading, &pending.node_id.clone())
                .await?;
            sqlx::query("DELETE FROM pending_readings WHERE id = ?")
                .bind(pending.id)
                .execute(&self.spool)
                .await
                .context("failed to acknowledge a spooled reading")?;
            flushed += 1;
        }

        self.has_backlog =
            sqlx::query_scalar::<_, i64>("SELECT EXISTS(SELECT 1 FROM pending_readings LIMIT 1)")
                .fetch_one(&self.spool)
                .await?
                != 0;
        info!(
            flushed,
            remaining = self.has_backlog,
            "flushed durable spool"
        );
        Ok(())
    }

    async fn load_pending(&self, limit: i64) -> Result<Vec<PendingReading>> {
        let rows = sqlx::query(
            r#"
            SELECT id, sensor_kind, node_id, timestamp_ms, temperature, humidity, co2,
                   pressure, pm1, pm2_5, pm4, pm10, voc_index, nox_index
            FROM pending_readings
            ORDER BY id
            LIMIT ?
            "#,
        )
        .bind(limit)
        .fetch_all(&self.spool)
        .await?;

        rows.into_iter().map(pending_from_row).collect()
    }

    async fn enforce_spool_limit(&self) -> Result<()> {
        loop {
            let used = self.spool_usage_bytes().await?;
            if used <= self.spool_limit_bytes {
                return Ok(());
            }

            let evicted = self.evict_oldest(EVICTION_BATCH_SIZE).await?;
            if evicted == 0 {
                warn!(
                    used,
                    limit = self.spool_limit_bytes,
                    "SQLite metadata exceeds the configured spool limit"
                );
                return Ok(());
            }
            error!(
                evicted,
                used,
                limit = self.spool_limit_bytes,
                "spool limit reached; evicted oldest readings"
            );
            self.checkpoint().await;
        }
    }

    async fn evict_oldest(&self, count: i64) -> Result<u64> {
        let result = sqlx::query(
            r#"
            DELETE FROM pending_readings
            WHERE id IN (SELECT id FROM pending_readings ORDER BY id LIMIT ?)
            "#,
        )
        .bind(count)
        .execute(&self.spool)
        .await?;
        Ok(result.rows_affected())
    }

    async fn spool_usage_bytes(&self) -> Result<u64> {
        let page_size: i64 = sqlx::query_scalar("PRAGMA page_size")
            .fetch_one(&self.spool)
            .await?;
        let page_count: i64 = sqlx::query_scalar("PRAGMA page_count")
            .fetch_one(&self.spool)
            .await?;
        let free_pages: i64 = sqlx::query_scalar("PRAGMA freelist_count")
            .fetch_one(&self.spool)
            .await?;
        let live_database = (page_count - free_pages).max(0) as u64 * page_size.max(0) as u64;
        let wal_bytes = std::fs::metadata(format!("{}-wal", self.spool_path.display()))
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        Ok(live_database.saturating_add(wal_bytes))
    }

    async fn checkpoint(&self) {
        if let Err(error) = sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
            .fetch_all(&self.spool)
            .await
        {
            warn!("failed to checkpoint local spool: {error}");
        }
    }
}

#[allow(clippy::type_complexity)]
fn reading_values(
    reading: &SensorReading,
) -> (
    Option<f32>,
    Option<f32>,
    Option<f32>,
    Option<f32>,
    Option<f32>,
    Option<f32>,
    Option<f32>,
    Option<f32>,
    Option<f32>,
    Option<f32>,
) {
    match reading {
        SensorReading::Scd30(reading) => (
            Some(reading.temperature_c),
            Some(reading.humidity_percent),
            Some(reading.co2_ppm),
            reading.pressure_mbar,
            None,
            None,
            None,
            None,
            None,
            None,
        ),
        SensorReading::Sen66(reading) => (
            reading.temperature_c,
            reading.humidity_percent,
            reading.co2_ppm,
            reading.pressure_mbar,
            reading.pm1_ug_m3,
            reading.pm2_5_ug_m3,
            reading.pm4_ug_m3,
            reading.pm10_ug_m3,
            reading.voc_index,
            reading.nox_index,
        ),
    }
}

fn pending_from_row(row: sqlx::sqlite::SqliteRow) -> Result<PendingReading> {
    let timestamp_ms: i64 = row.try_get("timestamp_ms")?;
    let timestamp = DateTime::<Utc>::from_timestamp_millis(timestamp_ms)
        .context("invalid timestamp in local spool")?;
    let sensor_kind: String = row.try_get("sensor_kind")?;

    let reading = match sensor_kind.as_str() {
        "scd30" => SensorReading::Scd30(Scd30Reading {
            timestamp,
            temperature_c: required_value(&row, "temperature")?,
            humidity_percent: required_value(&row, "humidity")?,
            co2_ppm: required_value(&row, "co2")?,
            pressure_mbar: row.try_get("pressure")?,
        }),
        "sen66" => SensorReading::Sen66(Sen66Reading {
            timestamp,
            temperature_c: row.try_get("temperature")?,
            humidity_percent: row.try_get("humidity")?,
            co2_ppm: row.try_get("co2")?,
            pressure_mbar: row.try_get("pressure")?,
            pm1_ug_m3: row.try_get("pm1")?,
            pm2_5_ug_m3: row.try_get("pm2_5")?,
            pm4_ug_m3: row.try_get("pm4")?,
            pm10_ug_m3: row.try_get("pm10")?,
            voc_index: row.try_get("voc_index")?,
            nox_index: row.try_get("nox_index")?,
        }),
        other => anyhow::bail!("unknown sensor kind in local spool: {other}"),
    };

    Ok(PendingReading {
        id: row.try_get("id")?,
        node_id: row.try_get("node_id")?,
        reading,
    })
}

fn required_value(row: &sqlx::sqlite::SqliteRow, column: &str) -> Result<f32> {
    row.try_get::<Option<f32>, _>(column)?
        .with_context(|| format!("missing required {column} value in SCD30 spool row"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temporary_spool(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "sensor-monitor-{name}-{}-{unique}.db",
            std::process::id()
        ))
    }

    fn sample_reading(timestamp_ms: i64) -> SensorReading {
        SensorReading::Scd30(Scd30Reading {
            timestamp: DateTime::from_timestamp_millis(timestamp_ms).unwrap(),
            co2_ppm: 450.0,
            temperature_c: 22.5,
            humidity_percent: 50.0,
            pressure_mbar: Some(1013.0),
        })
    }

    async fn cleanup(storage: Storage, path: &Path) {
        storage.spool.close().await;
        for candidate in [
            path.to_path_buf(),
            PathBuf::from(format!("{}-wal", path.display())),
            PathBuf::from(format!("{}-shm", path.display())),
        ] {
            let _ = std::fs::remove_file(candidate);
        }
    }

    #[tokio::test]
    async fn spool_survives_reopen_and_preserves_order() {
        let path = temporary_spool("reopen");
        let mut storage = Storage::open_with_limit(
            "postgresql://localhost:1/public",
            "node-a".into(),
            &path,
            1024 * 1024,
        )
        .await
        .unwrap();
        storage
            .enqueue(&sample_reading(2_000), "node-a")
            .await
            .unwrap();
        storage
            .enqueue(&sample_reading(1_000), "node-a")
            .await
            .unwrap();
        storage.spool.close().await;

        let storage = Storage::open_with_limit(
            "postgresql://localhost:1/public",
            "node-a".into(),
            &path,
            1024 * 1024,
        )
        .await
        .unwrap();
        assert!(storage.has_backlog);
        let pending = storage.load_pending(10).await.unwrap();
        assert_eq!(pending.len(), 2);
        assert_eq!(pending[0].reading.timestamp().timestamp_millis(), 2_000);
        assert_eq!(pending[1].reading.timestamp().timestamp_millis(), 1_000);
        cleanup(storage, &path).await;
    }

    #[tokio::test]
    async fn tiny_limit_evicts_oldest_rows() {
        let path = temporary_spool("limit");
        let mut storage = Storage::open_with_limit(
            "postgresql://localhost:1/public",
            "node-a".into(),
            &path,
            16 * 1024,
        )
        .await
        .unwrap();

        for timestamp in 0..500 {
            storage
                .enqueue(&sample_reading(timestamp), "node-a")
                .await
                .unwrap();
        }

        let pending = storage.load_pending(1000).await.unwrap();
        assert!(pending.len() < 500);
        if let Some(first) = pending.first() {
            assert!(first.reading.timestamp().timestamp_millis() > 0);
        }
        cleanup(storage, &path).await;
    }

    #[tokio::test]
    #[ignore = "requires GREPTIMEDB_TEST_URL pointing at a disposable GreptimeDB database"]
    async fn greptimedb_schema_insert_and_replay_are_compatible() {
        let remote_url =
            std::env::var("GREPTIMEDB_TEST_URL").expect("GREPTIMEDB_TEST_URL must be set");
        let path = temporary_spool("greptime");
        let node_id = format!("integration-{}", std::process::id());
        let mut storage =
            Storage::open_with_limit(&remote_url, node_id.clone(), &path, 1024 * 1024)
                .await
                .unwrap();

        let timestamp = Utc::now();
        let scd30 = SensorReading::Scd30(Scd30Reading {
            timestamp,
            co2_ppm: 450.0,
            temperature_c: 22.5,
            humidity_percent: 50.0,
            pressure_mbar: None,
        });
        let sen66 = SensorReading::Sen66(Sen66Reading {
            timestamp,
            pm1_ug_m3: Some(1.0),
            pm2_5_ug_m3: Some(2.5),
            pm4_ug_m3: Some(4.0),
            pm10_ug_m3: Some(10.0),
            humidity_percent: Some(50.0),
            temperature_c: Some(22.5),
            voc_index: Some(100.0),
            nox_index: None,
            co2_ppm: None,
            pressure_mbar: None,
        });

        storage.insert_remote(&scd30, &node_id).await.unwrap();
        storage.insert_remote(&scd30, &node_id).await.unwrap();
        storage.insert_remote(&sen66, &node_id).await.unwrap();

        let scd30_count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM scd30_readings WHERE node_id = $1 AND recorded_at = $2",
        )
        .bind(&node_id)
        .bind(timestamp.naive_utc())
        .fetch_one(&storage.remote)
        .await
        .unwrap();
        let sen66_count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM sen66_readings WHERE node_id = $1 AND recorded_at = $2 \
             AND co2 IS NULL AND nox_index IS NULL",
        )
        .bind(&node_id)
        .bind(timestamp.naive_utc())
        .fetch_one(&storage.remote)
        .await
        .unwrap();

        assert_eq!(scd30_count, 1, "replay should deduplicate by node and time");
        assert_eq!(sen66_count, 1, "nullable warm-up fields should round-trip");
        cleanup(storage, &path).await;
    }
}
