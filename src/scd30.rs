use crate::model::Scd30Reading;
use crate::sensirion::{command_with_word, crc8, decode_words};
use anyhow::{Context, Result};
use chrono::Utc;
use embedded_hal::i2c::I2c;
use linux_embedded_hal::I2cdev;
use std::thread::sleep;
use std::time::{Duration, Instant};

const ADDRESS: u8 = 0x61;
const CMD_START_CONTINUOUS: u16 = 0x0010;
const CMD_STOP_MEASUREMENT: [u8; 2] = [0x01, 0x04];
const CMD_DATA_READY: [u8; 2] = [0x02, 0x02];
const CMD_READ_MEASUREMENT: [u8; 2] = [0x03, 0x00];
const CMD_MEASUREMENT_INTERVAL: u16 = 0x4600;

pub struct Scd30<I2C> {
    i2c: I2C,
}

impl Scd30<I2cdev> {
    pub fn new(i2c_bus: &str) -> Result<Self> {
        let i2c = I2cdev::new(i2c_bus).context("failed to open I2C device")?;
        Ok(Self { i2c })
    }
}

impl<I2C> Scd30<I2C>
where
    I2C: I2c,
    I2C::Error: std::fmt::Debug,
{
    #[cfg(test)]
    fn with_i2c(i2c: I2C) -> Self {
        Self { i2c }
    }

    fn write(&mut self, bytes: &[u8]) -> Result<()> {
        self.i2c
            .write(ADDRESS, bytes)
            .map_err(|error| anyhow::anyhow!("SCD30 I2C write error: {error:?}"))
    }

    fn read(&mut self, bytes: &mut [u8]) -> Result<()> {
        self.i2c
            .read(ADDRESS, bytes)
            .map_err(|error| anyhow::anyhow!("SCD30 I2C read error: {error:?}"))
    }

    pub fn start_continuous_measurement(&mut self, pressure_mbar: u16) -> Result<()> {
        if pressure_mbar != 0 && !(700..=1400).contains(&pressure_mbar) {
            anyhow::bail!("SCD30 pressure must be 0 or between 700 and 1400 mBar");
        }
        self.write(&command_with_word(CMD_START_CONTINUOUS, pressure_mbar))
    }

    pub fn stop_measurement(&mut self) -> Result<()> {
        self.write(&CMD_STOP_MEASUREMENT)?;
        sleep(Duration::from_millis(100));
        Ok(())
    }

    pub fn data_ready(&mut self) -> Result<bool> {
        self.write(&CMD_DATA_READY)?;
        sleep(Duration::from_millis(3));
        let mut response = [0_u8; 3];
        self.read(&mut response)?;
        let words = decode_words::<1>(&response)?;
        Ok(u16::from_be_bytes(words[0]) == 1)
    }

    pub fn read_measurement(&mut self, pressure_mbar: Option<f32>) -> Result<Scd30Reading> {
        self.write(&CMD_READ_MEASUREMENT)?;
        sleep(Duration::from_millis(3));
        let mut response = [0_u8; 18];
        self.read(&mut response)?;

        for chunk in response.chunks_exact(3) {
            if crc8(&chunk[..2]) != chunk[2] {
                anyhow::bail!("CRC mismatch in SCD30 measurement");
            }
        }

        let decode_float = |offset: usize| {
            f32::from_bits(u32::from_be_bytes([
                response[offset],
                response[offset + 1],
                response[offset + 3],
                response[offset + 4],
            ]))
        };

        Ok(Scd30Reading {
            timestamp: Utc::now(),
            co2_ppm: decode_float(0),
            temperature_c: decode_float(6),
            humidity_percent: decode_float(12),
            pressure_mbar,
        })
    }

    pub fn wait_and_read(
        &mut self,
        timeout: Duration,
        pressure_mbar: Option<f32>,
    ) -> Result<Scd30Reading> {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if self.data_ready()? {
                return self.read_measurement(pressure_mbar);
            }
            sleep(Duration::from_millis(500));
        }
        anyhow::bail!("timeout waiting for SCD30 data")
    }

    pub fn set_measurement_interval(&mut self, interval_secs: u16) -> Result<()> {
        if !(2..=1800).contains(&interval_secs) {
            anyhow::bail!("SCD30 interval must be between 2 and 1800 seconds");
        }
        self.write(&command_with_word(CMD_MEASUREMENT_INTERVAL, interval_secs))
    }

    pub fn get_measurement_interval(&mut self) -> Result<u16> {
        self.write(&CMD_MEASUREMENT_INTERVAL.to_be_bytes())?;
        let mut response = [0_u8; 3];
        self.read(&mut response)?;
        Ok(u16::from_be_bytes(decode_words::<1>(&response)?[0]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use embedded_hal::i2c::{ErrorKind, ErrorType, Operation};
    use std::collections::VecDeque;

    #[derive(Debug)]
    struct MockError;

    impl embedded_hal::i2c::Error for MockError {
        fn kind(&self) -> ErrorKind {
            ErrorKind::Other
        }
    }

    struct MockI2c {
        writes: Vec<Vec<u8>>,
        reads: VecDeque<Vec<u8>>,
    }

    impl MockI2c {
        fn new(reads: Vec<Vec<u8>>) -> Self {
            Self {
                writes: Vec::new(),
                reads: reads.into(),
            }
        }
    }

    impl ErrorType for MockI2c {
        type Error = MockError;
    }

    impl I2c for MockI2c {
        fn transaction(
            &mut self,
            address: u8,
            operations: &mut [Operation<'_>],
        ) -> std::result::Result<(), Self::Error> {
            assert_eq!(address, ADDRESS);
            for operation in operations {
                match operation {
                    Operation::Write(bytes) => self.writes.push(bytes.to_vec()),
                    Operation::Read(buffer) => {
                        let response = self.reads.pop_front().unwrap();
                        buffer.copy_from_slice(&response);
                    }
                }
            }
            Ok(())
        }
    }

    fn encoded_word(word: [u8; 2]) -> Vec<u8> {
        vec![word[0], word[1], crc8(&word)]
    }

    fn encoded_float(value: f32) -> Vec<u8> {
        let bytes = value.to_bits().to_be_bytes();
        let mut result = encoded_word([bytes[0], bytes[1]]);
        result.extend(encoded_word([bytes[2], bytes[3]]));
        result
    }

    #[test]
    fn interval_is_read_and_only_valid_values_are_written() {
        let mock = MockI2c::new(vec![encoded_word(10_u16.to_be_bytes())]);
        let mut sensor = Scd30::with_i2c(mock);
        assert_eq!(sensor.get_measurement_interval().unwrap(), 10);
        sensor.set_measurement_interval(30).unwrap();
        assert!(sensor.set_measurement_interval(1).is_err());
        assert_eq!(sensor.i2c.writes[0], [0x46, 0x00]);
        assert_eq!(&sensor.i2c.writes[1][..4], [0x46, 0x00, 0x00, 0x1e]);
    }

    #[test]
    fn pressure_validation_and_command_are_correct() {
        let mut sensor = Scd30::with_i2c(MockI2c::new(vec![]));
        sensor.start_continuous_measurement(1013).unwrap();
        assert!(sensor.start_continuous_measurement(699).is_err());
        assert_eq!(&sensor.i2c.writes[0][..4], [0x00, 0x10, 0x03, 0xf5]);
    }

    #[test]
    fn measurement_is_decoded() {
        let mut response = encoded_float(450.0);
        response.extend(encoded_float(23.5));
        response.extend(encoded_float(55.0));
        let mut sensor = Scd30::with_i2c(MockI2c::new(vec![response]));
        let reading = sensor.read_measurement(Some(1013.0)).unwrap();
        assert!((reading.co2_ppm - 450.0).abs() < f32::EPSILON);
        assert!((reading.temperature_c - 23.5).abs() < f32::EPSILON);
        assert!((reading.humidity_percent - 55.0).abs() < f32::EPSILON);
        assert_eq!(reading.pressure_mbar, Some(1013.0));
    }

    #[test]
    fn readiness_checks_crc() {
        let mut sensor = Scd30::with_i2c(MockI2c::new(vec![encoded_word([0, 1])]));
        assert!(sensor.data_ready().unwrap());

        let mut sensor = Scd30::with_i2c(MockI2c::new(vec![vec![0, 1, 0]]));
        assert!(sensor.data_ready().is_err());
    }
}
