use chrono::{DateTime, Utc};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SensorKind {
    Scd30,
    Sen66,
}

impl SensorKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Scd30 => "scd30",
            Self::Sen66 => "sen66",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Scd30Reading {
    pub timestamp: DateTime<Utc>,
    pub co2_ppm: f32,
    pub temperature_c: f32,
    pub humidity_percent: f32,
    pub pressure_mbar: Option<f32>,
}

#[derive(Debug, Clone)]
pub struct Sen66Reading {
    pub timestamp: DateTime<Utc>,
    pub pm1_ug_m3: Option<f32>,
    pub pm2_5_ug_m3: Option<f32>,
    pub pm4_ug_m3: Option<f32>,
    pub pm10_ug_m3: Option<f32>,
    pub humidity_percent: Option<f32>,
    pub temperature_c: Option<f32>,
    pub voc_index: Option<f32>,
    pub nox_index: Option<f32>,
    pub co2_ppm: Option<f32>,
    pub pressure_mbar: Option<f32>,
}

#[derive(Debug, Clone)]
pub enum SensorReading {
    Scd30(Scd30Reading),
    Sen66(Sen66Reading),
}

impl SensorReading {
    pub const fn kind(&self) -> SensorKind {
        match self {
            Self::Scd30(_) => SensorKind::Scd30,
            Self::Sen66(_) => SensorKind::Sen66,
        }
    }

    pub fn timestamp(&self) -> DateTime<Utc> {
        match self {
            Self::Scd30(reading) => reading.timestamp,
            Self::Sen66(reading) => reading.timestamp,
        }
    }
}
