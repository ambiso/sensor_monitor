use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use clap::Parser;
use embedded_hal::i2c::{ErrorType, I2c};
use linux_embedded_hal::I2cdev;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::Arc;
use std::thread::sleep;
use std::time::Duration;
use tracing::{error, info, warn};

mod pressure;

use pressure::fetch_vienna_pressure;

/// SCD30 CO2 sensor monitor for Raspberry Pi
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Measurement interval in seconds (2-1800)
    #[arg(short, long, default_value_t = 10)]
    interval: u16,

    /// Pressure offset in mBar to add to fetched pressure
    #[arg(short, long, default_value_t = 0.0)]
    pressure_offset: f64,
}

/// SCD30 I2C address
const SCD30_ADDR: u8 = 0x61;

/// SCD30 Commands
const CMD_START_CONTINUOUS_BASE: [u8; 2] = [0x00, 0x10]; // Start continuous measurement (base command)
const CMD_DATA_READY: [u8; 2] = [0x02, 0x02]; // Get data ready status
const CMD_READ_MEASUREMENT: [u8; 2] = [0x03, 0x00]; // Read measurement
const CMD_MEASUREMENT_INTERVAL: [u8; 2] = [0x46, 0x00]; // Get/Set measurement interval

/// Sensor reading data
#[derive(Debug, Clone)]
pub struct SensorReading {
    pub timestamp: DateTime<Utc>,
    pub co2_ppm: f32,
    pub temperature_c: f32,
    pub humidity_percent: f32,
    pub pressure_mbar: Option<f64>,
}

/// SCD30 Sensor driver - generic over I2C implementation
pub struct Scd30<I2C> {
    i2c: I2C,
    address: u8,
}

impl Scd30<I2cdev> {
    /// Create a new SCD30 sensor instance using Linux I2C device
    pub fn new(i2c_bus: &str) -> Result<Self> {
        let i2c = I2cdev::new(i2c_bus).context("Failed to open I2C device")?;
        Ok(Self {
            i2c,
            address: SCD30_ADDR,
        })
    }
}

impl<I2C> Scd30<I2C>
where
    I2C: I2c,
    I2C::Error: std::fmt::Debug,
{
    /// Create a new SCD30 sensor instance with a provided I2C bus
    pub fn with_i2c(i2c: I2C) -> Self {
        Self {
            i2c,
            address: SCD30_ADDR,
        }
    }

    /// Calculate CRC8 for SCD30 (polynomial 0x31, init 0xFF)
    fn crc8(data: &[u8]) -> u8 {
        let mut crc: u8 = 0xFF;
        for byte in data {
            crc ^= byte;
            for _ in 0..8 {
                if crc & 0x80 != 0 {
                    crc = (crc << 1) ^ 0x31;
                } else {
                    crc <<= 1;
                }
            }
        }
        crc
    }

    /// Write command to sensor
    fn write_command(&mut self, command: &[u8]) -> Result<()> {
        self.i2c
            .write(self.address, command)
            .map_err(|e| anyhow::anyhow!("I2C write error: {:?}", e))?;
        Ok(())
    }

    /// Read bytes from sensor
    fn read_bytes(&mut self, buf: &mut [u8]) -> Result<()> {
        self.i2c
            .read(self.address, buf)
            .map_err(|e| anyhow::anyhow!("I2C read error: {:?}", e))?;
        Ok(())
    }

    /// Start continuous measurement with optional pressure compensation
    /// pressure_mbar: 0 disables compensation, valid range is 700-1400 mBar
    pub fn start_continuous_measurement(&mut self, pressure_mbar: u16) -> Result<()> {
        // Validate pressure: 0 (disabled) or 700-1400
        if pressure_mbar != 0 && !(700..=1400).contains(&pressure_mbar) {
            anyhow::bail!("Pressure must be 0 (disabled) or between 700 and 1400 mBar");
        }

        let pressure_bytes = pressure_mbar.to_be_bytes();
        let crc = Self::crc8(&pressure_bytes);

        let command = [
            CMD_START_CONTINUOUS_BASE[0],
            CMD_START_CONTINUOUS_BASE[1],
            pressure_bytes[0],
            pressure_bytes[1],
            crc,
        ];

        self.write_command(&command)?;
        info!(
            "Started continuous measurement with pressure compensation: {} mBar",
            if pressure_mbar == 0 {
                "disabled".to_string()
            } else {
                pressure_mbar.to_string()
            }
        );
        Ok(())
    }

    /// Check if data is ready to be read
    pub fn data_ready(&mut self) -> Result<bool> {
        self.write_command(&CMD_DATA_READY)?;
        sleep(Duration::from_millis(3));

        let mut buf = [0u8; 3];
        self.read_bytes(&mut buf)?;

        // Verify CRC
        if Self::crc8(&buf[0..2]) != buf[2] {
            anyhow::bail!("CRC mismatch in data ready response");
        }

        Ok(buf[1] == 1)
    }

    /// Read measurement from sensor
    pub fn read_measurement(&mut self, pressure_mbar: Option<f64>) -> Result<SensorReading> {
        self.write_command(&CMD_READ_MEASUREMENT)?;
        sleep(Duration::from_millis(3));

        // Read 18 bytes: 3 values * (4 bytes + 2 CRC bytes)
        let mut buf = [0u8; 18];
        self.read_bytes(&mut buf)?;

        // Parse CO2 (bytes 0-5)
        if Self::crc8(&buf[0..2]) != buf[2] || Self::crc8(&buf[3..5]) != buf[5] {
            anyhow::bail!("CRC mismatch in CO2 data");
        }
        let co2_raw = u32::from_be_bytes([buf[0], buf[1], buf[3], buf[4]]);
        let co2_ppm = f32::from_bits(co2_raw);

        // Parse Temperature (bytes 6-11)
        if Self::crc8(&buf[6..8]) != buf[8] || Self::crc8(&buf[9..11]) != buf[11] {
            anyhow::bail!("CRC mismatch in temperature data");
        }
        let temp_raw = u32::from_be_bytes([buf[6], buf[7], buf[9], buf[10]]);
        let temperature_c = f32::from_bits(temp_raw);

        // Parse Humidity (bytes 12-17)
        if Self::crc8(&buf[12..14]) != buf[14] || Self::crc8(&buf[15..17]) != buf[17] {
            anyhow::bail!("CRC mismatch in humidity data");
        }
        let hum_raw = u32::from_be_bytes([buf[12], buf[13], buf[15], buf[16]]);
        let humidity_percent = f32::from_bits(hum_raw);

        Ok(SensorReading {
            timestamp: Utc::now(),
            co2_ppm,
            temperature_c,
            humidity_percent,
            pressure_mbar,
        })
    }

    /// Wait for data and read measurement
    pub fn wait_and_read(&mut self, timeout_secs: u64, pressure_mbar: Option<f64>) -> Result<SensorReading> {
        let start = std::time::Instant::now();
        let timeout = Duration::from_secs(timeout_secs);

        while start.elapsed() < timeout {
            if self.data_ready()? {
                return self.read_measurement(pressure_mbar);
            }
            sleep(Duration::from_millis(500));
        }

        anyhow::bail!("Timeout waiting for sensor data")
    }

    /// Set measurement interval in seconds (range: 2-1800)
    pub fn set_measurement_interval(&mut self, interval_secs: u16) -> Result<()> {
        if !(2..=1800).contains(&interval_secs) {
            anyhow::bail!("Measurement interval must be between 2 and 1800 seconds");
        }

        let interval_bytes = interval_secs.to_be_bytes();
        let crc = Self::crc8(&interval_bytes);

        let command = [
            CMD_MEASUREMENT_INTERVAL[0],
            CMD_MEASUREMENT_INTERVAL[1],
            interval_bytes[0],
            interval_bytes[1],
            crc,
        ];

        self.write_command(&command)?;
        info!("Set measurement interval to {} seconds", interval_secs);
        Ok(())
    }

    /// Get the current measurement interval in seconds
    pub fn get_measurement_interval(&mut self) -> Result<u16> {
        self.write_command(&CMD_MEASUREMENT_INTERVAL)?;

        let mut buf = [0u8; 3];
        self.read_bytes(&mut buf)?;

        // Verify CRC
        if Self::crc8(&buf[0..2]) != buf[2] {
            anyhow::bail!("CRC mismatch in measurement interval response");
        }

        let interval = u16::from_be_bytes([buf[0], buf[1]]);
        Ok(interval)
    }
}

/// Insert a sensor reading into the database
async fn insert_reading(pool: &PgPool, reading: &SensorReading) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO sensor_readings (timestamp, temperature, humidity, co2, pressure)
        VALUES ($1, $2, $3, $4, $5)
        "#,
    )
    .bind(reading.timestamp)
    .bind(reading.temperature_c as f64)
    .bind(reading.humidity_percent as f64)
    .bind(reading.co2_ppm as f64)
    .bind(reading.pressure_mbar)
    .execute(pool)
    .await
    .context("Failed to insert sensor reading")?;

    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    // Parse command line arguments
    let args = Args::parse();

    // Validate interval range
    if !(2..=1800).contains(&args.interval) {
        anyhow::bail!("Measurement interval must be between 2 and 1800 seconds");
    }

    // Initialize logging
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("sensor_monitor=info".parse()?),
        )
        .init();

    // Load environment variables from .env file
    dotenvy::dotenv().ok();

    // Get database URL from environment
    let database_url =
        std::env::var("DATABASE_URL").context("DATABASE_URL environment variable not set")?;

    // Create database connection pool
    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(&database_url)
        .await
        .context("Failed to connect to database")?;

    info!("Connected to database");

    // Run migrations
    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .context("Failed to run database migrations")?;

    info!("Database migrations completed");

    // Initialize SCD30 sensor on I2C bus 1 (typical for Raspberry Pi)
    let mut sensor = Scd30::new("/dev/i2c-1").context("Failed to initialize SCD30 sensor")?;

    info!("SCD30 sensor initialized");

    // Check current measurement interval and update only if different
    // (interval is stored in NV-memory with limited write cycles)
    match sensor.get_measurement_interval() {
        Ok(current_interval) => {
            info!("Current measurement interval: {} seconds", current_interval);
            if current_interval != args.interval {
                info!(
                    "Updating measurement interval from {} to {} seconds",
                    current_interval, args.interval
                );
                sensor.set_measurement_interval(args.interval)?;
            } else {
                info!("Measurement interval already set to {} seconds, skipping write", args.interval);
            }
        }
        Err(e) => {
            warn!("Failed to read current interval: {}, setting to {} seconds", e, args.interval);
            sensor.set_measurement_interval(args.interval)?;
        }
    }

    // Fetch initial pressure from Vienna weather stations
    let current_pressure: Arc<AtomicU16> = Arc::new(AtomicU16::new(0));
    let pressure_offset = args.pressure_offset;

    // Try to get initial pressure
    let initial_pressure = match fetch_vienna_pressure(pressure_offset) {
        Ok(p) => {
            let p_u16 = (p.round() as u16).clamp(700, 1400);
            current_pressure.store(p_u16, Ordering::SeqCst);
            info!("Initial pressure set to {} mBar", p_u16);
            p_u16
        }
        Err(e) => {
            warn!("Failed to fetch initial pressure: {}, starting with compensation disabled", e);
            0
        }
    };

    // Start continuous measurement with pressure compensation
    sensor.start_continuous_measurement(initial_pressure)?;

    // Spawn background task to update pressure every hour
    let pressure_for_task = Arc::clone(&current_pressure);
    std::thread::spawn(move || {
        loop {
            // Sleep for 1 hour
            sleep(Duration::from_secs(3600));

            match fetch_vienna_pressure(pressure_offset) {
                Ok(p) => {
                    let p_u16 = (p.round() as u16).clamp(700, 1400);
                    let old_pressure = pressure_for_task.swap(p_u16, Ordering::SeqCst);
                    if old_pressure != p_u16 {
                        info!("Updated pressure from {} to {} mBar", old_pressure, p_u16);
                    }
                }
                Err(e) => {
                    warn!("Failed to fetch pressure update: {}", e);
                }
            }
        }
    });

    // Wait for sensor to warm up
    sleep(Duration::from_secs(args.interval as u64));

    info!("Starting sensor reading loop");

    // Track when we last restarted continuous measurement with new pressure
    let mut last_pressure_update: u16 = initial_pressure;

    // Main loop - read sensor data
    loop {
        // Check if pressure changed significantly (restart continuous measurement if so)
        let current_p = current_pressure.load(Ordering::SeqCst);
        if current_p != last_pressure_update && current_p > 0 {
            info!("Restarting continuous measurement with updated pressure: {} mBar", current_p);
            if let Err(e) = sensor.start_continuous_measurement(current_p) {
                error!("Failed to restart continuous measurement: {}", e);
            } else {
                last_pressure_update = current_p;
            }
        }

        let pressure_for_reading = if current_p > 0 {
            Some(current_p as f64)
        } else {
            None
        };

        match sensor.wait_and_read(30, pressure_for_reading) {
            Ok(reading) => {
                info!(
                    "CO2: {:.1} ppm, Temp: {:.2}°C, Humidity: {:.2}%, Pressure: {} mBar",
                    reading.co2_ppm,
                    reading.temperature_c,
                    reading.humidity_percent,
                    reading.pressure_mbar.map_or("N/A".to_string(), |p| format!("{:.0}", p))
                );

                if let Err(e) = insert_reading(&pool, &reading).await {
                    error!("Failed to store reading: {}", e);
                } else {
                    info!("Reading stored in database");
                }
            }
            Err(e) => {
                error!("Failed to read sensor: {}", e);
            }
        }

        // Wait before next reading based on configured interval
        sleep(Duration::from_secs(args.interval as u64));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::VecDeque;

    /// Mock I2C error type
    #[derive(Debug, Clone)]
    pub struct MockI2cError;

    impl embedded_hal::i2c::Error for MockI2cError {
        fn kind(&self) -> embedded_hal::i2c::ErrorKind {
            embedded_hal::i2c::ErrorKind::Other
        }
    }

    /// Mock I2C bus for testing SCD30 driver
    pub struct MockI2c {
        /// Commands written to the bus (address, data)
        pub written: RefCell<Vec<(u8, Vec<u8>)>>,
        /// Responses to return for read operations
        pub responses: RefCell<VecDeque<Vec<u8>>>,
        /// Whether to fail the next operation
        pub should_fail: RefCell<bool>,
    }

    impl MockI2c {
        pub fn new() -> Self {
            Self {
                written: RefCell::new(Vec::new()),
                responses: RefCell::new(VecDeque::new()),
                should_fail: RefCell::new(false),
            }
        }

        /// Queue a response to be returned on the next read
        pub fn queue_response(&self, response: Vec<u8>) {
            self.responses.borrow_mut().push_back(response);
        }

        /// Set whether operations should fail
        pub fn set_should_fail(&self, fail: bool) {
            *self.should_fail.borrow_mut() = fail;
        }

        /// Get all commands that were written
        pub fn get_written(&self) -> Vec<(u8, Vec<u8>)> {
            self.written.borrow().clone()
        }

        /// Build a valid data ready response (ready = true)
        pub fn data_ready_response(ready: bool) -> Vec<u8> {
            let data = [0x00, if ready { 0x01 } else { 0x00 }];
            let crc = Scd30::<MockI2c>::crc8_for_test(&data);
            vec![data[0], data[1], crc]
        }

        /// Build a valid measurement response with given values
        pub fn measurement_response(co2: f32, temp: f32, humidity: f32) -> Vec<u8> {
            let mut response = Vec::with_capacity(18);

            // CO2 (4 bytes as float + 2 CRC bytes)
            let co2_bytes = co2.to_bits().to_be_bytes();
            response.extend_from_slice(&co2_bytes[0..2]);
            response.push(Scd30::<MockI2c>::crc8_for_test(&co2_bytes[0..2]));
            response.extend_from_slice(&co2_bytes[2..4]);
            response.push(Scd30::<MockI2c>::crc8_for_test(&co2_bytes[2..4]));

            // Temperature
            let temp_bytes = temp.to_bits().to_be_bytes();
            response.extend_from_slice(&temp_bytes[0..2]);
            response.push(Scd30::<MockI2c>::crc8_for_test(&temp_bytes[0..2]));
            response.extend_from_slice(&temp_bytes[2..4]);
            response.push(Scd30::<MockI2c>::crc8_for_test(&temp_bytes[2..4]));

            // Humidity
            let hum_bytes = humidity.to_bits().to_be_bytes();
            response.extend_from_slice(&hum_bytes[0..2]);
            response.push(Scd30::<MockI2c>::crc8_for_test(&hum_bytes[0..2]));
            response.extend_from_slice(&hum_bytes[2..4]);
            response.push(Scd30::<MockI2c>::crc8_for_test(&hum_bytes[2..4]));

            response
        }

        /// Build a valid measurement interval response
        pub fn interval_response(interval: u16) -> Vec<u8> {
            let bytes = interval.to_be_bytes();
            let crc = Scd30::<MockI2c>::crc8_for_test(&bytes);
            vec![bytes[0], bytes[1], crc]
        }
    }

    impl ErrorType for MockI2c {
        type Error = MockI2cError;
    }

    impl I2c for MockI2c {
        fn transaction(
            &mut self,
            address: u8,
            operations: &mut [embedded_hal::i2c::Operation<'_>],
        ) -> Result<(), Self::Error> {
            if *self.should_fail.borrow() {
                return Err(MockI2cError);
            }

            for op in operations {
                match op {
                    embedded_hal::i2c::Operation::Write(data) => {
                        self.written.borrow_mut().push((address, data.to_vec()));
                    }
                    embedded_hal::i2c::Operation::Read(buf) => {
                        if let Some(response) = self.responses.borrow_mut().pop_front() {
                            let len = buf.len().min(response.len());
                            buf[..len].copy_from_slice(&response[..len]);
                        }
                    }
                }
            }
            Ok(())
        }
    }

    // Expose crc8 for testing
    impl<I2C> Scd30<I2C> {
        pub fn crc8_for_test(data: &[u8]) -> u8 {
            let mut crc: u8 = 0xFF;
            for byte in data {
                crc ^= byte;
                for _ in 0..8 {
                    if crc & 0x80 != 0 {
                        crc = (crc << 1) ^ 0x31;
                    } else {
                        crc <<= 1;
                    }
                }
            }
            crc
        }
    }

    #[test]
    fn test_crc8() {
        // Test known CRC values
        assert_eq!(Scd30::<MockI2c>::crc8_for_test(&[0x00, 0x00]), 0x81);
        assert_eq!(Scd30::<MockI2c>::crc8_for_test(&[0x00, 0x02]), 0xE3);
    }

    #[test]
    fn test_start_continuous_measurement() {
        let mock = MockI2c::new();
        let mut sensor = Scd30::with_i2c(mock);

        // Start with pressure = 0 (disabled)
        sensor.start_continuous_measurement(0).unwrap();

        let written = sensor.i2c.get_written();
        assert_eq!(written.len(), 1);
        assert_eq!(written[0].0, SCD30_ADDR);
        // Command should be [0x00, 0x10, 0x00, 0x00, CRC]
        assert_eq!(written[0].1[0..2], [0x00, 0x10]);
        assert_eq!(written[0].1[2..4], [0x00, 0x00]);
    }

    #[test]
    fn test_start_continuous_measurement_with_pressure() {
        let mock = MockI2c::new();
        let mut sensor = Scd30::with_i2c(mock);

        // Start with pressure = 1013 mBar
        sensor.start_continuous_measurement(1013).unwrap();

        let written = sensor.i2c.get_written();
        assert_eq!(written.len(), 1);
        let pressure_bytes = 1013u16.to_be_bytes();
        assert_eq!(written[0].1[2..4], pressure_bytes);
    }

    #[test]
    fn test_start_continuous_measurement_invalid_pressure() {
        let mock = MockI2c::new();
        let mut sensor = Scd30::with_i2c(mock);

        // Pressure outside valid range should fail
        assert!(sensor.start_continuous_measurement(500).is_err());
        assert!(sensor.start_continuous_measurement(1500).is_err());

        // Valid pressures should work
        assert!(sensor.start_continuous_measurement(0).is_ok());
        assert!(sensor.start_continuous_measurement(700).is_ok());
        assert!(sensor.start_continuous_measurement(1400).is_ok());
    }

    #[test]
    fn test_data_ready() {
        let mock = MockI2c::new();
        mock.queue_response(MockI2c::data_ready_response(true));

        let mut sensor = Scd30::with_i2c(mock);
        assert!(sensor.data_ready().unwrap());
    }

    #[test]
    fn test_data_not_ready() {
        let mock = MockI2c::new();
        mock.queue_response(MockI2c::data_ready_response(false));

        let mut sensor = Scd30::with_i2c(mock);
        assert!(!sensor.data_ready().unwrap());
    }

    #[test]
    fn test_read_measurement() {
        let mock = MockI2c::new();
        let expected_co2 = 450.0f32;
        let expected_temp = 22.5f32;
        let expected_humidity = 55.0f32;

        mock.queue_response(MockI2c::measurement_response(
            expected_co2,
            expected_temp,
            expected_humidity,
        ));

        let mut sensor = Scd30::with_i2c(mock);
        let reading = sensor.read_measurement(Some(1013.0)).unwrap();

        assert!((reading.co2_ppm - expected_co2).abs() < 0.01);
        assert!((reading.temperature_c - expected_temp).abs() < 0.01);
        assert!((reading.humidity_percent - expected_humidity).abs() < 0.01);
        assert_eq!(reading.pressure_mbar, Some(1013.0));
    }

    #[test]
    fn test_get_measurement_interval() {
        let mock = MockI2c::new();
        mock.queue_response(MockI2c::interval_response(10));

        let mut sensor = Scd30::with_i2c(mock);
        let interval = sensor.get_measurement_interval().unwrap();

        assert_eq!(interval, 10);
    }

    #[test]
    fn test_set_measurement_interval() {
        let mock = MockI2c::new();
        let mut sensor = Scd30::with_i2c(mock);

        sensor.set_measurement_interval(30).unwrap();

        let written = sensor.i2c.get_written();
        assert_eq!(written.len(), 1);
        // Check command bytes
        assert_eq!(written[0].1[0..2], CMD_MEASUREMENT_INTERVAL);
        // Check interval bytes (30 = 0x001E)
        assert_eq!(written[0].1[2..4], [0x00, 0x1E]);
    }

    #[test]
    fn test_set_measurement_interval_invalid() {
        let mock = MockI2c::new();
        let mut sensor = Scd30::with_i2c(mock);

        // Invalid intervals
        assert!(sensor.set_measurement_interval(1).is_err());
        assert!(sensor.set_measurement_interval(1801).is_err());

        // Valid intervals
        assert!(sensor.set_measurement_interval(2).is_ok());
        assert!(sensor.set_measurement_interval(1800).is_ok());
    }

    #[test]
    fn test_i2c_error_handling() {
        let mock = MockI2c::new();
        mock.set_should_fail(true);

        let mut sensor = Scd30::with_i2c(mock);
        assert!(sensor.start_continuous_measurement(0).is_err());
    }

    #[test]
    fn test_crc_mismatch_detection() {
        let mock = MockI2c::new();
        // Queue a response with invalid CRC
        mock.queue_response(vec![0x00, 0x01, 0xFF]); // Wrong CRC

        let mut sensor = Scd30::with_i2c(mock);
        assert!(sensor.data_ready().is_err());
    }
}