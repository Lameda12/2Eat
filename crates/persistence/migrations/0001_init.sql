-- 2Eat schema. Money is integer SGD cents. Lat/lng are plain doubles with a
-- bounding-box index; swap for a PostGIS geography column + GiST index later.

CREATE TABLE cooks (
    id                          UUID PRIMARY KEY,
    display_name                TEXT NOT NULL CHECK (char_length(display_name) BETWEEN 1 AND 80),
    food_safety_cert_expires_at TIMESTAMPTZ,
    is_verified                 BOOLEAN NOT NULL DEFAULT FALSE,
    lat                         DOUBLE PRECISION NOT NULL CHECK (lat BETWEEN -90 AND 90),
    lng                         DOUBLE PRECISION NOT NULL CHECK (lng BETWEEN -180 AND 180),
    hbfb_terms_accepted_at      TIMESTAMPTZ NOT NULL,
    created_at                  TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- A verified cook always has a certificate expiry on record.
    CONSTRAINT verified_requires_cert CHECK (NOT is_verified OR food_safety_cert_expires_at IS NOT NULL)
);
CREATE INDEX cooks_discovery_idx ON cooks (lat, lng) WHERE is_verified;

CREATE TABLE certification_submissions (
    id           UUID PRIMARY KEY,
    cook_id      UUID NOT NULL REFERENCES cooks (id),
    document_ref TEXT NOT NULL,
    content_type TEXT NOT NULL,
    status       TEXT NOT NULL CHECK (status IN ('pending', 'approved', 'rejected')),
    submitted_at TIMESTAMPTZ NOT NULL,
    reviewed_at  TIMESTAMPTZ
);
CREATE INDEX certification_submissions_cook_idx ON certification_submissions (cook_id);

CREATE TABLE dishes (
    id             UUID PRIMARY KEY,
    cook_id        UUID NOT NULL REFERENCES cooks (id),
    name           TEXT NOT NULL,
    description    TEXT NOT NULL DEFAULT '',
    price_cents    BIGINT NOT NULL CHECK (price_cents > 0),
    is_catering    BOOLEAN NOT NULL DEFAULT FALSE,
    is_raw_seafood BOOLEAN NOT NULL DEFAULT FALSE,
    is_active      BOOLEAN NOT NULL DEFAULT TRUE,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- Belt and braces: the domain already refuses these; the database does too.
    CONSTRAINT no_catering CHECK (NOT is_catering),
    CONSTRAINT no_raw_seafood CHECK (NOT is_raw_seafood)
);
CREATE INDEX dishes_cook_idx ON dishes (cook_id);

CREATE TABLE orders (
    id                  UUID PRIMARY KEY,
    eater_id            UUID NOT NULL,
    cook_id             UUID NOT NULL REFERENCES cooks (id),
    fulfillment         TEXT NOT NULL CHECK (fulfillment IN ('pickup', 'delivery')),
    delivery_address    TEXT,
    delivery_postal     TEXT,
    subtotal_cents      BIGINT NOT NULL CHECK (subtotal_cents > 0),
    platform_fee_cents  BIGINT NOT NULL CHECK (platform_fee_cents >= 0),
    delivery_fee_cents  BIGINT NOT NULL CHECK (delivery_fee_cents >= 0),
    total_cents         BIGINT NOT NULL,
    status              TEXT NOT NULL CHECK (status IN
        ('placed', 'accepted_by_cook', 'ready_for_pickup', 'out_for_delivery', 'completed', 'cancelled', 'refunded')),
    payment_ref         TEXT,
    refund_amount_cents BIGINT,
    refund_reason       TEXT,
    refunded_at         TIMESTAMPTZ,
    placed_at           TIMESTAMPTZ NOT NULL,
    updated_at          TIMESTAMPTZ NOT NULL,
    CONSTRAINT totals_add_up CHECK (total_cents = subtotal_cents + platform_fee_cents + delivery_fee_cents),
    CONSTRAINT delivery_has_address CHECK (fulfillment <> 'delivery' OR (delivery_address IS NOT NULL AND delivery_postal IS NOT NULL)),
    -- Every refund is a full refund, platform fee included.
    CONSTRAINT refund_is_full CHECK (refund_amount_cents IS NULL OR refund_amount_cents = total_cents),
    CONSTRAINT refunded_has_refund CHECK ((status = 'refunded') = (refund_amount_cents IS NOT NULL))
);
CREATE INDEX orders_cook_idx ON orders (cook_id);
CREATE INDEX orders_eater_idx ON orders (eater_id);

CREATE TABLE order_items (
    order_id         UUID NOT NULL REFERENCES orders (id) ON DELETE CASCADE,
    position         INT NOT NULL,
    dish_id          UUID NOT NULL REFERENCES dishes (id),
    dish_name        TEXT NOT NULL,
    unit_price_cents BIGINT NOT NULL CHECK (unit_price_cents > 0),
    quantity         INT NOT NULL CHECK (quantity > 0),
    PRIMARY KEY (order_id, position)
);
