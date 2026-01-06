-- Change all measurement columns from DOUBLE PRECISION to REAL (single precision)
-- This matches the f32 types used in the Rust application
ALTER TABLE sensor_readings 
    ALTER COLUMN temperature TYPE REAL,
    ALTER COLUMN humidity TYPE REAL,
    ALTER COLUMN co2 TYPE REAL,
    ALTER COLUMN pressure TYPE REAL;
