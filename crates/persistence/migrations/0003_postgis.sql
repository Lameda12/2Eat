-- Optional PostGIS support. Safe on databases without PostGIS: the whole block
-- is best-effort and only adds objects when the extension is usable. The
-- application-level PostgisCookLocationRepo (cargo feature `postgis`) checks
-- for the column at startup and refuses to start without it.
DO $$
DECLARE
    pg_schema TEXT;
BEGIN
    BEGIN
        CREATE EXTENSION IF NOT EXISTS postgis SCHEMA public;
    EXCEPTION WHEN OTHERS THEN
        RAISE NOTICE 'postgis not available (%); skipping cooks.location', SQLERRM;
        RETURN;
    END;

    -- Wherever PostGIS actually lives (it is database-wide, not per schema).
    SELECT n.nspname INTO pg_schema
      FROM pg_extension e JOIN pg_namespace n ON n.oid = e.extnamespace
     WHERE e.extname = 'postgis';
    IF pg_schema IS NULL THEN
        RETURN;
    END IF;

    -- Derived from lat/lng so the two can never disagree. Exact home position
    -- stays server-side; the API only ever exposes a rounded distance.
    EXECUTE format(
        'ALTER TABLE cooks ADD COLUMN IF NOT EXISTS location %1$I.geography(Point, 4326) '
        'GENERATED ALWAYS AS (%1$I.ST_SetSRID(%1$I.ST_MakePoint(lng, lat), 4326)::%1$I.geography) STORED',
        pg_schema);
    EXECUTE 'CREATE INDEX IF NOT EXISTS cooks_location_gix ON cooks USING GIST (location) WHERE is_verified';
EXCEPTION WHEN OTHERS THEN
    RAISE NOTICE 'postgis column setup skipped: %', SQLERRM;
END $$;
