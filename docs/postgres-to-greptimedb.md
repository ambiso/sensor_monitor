# Migrate legacy PostgreSQL readings to GreptimeDB

This procedure copies the old `sensor_readings` table into the new shared
`scd30_readings` table. It does not modify or delete the PostgreSQL source.

The examples use:

```sh
export LEGACY_DATABASE_URL='postgresql://user:password@postgres-host/sensor_db'
export GREPTIMEDB_URL='postgresql://user:password@greptime-host:4003/public'
export LEGACY_NODE_ID='living-room'
export MIGRATION_FILE='/tmp/scd30-readings.csv'
```

Use the same stable node ID configured in the daemon. Run the migration while
the old writer is stopped, or record a cutoff time and export only rows at or
before that time.

## 1. Create the target table

Connect to GreptimeDB through its PostgreSQL-compatible endpoint:

```sh
psql "$GREPTIMEDB_URL"
```

Create the table:

```sql
CREATE TABLE IF NOT EXISTS scd30_readings (
    node_id STRING NOT NULL,
    recorded_at TIMESTAMP(3) NOT NULL TIME INDEX,
    temperature FLOAT32 NOT NULL,
    humidity FLOAT32 NOT NULL,
    co2 FLOAT32 NOT NULL,
    pressure FLOAT32,
    PRIMARY KEY (node_id)
);
```

## 2. Export PostgreSQL to CSV

The export drops the legacy synthetic `id`, renames `timestamp` to
`recorded_at`, and adds the node ID required by the new time-series schema:

```sh
psql "$LEGACY_DATABASE_URL" \
  --set=legacy_node_id="$LEGACY_NODE_ID" \
  --command="COPY (
    SELECT
      :'legacy_node_id' AS node_id,
      timestamp AS recorded_at,
      temperature,
      humidity,
      co2,
      pressure
    FROM sensor_readings
    ORDER BY timestamp
  ) TO STDOUT WITH (FORMAT csv, HEADER true)" \
  > "$MIGRATION_FILE"
```

Record the source checks before importing:

```sh
psql "$LEGACY_DATABASE_URL" --command="
  SELECT count(*) AS rows, min(timestamp), max(timestamp)
  FROM sensor_readings;"
```

## 3. Make the CSV available to GreptimeDB

GreptimeDB's `COPY FROM` reads from the GreptimeDB server filesystem, not the
client filesystem. Copy the file to the database host or use an object-storage
location supported by your GreptimeDB deployment. For a standalone host:

```sh
scp "$MIGRATION_FILE" greptime-host:/tmp/scd30-readings.csv
```

Then run:

```sh
psql "$GREPTIMEDB_URL" --command="
  COPY scd30_readings
  FROM '/tmp/scd30-readings.csv'
  WITH (FORMAT = 'csv', HEADERS = 'true');"
```

## 4. Validate

```sh
psql "$GREPTIMEDB_URL" --set=legacy_node_id="$LEGACY_NODE_ID" --command="
  SELECT count(*) AS rows, min(recorded_at), max(recorded_at)
  FROM scd30_readings
  WHERE node_id = :'legacy_node_id';"
```

Compare the count and time range with the PostgreSQL result. Spot-check several
rows, especially rows where `pressure` is `NULL`:

```sh
psql "$GREPTIMEDB_URL" --set=legacy_node_id="$LEGACY_NODE_ID" --command="
  SELECT *
  FROM scd30_readings
  WHERE node_id = :'legacy_node_id'
  ORDER BY recorded_at DESC
  LIMIT 10;"
```

Keep the PostgreSQL database until the GreptimeDB counts and sampled values
have been verified and the new daemon has been running successfully.
