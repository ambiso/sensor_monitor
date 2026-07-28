# Sensor Monitor

`sensor_monitor` collects air-quality measurements from SCD30 and SEN66 sensors
on a Raspberry Pi. It automatically detects either or both sensors on
`/dev/i2c-1`, writes readings to GreptimeDB, and keeps a durable local queue
while the database is unavailable.

## Configuration

Copy `env.example` to the environment file used by the systemd unit and set:

```text
GREPTIMEDB_URL=postgresql://user:password@host:4003/public
NODE_ID=living-room
```

`NODE_ID` identifies the Pi in shared `scd30_readings` and `sen66_readings`
tables. It must remain stable after deployment.

The local queue defaults to `/var/lib/sensor_monitor/spool.db`. Set
`SPOOL_PATH` only when running outside the supplied systemd unit or when a
different persistent filesystem is required.

## Sampling intervals

Both sensors default to a 10-second storage interval:

```console
sensor_monitor --interval 10
```

Use model-specific overrides only when needed:

```console
sensor_monitor --interval 10 --scd30-interval 30 --sen66-interval 5
```

SCD30 accepts 2–1800 seconds. SEN66 measures internally once per second and can
be stored at an interval of 1–1800 seconds.

`--pressure-offset` adjusts the Vienna ambient-pressure value before it is
applied to connected CO₂ sensors.

## Mock sensors

Use `--mock-sensors` to exercise the complete storage pipeline without I²C
hardware:

```console
GREPTIMEDB_URL=postgresql://greptime:greptime@127.0.0.1:4003/public \
NODE_ID=host-test \
SPOOL_PATH=/tmp/sensor-monitor-spool.db \
sensor_monitor --mock-sensors --scd30-interval 2 --sen66-interval 1
```

Mock mode emits deterministic readings for both sensor models, uses 1013 mBar
for pressure compensation, and does not open the I²C bus or fetch ambient
pressure.

## Runtime behavior

- Missing sensors are reprobed every 30 seconds.
- A sensor is taken offline after three consecutive I²C failures and is
  reinitialized automatically when it returns.
- The other sensor continues running independently.
- Failed GreptimeDB writes are stored in SQLite and replayed oldest-first.
- The queue is bounded at approximately 1 GiB. If it fills, the oldest samples
  are evicted and an error is written to the journal.

For moving existing SCD30 rows from PostgreSQL, see
[docs/postgres-to-greptimedb.md](docs/postgres-to-greptimedb.md).
