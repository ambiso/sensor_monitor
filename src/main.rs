use anyhow::{Context, Result};
use clap::Parser;
use model::{Scd30Reading, Sen66Reading, SensorReading};
use pressure::fetch_vienna_pressure;
use scd30::Scd30;
use sen66::Sen66;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;
use storage::Storage;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

mod model;
mod pressure;
mod scd30;
mod sen66;
mod sensirion;
mod storage;

const I2C_BUS: &str = "/dev/i2c-1";
const SENSOR_REPROBE_INTERVAL: Duration = Duration::from_secs(30);
const MAX_CONSECUTIVE_SENSOR_ERRORS: u8 = 3;

/// SCD30 and SEN66 air-quality monitor for Raspberry Pi.
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Default storage interval in seconds for both sensors.
    #[arg(short, long, default_value_t = 10)]
    interval: u16,

    /// Override the SCD30 storage interval (2-1800 seconds).
    #[arg(long)]
    scd30_interval: Option<u16>,

    /// Override the SEN66 storage interval (1-1800 seconds).
    #[arg(long)]
    sen66_interval: Option<u16>,

    /// Pressure offset in mBar to add to fetched pressure.
    #[arg(short, long, default_value_t = 0.0, allow_hyphen_values = true)]
    pressure_offset: f32,

    /// Generate deterministic readings instead of opening the I2C bus.
    #[arg(long)]
    mock_sensors: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let scd30_interval = args.scd30_interval.unwrap_or(args.interval);
    let sen66_interval = args.sen66_interval.unwrap_or(args.interval);
    validate_intervals(scd30_interval, sen66_interval)?;

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("sensor_monitor=info".parse()?),
        )
        .init();
    dotenvy::dotenv().ok();

    let greptimedb_url = std::env::var("GREPTIMEDB_URL").context("GREPTIMEDB_URL is not set")?;
    let node_id = std::env::var("NODE_ID").context("NODE_ID is not set")?;
    validate_node_id(&node_id)?;
    let spool_path = std::env::var_os("SPOOL_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/var/lib/sensor_monitor/spool.db"));

    let storage = Storage::open(&greptimedb_url, node_id.clone(), &spool_path).await?;
    info!(
        node_id,
        spool = %spool_path.display(),
        "storage pipeline initialized"
    );

    let running = Arc::new(AtomicBool::new(true));
    let pressure = Arc::new(AtomicU16::new(if args.mock_sensors { 1013 } else { 0 }));
    if !args.mock_sensors {
        update_pressure_once(&pressure, args.pressure_offset).await;
    }

    let (reading_tx, reading_rx) = mpsc::channel(1024);
    let sensor_workers = if args.mock_sensors {
        info!("mock sensor mode enabled; I2C will not be opened");
        spawn_mock_sensor_workers(
            Arc::clone(&running),
            Arc::clone(&pressure),
            reading_tx.clone(),
            scd30_interval,
            sen66_interval,
        )
    } else {
        vec![
            spawn_scd30_worker(
                Arc::clone(&running),
                Arc::clone(&pressure),
                reading_tx.clone(),
                scd30_interval,
            ),
            spawn_sen66_worker(
                Arc::clone(&running),
                Arc::clone(&pressure),
                reading_tx.clone(),
                sen66_interval,
            ),
        ]
    };
    let pressure_task = (!args.mock_sensors).then(|| {
        spawn_pressure_updater(
            Arc::clone(&running),
            Arc::clone(&pressure),
            args.pressure_offset,
        )
    });

    let mut storage_task = tokio::spawn(storage.run(reading_rx));
    let storage_finished = tokio::select! {
        signal = tokio::signal::ctrl_c() => {
            signal.context("failed to listen for shutdown signal")?;
            info!("shutdown requested");
            None
        }
        result = &mut storage_task => Some(result),
    };

    running.store(false, Ordering::SeqCst);
    for worker in sensor_workers {
        if worker.join().is_err() {
            error!("sensor worker panicked during shutdown");
        }
    }
    drop(reading_tx);
    if let Some(task) = pressure_task {
        task.abort();
    }

    match storage_finished {
        Some(result) => result.context("storage task panicked")??,
        None => storage_task.await.context("storage task panicked")??,
    }
    info!("sensor monitor stopped");
    Ok(())
}

fn validate_intervals(scd30_interval: u16, sen66_interval: u16) -> Result<()> {
    if !(2..=1800).contains(&scd30_interval) {
        anyhow::bail!("SCD30 interval must be between 2 and 1800 seconds");
    }
    if !(1..=1800).contains(&sen66_interval) {
        anyhow::bail!("SEN66 interval must be between 1 and 1800 seconds");
    }
    Ok(())
}

fn validate_node_id(node_id: &str) -> Result<()> {
    if node_id.is_empty() || node_id.len() > 128 {
        anyhow::bail!("NODE_ID must contain between 1 and 128 characters");
    }
    if !node_id
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        anyhow::bail!("NODE_ID may contain only letters, numbers, '.', '-' and '_'");
    }
    Ok(())
}

async fn update_pressure_once(pressure: &AtomicU16, pressure_offset: f32) {
    match fetch_vienna_pressure(pressure_offset).await {
        Ok(value) => {
            let pressure_mbar = value.round().clamp(700.0, 1400.0) as u16;
            pressure.store(pressure_mbar, Ordering::SeqCst);
            info!(pressure_mbar, "ambient pressure updated");
        }
        Err(error) => {
            warn!("failed to fetch ambient pressure; compensation remains unchanged: {error:#}");
        }
    }
}

fn spawn_pressure_updater(
    running: Arc<AtomicBool>,
    pressure: Arc<AtomicU16>,
    pressure_offset: f32,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while running.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_secs(3600)).await;
            if running.load(Ordering::SeqCst) {
                update_pressure_once(&pressure, pressure_offset).await;
            }
        }
    })
}

fn spawn_mock_sensor_workers(
    running: Arc<AtomicBool>,
    pressure: Arc<AtomicU16>,
    readings: mpsc::Sender<SensorReading>,
    scd30_interval_secs: u16,
    sen66_interval_secs: u16,
) -> Vec<JoinHandle<()>> {
    let scd30_running = Arc::clone(&running);
    let scd30_pressure = Arc::clone(&pressure);
    let scd30_readings = readings.clone();
    let scd30 = thread::Builder::new()
        .name("mock-scd30-worker".into())
        .spawn(move || {
            let mut sequence = 0_u32;
            while scd30_running.load(Ordering::SeqCst) {
                let reading = mock_scd30_reading(sequence, &scd30_pressure);
                if scd30_readings
                    .blocking_send(SensorReading::Scd30(reading))
                    .is_err()
                {
                    break;
                }
                sequence = sequence.wrapping_add(1);
                if !interruptible_sleep(
                    &scd30_running,
                    Duration::from_secs(scd30_interval_secs as u64),
                ) {
                    break;
                }
            }
        })
        .expect("failed to spawn mock SCD30 worker");

    let sen66 = thread::Builder::new()
        .name("mock-sen66-worker".into())
        .spawn(move || {
            let mut sequence = 0_u32;
            while running.load(Ordering::SeqCst) {
                let reading = mock_sen66_reading(sequence, &pressure);
                if readings
                    .blocking_send(SensorReading::Sen66(reading))
                    .is_err()
                {
                    break;
                }
                sequence = sequence.wrapping_add(1);
                if !interruptible_sleep(&running, Duration::from_secs(sen66_interval_secs as u64)) {
                    break;
                }
            }
        })
        .expect("failed to spawn mock SEN66 worker");

    vec![scd30, sen66]
}

fn mock_scd30_reading(sequence: u32, pressure: &AtomicU16) -> Scd30Reading {
    let variation = (sequence % 10) as f32;
    Scd30Reading {
        timestamp: chrono::Utc::now(),
        co2_ppm: 420.0 + variation,
        temperature_c: 22.0 + variation / 10.0,
        humidity_percent: 45.0 + variation / 10.0,
        pressure_mbar: applied_pressure(pressure, 1400),
    }
}

fn mock_sen66_reading(sequence: u32, pressure: &AtomicU16) -> Sen66Reading {
    let variation = (sequence % 10) as f32;
    Sen66Reading {
        timestamp: chrono::Utc::now(),
        pm1_ug_m3: Some(2.0 + variation / 10.0),
        pm2_5_ug_m3: Some(3.0 + variation / 10.0),
        pm4_ug_m3: Some(4.0 + variation / 10.0),
        pm10_ug_m3: Some(5.0 + variation / 10.0),
        humidity_percent: Some(45.0 + variation / 10.0),
        temperature_c: Some(22.0 + variation / 10.0),
        voc_index: Some(100.0 + variation),
        nox_index: Some(1.0 + variation / 10.0),
        co2_ppm: Some(425.0 + variation),
        pressure_mbar: applied_pressure(pressure, 1200),
    }
}

fn spawn_scd30_worker(
    running: Arc<AtomicBool>,
    pressure: Arc<AtomicU16>,
    readings: mpsc::Sender<SensorReading>,
    interval_secs: u16,
) -> JoinHandle<()> {
    thread::Builder::new()
        .name("scd30-worker".into())
        .spawn(move || {
            let mut previously_missing = false;
            while running.load(Ordering::SeqCst) {
                let mut sensor = match initialize_scd30(interval_secs, &pressure) {
                    Ok(sensor) => {
                        info!(interval_secs, "SCD30 connected and monitoring started");
                        previously_missing = false;
                        sensor
                    }
                    Err(error) => {
                        if !previously_missing {
                            warn!("SCD30 not available; probing every 30 seconds: {error:#}");
                            previously_missing = true;
                        } else {
                            debug!("SCD30 still unavailable: {error:#}");
                        }
                        if !interruptible_sleep(&running, SENSOR_REPROBE_INTERVAL) {
                            break;
                        }
                        continue;
                    }
                };

                let mut failures = 0_u8;
                let mut current_applied_pressure = applied_pressure(&pressure, 1400);
                while running.load(Ordering::SeqCst) {
                    let latest_pressure = applied_pressure(&pressure, 1400);
                    if let Some(value) =
                        latest_pressure.filter(|value| Some(*value) != current_applied_pressure)
                    {
                        let value = value as u16;
                        match sensor.start_continuous_measurement(value) {
                            Ok(()) => {
                                current_applied_pressure = latest_pressure;
                                info!(pressure_mbar = value, "updated SCD30 pressure compensation");
                            }
                            Err(error) => warn!("failed to update SCD30 pressure: {error:#}"),
                        }
                    }

                    let timeout = Duration::from_secs(interval_secs as u64 + 5);
                    match sensor.wait_and_read(timeout, current_applied_pressure) {
                        Ok(reading) => {
                            failures = 0;
                            debug!(
                                co2 = reading.co2_ppm,
                                temperature = reading.temperature_c,
                                humidity = reading.humidity_percent,
                                "SCD30 reading"
                            );
                            if readings
                                .blocking_send(SensorReading::Scd30(reading))
                                .is_err()
                            {
                                return;
                            }
                        }
                        Err(error) => {
                            failures += 1;
                            warn!(failures, "failed to read SCD30 measurement: {error:#}");
                            if failures >= MAX_CONSECUTIVE_SENSOR_ERRORS {
                                warn!("SCD30 marked offline; returning to hot-plug detection");
                                break;
                            }
                        }
                    }

                    if !interruptible_sleep(&running, Duration::from_secs(interval_secs as u64)) {
                        break;
                    }
                }
                let _ = sensor.stop_measurement();
            }
        })
        .expect("failed to spawn SCD30 worker")
}

fn initialize_scd30(
    interval_secs: u16,
    pressure: &AtomicU16,
) -> Result<Scd30<linux_embedded_hal::I2cdev>> {
    let mut sensor = Scd30::new(I2C_BUS).context("failed to open SCD30")?;
    match sensor.get_measurement_interval() {
        Ok(current) if current == interval_secs => {}
        Ok(current) => {
            info!(
                current,
                requested = interval_secs,
                "changing SCD30 interval"
            );
            sensor.set_measurement_interval(interval_secs)?;
        }
        Err(error) => {
            debug!("could not read SCD30 interval; trying to configure it: {error:#}");
            sensor.set_measurement_interval(interval_secs)?;
        }
    }
    sensor.start_continuous_measurement(
        applied_pressure(pressure, 1400)
            .map(|value| value as u16)
            .unwrap_or(0),
    )?;
    Ok(sensor)
}

fn spawn_sen66_worker(
    running: Arc<AtomicBool>,
    pressure: Arc<AtomicU16>,
    readings: mpsc::Sender<SensorReading>,
    interval_secs: u16,
) -> JoinHandle<()> {
    thread::Builder::new()
        .name("sen66-worker".into())
        .spawn(move || {
            let mut previously_missing = false;
            while running.load(Ordering::SeqCst) {
                let mut sensor = match initialize_sen66(&pressure) {
                    Ok(sensor) => {
                        info!(interval_secs, "SEN66 connected and monitoring started");
                        previously_missing = false;
                        sensor
                    }
                    Err(error) => {
                        if !previously_missing {
                            warn!("SEN66 not available; probing every 30 seconds: {error:#}");
                            previously_missing = true;
                        } else {
                            debug!("SEN66 still unavailable: {error:#}");
                        }
                        if !interruptible_sleep(&running, SENSOR_REPROBE_INTERVAL) {
                            break;
                        }
                        continue;
                    }
                };

                let mut failures = 0_u8;
                let mut current_applied_pressure = applied_pressure(&pressure, 1200);
                while running.load(Ordering::SeqCst) {
                    let latest_pressure = applied_pressure(&pressure, 1200);
                    if let Some(value) =
                        latest_pressure.filter(|value| Some(*value) != current_applied_pressure)
                    {
                        let value = value as u16;
                        match sensor.set_ambient_pressure(value) {
                            Ok(()) => {
                                current_applied_pressure = latest_pressure;
                                info!(pressure_mbar = value, "updated SEN66 pressure compensation");
                            }
                            Err(error) => warn!("failed to update SEN66 pressure: {error:#}"),
                        }
                    }

                    match sensor.wait_and_read(Duration::from_secs(5), current_applied_pressure) {
                        Ok(reading) => {
                            failures = 0;
                            debug!(
                                co2 = ?reading.co2_ppm,
                                pm2_5 = ?reading.pm2_5_ug_m3,
                                voc = ?reading.voc_index,
                                nox = ?reading.nox_index,
                                "SEN66 reading"
                            );
                            if readings
                                .blocking_send(SensorReading::Sen66(reading))
                                .is_err()
                            {
                                return;
                            }
                        }
                        Err(error) => {
                            failures += 1;
                            warn!(failures, "failed to read SEN66 measurement: {error:#}");
                            if failures >= MAX_CONSECUTIVE_SENSOR_ERRORS {
                                warn!("SEN66 marked offline; returning to hot-plug detection");
                                break;
                            }
                        }
                    }

                    if !interruptible_sleep(&running, Duration::from_secs(interval_secs as u64)) {
                        break;
                    }
                }
                let _ = sensor.stop_measurement();
            }
        })
        .expect("failed to spawn SEN66 worker")
}

fn initialize_sen66(pressure: &AtomicU16) -> Result<Sen66<linux_embedded_hal::I2cdev>> {
    let mut sensor = Sen66::new(I2C_BUS).context("failed to open SEN66")?;
    sensor.probe().context("SEN66 probe failed")?;
    sensor.stop_measurement()?;
    if let Some(value) = applied_pressure(pressure, 1200) {
        sensor.set_ambient_pressure(value as u16)?;
    }
    sensor.start_continuous_measurement()?;
    Ok(sensor)
}

fn applied_pressure(pressure: &AtomicU16, maximum: u16) -> Option<f32> {
    let value = pressure.load(Ordering::SeqCst);
    (value > 0).then_some(value.clamp(700, maximum) as f32)
}

fn interruptible_sleep(running: &AtomicBool, duration: Duration) -> bool {
    let deadline = std::time::Instant::now() + duration;
    while running.load(Ordering::SeqCst) {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return true;
        }
        thread::sleep(remaining.min(Duration::from_millis(200)));
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interval_validation_respects_sensor_limits() {
        assert!(validate_intervals(2, 1).is_ok());
        assert!(validate_intervals(1800, 1800).is_ok());
        assert!(validate_intervals(1, 10).is_err());
        assert!(validate_intervals(10, 0).is_err());
    }

    #[test]
    fn node_id_validation_is_strict_but_practical() {
        assert!(validate_node_id("living-room.pi_1").is_ok());
        assert!(validate_node_id("").is_err());
        assert!(validate_node_id("node with spaces").is_err());
        assert!(validate_node_id("node/one").is_err());
    }

    #[test]
    fn pressure_is_clamped_per_sensor() {
        let pressure = AtomicU16::new(1300);
        assert_eq!(applied_pressure(&pressure, 1400), Some(1300.0));
        assert_eq!(applied_pressure(&pressure, 1200), Some(1200.0));
        pressure.store(0, Ordering::SeqCst);
        assert_eq!(applied_pressure(&pressure, 1200), None);
    }

    #[test]
    fn sensor_kind_labels_are_stable() {
        assert_eq!(model::SensorKind::Scd30.as_str(), "scd30");
        assert_eq!(model::SensorKind::Sen66.as_str(), "sen66");
    }

    #[test]
    fn mock_readings_are_complete_and_vary() {
        let pressure = AtomicU16::new(1013);
        let first_scd30 = mock_scd30_reading(0, &pressure);
        let next_scd30 = mock_scd30_reading(1, &pressure);
        assert_eq!(first_scd30.co2_ppm, 420.0);
        assert_eq!(next_scd30.co2_ppm, 421.0);
        assert_eq!(first_scd30.pressure_mbar, Some(1013.0));

        let sen66 = mock_sen66_reading(0, &pressure);
        assert_eq!(sen66.pm2_5_ug_m3, Some(3.0));
        assert_eq!(sen66.co2_ppm, Some(425.0));
        assert_eq!(sen66.pressure_mbar, Some(1013.0));
    }
}
