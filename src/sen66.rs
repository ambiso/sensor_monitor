use crate::model::Sen66Reading;
use crate::sensirion::{command_bytes, command_with_word, decode_words};
use anyhow::{Context, Result};
use chrono::Utc;
use embedded_hal::i2c::I2c;
use linux_embedded_hal::I2cdev;
use std::thread::sleep;
use std::time::{Duration, Instant};

const ADDRESS: u8 = 0x6b;
const CMD_START_CONTINUOUS: u16 = 0x0021;
const CMD_STOP_MEASUREMENT: u16 = 0x0104;
const CMD_DATA_READY: u16 = 0x0202;
const CMD_READ_MEASURED_VALUES: u16 = 0x0300;
const CMD_SET_AMBIENT_PRESSURE: u16 = 0x6720;

const UNKNOWN_U16: u16 = u16::MAX;
const UNKNOWN_I16: i16 = i16::MAX;

pub struct Sen66<I2C> {
    i2c: I2C,
}

impl Sen66<I2cdev> {
    pub fn new(i2c_bus: &str) -> Result<Self> {
        let i2c = I2cdev::new(i2c_bus).context("failed to open I2C device")?;
        Ok(Self { i2c })
    }
}

impl<I2C> Sen66<I2C>
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
            .map_err(|error| anyhow::anyhow!("SEN66 I2C write error: {error:?}"))
    }

    fn read(&mut self, bytes: &mut [u8]) -> Result<()> {
        self.i2c
            .read(ADDRESS, bytes)
            .map_err(|error| anyhow::anyhow!("SEN66 I2C read error: {error:?}"))
    }

    pub fn probe(&mut self) -> Result<()> {
        self.write(&command_bytes(CMD_DATA_READY))?;
        sleep(Duration::from_millis(20));
        let mut response = [0_u8; 3];
        self.read(&mut response)?;
        decode_words::<1>(&response)?;
        Ok(())
    }

    pub fn start_continuous_measurement(&mut self) -> Result<()> {
        self.write(&command_bytes(CMD_START_CONTINUOUS))?;
        sleep(Duration::from_millis(50));
        Ok(())
    }

    pub fn stop_measurement(&mut self) -> Result<()> {
        self.write(&command_bytes(CMD_STOP_MEASUREMENT))?;
        sleep(Duration::from_millis(1400));
        Ok(())
    }

    pub fn set_ambient_pressure(&mut self, pressure_mbar: u16) -> Result<()> {
        if !(700..=1200).contains(&pressure_mbar) {
            anyhow::bail!("SEN66 pressure must be between 700 and 1200 mBar");
        }
        self.write(&command_with_word(CMD_SET_AMBIENT_PRESSURE, pressure_mbar))?;
        sleep(Duration::from_millis(20));
        Ok(())
    }

    pub fn data_ready(&mut self) -> Result<bool> {
        self.write(&command_bytes(CMD_DATA_READY))?;
        sleep(Duration::from_millis(20));
        let mut response = [0_u8; 3];
        self.read(&mut response)?;
        let word = decode_words::<1>(&response)?[0];
        Ok(word[1] == 1)
    }

    pub fn read_measurement(&mut self, pressure_mbar: Option<f32>) -> Result<Sen66Reading> {
        self.write(&command_bytes(CMD_READ_MEASURED_VALUES))?;
        sleep(Duration::from_millis(20));

        // Nine two-byte values, each followed by its own CRC byte.
        let mut response = [0_u8; 27];
        self.read(&mut response)?;
        let words = decode_words::<9>(&response)?;

        let unsigned = |index: usize| u16::from_be_bytes(words[index]);
        let signed = |index: usize| i16::from_be_bytes(words[index]);
        let scale_u16 = |index: usize, divisor: f32| {
            let value = unsigned(index);
            (value != UNKNOWN_U16).then_some(value as f32 / divisor)
        };
        let scale_i16 = |index: usize, divisor: f32| {
            let value = signed(index);
            (value != UNKNOWN_I16).then_some(value as f32 / divisor)
        };

        Ok(Sen66Reading {
            timestamp: Utc::now(),
            pm1_ug_m3: scale_u16(0, 10.0),
            pm2_5_ug_m3: scale_u16(1, 10.0),
            pm4_ug_m3: scale_u16(2, 10.0),
            pm10_ug_m3: scale_u16(3, 10.0),
            humidity_percent: scale_i16(4, 100.0),
            temperature_c: scale_i16(5, 200.0),
            voc_index: scale_i16(6, 10.0),
            nox_index: scale_i16(7, 10.0),
            co2_ppm: scale_u16(8, 1.0),
            pressure_mbar,
        })
    }

    pub fn wait_and_read(
        &mut self,
        timeout: Duration,
        pressure_mbar: Option<f32>,
    ) -> Result<Sen66Reading> {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if self.data_ready()? {
                return self.read_measurement(pressure_mbar);
            }
            sleep(Duration::from_millis(200));
        }
        anyhow::bail!("timeout waiting for SEN66 data")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sensirion::crc8;
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

    fn encode(values: &[u16]) -> Vec<u8> {
        let mut response = Vec::new();
        for value in values {
            let word = value.to_be_bytes();
            response.extend(word);
            response.push(crc8(&word));
        }
        response
    }

    #[test]
    fn command_frames_and_pressure_validation_are_correct() {
        let mut sensor = Sen66::with_i2c(MockI2c::new(vec![]));
        sensor.start_continuous_measurement().unwrap();
        sensor.set_ambient_pressure(1013).unwrap();
        assert!(sensor.set_ambient_pressure(1201).is_err());
        assert_eq!(sensor.i2c.writes[0], [0x00, 0x21]);
        assert_eq!(&sensor.i2c.writes[1][..4], [0x67, 0x20, 0x03, 0xf5]);
    }

    #[test]
    fn measurement_scaling_and_unknown_values_are_handled() {
        let values = [
            12,
            25,
            40,
            101,
            4550_i16 as u16,
            (-1234_i16) as u16,
            1110_i16 as u16,
            UNKNOWN_I16 as u16,
            UNKNOWN_U16,
        ];
        let mut sensor = Sen66::with_i2c(MockI2c::new(vec![encode(&values)]));
        let reading = sensor.read_measurement(Some(1013.0)).unwrap();
        assert_eq!(reading.pm1_ug_m3, Some(1.2));
        assert_eq!(reading.pm2_5_ug_m3, Some(2.5));
        assert_eq!(reading.humidity_percent, Some(45.5));
        assert_eq!(reading.temperature_c, Some(-6.17));
        assert_eq!(reading.voc_index, Some(111.0));
        assert_eq!(reading.nox_index, None);
        assert_eq!(reading.co2_ppm, None);
    }

    #[test]
    fn readiness_and_crc_are_checked() {
        let mut sensor = Sen66::with_i2c(MockI2c::new(vec![encode(&[1])]));
        assert!(sensor.data_ready().unwrap());

        let mut sensor = Sen66::with_i2c(MockI2c::new(vec![vec![0, 1, 0]]));
        assert!(sensor.data_ready().is_err());
    }
}
