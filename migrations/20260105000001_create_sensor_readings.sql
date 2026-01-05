-- Create sensor_readings table for SCD30 CO2 sensor data
CREATE TABLE IF NOT EXISTS sensor_readings (
    id BIGSERIAL PRIMARY KEY,
    timestamp TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    temperature DOUBLE PRECISION NOT NULL,
    humidity DOUBLE PRECISION NOT NULL,
    co2 DOUBLE PRECISION NOT NULL,
    pressure DOUBLE PRECISION
);

-- Create index on timestamp for efficient time-range queries
CREATE INDEX idx_sensor_readings_timestamp ON sensor_readings (timestamp DESC);

-- Add comment to table
COMMENT ON TABLE sensor_readings IS 'SCD30 CO2 sensor readings from Raspberry Pi';
COMMENT ON COLUMN sensor_readings.temperature IS 'Temperature in degrees Celsius';
COMMENT ON COLUMN sensor_readings.humidity IS 'Relative humidity in percent';
COMMENT ON COLUMN sensor_readings.co2 IS 'CO2 concentration in ppm';
COMMENT ON COLUMN sensor_readings.pressure IS 'Ambient pressure in mBar used for compensation';
