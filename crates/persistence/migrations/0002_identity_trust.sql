-- Phase 2: identity, admin review, audit log, refund governance.

-- Extensions live in `public` (database-wide), never in a tenant/test schema.
CREATE EXTENSION IF NOT EXISTS citext SCHEMA public;

-- ---------------------------------------------------------------- identity
CREATE TABLE users (
    id            UUID PRIMARY KEY,
    email         CITEXT NOT NULL UNIQUE,
    password_hash TEXT NOT NULL,           -- Argon2id PHC string
    role          TEXT NOT NULL CHECK (role IN ('eater', 'cook', 'admin')),
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE sessions (
    id         UUID PRIMARY KEY,
    user_id    UUID NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    -- SHA-256 of the opaque bearer token. The token itself is never stored.
    token_hash BYTEA NOT NULL UNIQUE CHECK (octet_length(token_hash) = 32),
    expires_at TIMESTAMPTZ NOT NULL,
    revoked_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX sessions_user_idx ON sessions (user_id);

-- ------------------------------------------------------ certificate reviews
ALTER TABLE certification_submissions RENAME TO certificate_reviews;
ALTER TABLE certificate_reviews
    ADD COLUMN reviewer_id      UUID REFERENCES users (id),
    ADD COLUMN rejection_reason TEXT CHECK (rejection_reason IN
        ('illegible_document', 'expired_certificate', 'wrong_certificate_type', 'name_mismatch', 'suspected_fraud', 'other')),
    ADD COLUMN rejection_detail TEXT,   -- free text, only for rejection_reason = 'other'
    ADD COLUMN notes            TEXT;
ALTER TABLE certificate_reviews
    ADD CONSTRAINT rejected_has_reason CHECK (status <> 'rejected' OR rejection_reason IS NOT NULL) NOT VALID;
ALTER INDEX certification_submissions_cook_idx RENAME TO certificate_reviews_cook_idx;
CREATE INDEX certificate_reviews_status_idx ON certificate_reviews (status, submitted_at);

-- --------------------------------------------------------------- audit log
CREATE TABLE audit_log (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    -- Insertion order. created_at comes from the caller's clock and can tie within one request.
    seq         BIGINT GENERATED ALWAYS AS IDENTITY,
    actor_id    UUID REFERENCES users (id),   -- NULL = system
    action      TEXT NOT NULL CHECK (char_length(action) BETWEEN 1 AND 100),
    target_type TEXT NOT NULL,
    target_id   UUID NOT NULL,
    metadata    JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX audit_log_target_idx ON audit_log (target_type, target_id, seq);
CREATE INDEX audit_log_actor_idx ON audit_log (actor_id, created_at);

CREATE FUNCTION audit_log_is_append_only() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION 'audit_log is append-only' USING ERRCODE = 'integrity_constraint_violation';
END $$;
CREATE TRIGGER audit_log_no_update_delete BEFORE UPDATE OR DELETE ON audit_log
    FOR EACH ROW EXECUTE FUNCTION audit_log_is_append_only();
CREATE TRIGGER audit_log_no_truncate BEFORE TRUNCATE ON audit_log
    FOR EACH STATEMENT EXECUTE FUNCTION audit_log_is_append_only();

-- ----------------------------------------------------------------- refunds
-- One refund request per order, ever: an order can never be double-counted.
CREATE TABLE refunds (
    id           UUID PRIMARY KEY,
    order_id     UUID NOT NULL UNIQUE REFERENCES orders (id),
    initiated_by UUID NOT NULL,
    reason       TEXT NOT NULL,
    amount_cents BIGINT NOT NULL CHECK (amount_cents > 0),
    settlement   TEXT NOT NULL CHECK (settlement IN ('refund', 'void')),
    -- NULL while pending admin review.
    decision     TEXT CHECK (decision IN ('auto', 'admin_approved', 'admin_denied')),
    decided_by   UUID REFERENCES users (id),
    decided_at   TIMESTAMPTZ,
    created_at   TIMESTAMPTZ NOT NULL,
    CONSTRAINT admin_decisions_have_decider CHECK (decision IS DISTINCT FROM 'admin_denied' OR decided_by IS NOT NULL)
);
CREATE INDEX refunds_initiator_idx ON refunds (initiated_by, created_at);
CREATE INDEX refunds_pending_idx ON refunds (created_at) WHERE decision IS NULL;

-- ------------------------------------------------------------ dish review
ALTER TABLE dishes
    ADD COLUMN review_state TEXT NOT NULL DEFAULT 'clear' CHECK (review_state IN ('clear', 'pending_review', 'approved', 'rejected')),
    ADD COLUMN review_flag  TEXT;
CREATE INDEX dishes_pending_review_idx ON dishes (created_at) WHERE review_state = 'pending_review';

-- ---------------------------------------------- accepted orders need approval
-- Backstop for the application check: no order may ENTER the accepted state
-- unless the cook is verified and their most recent decided certificate review
-- is 'approved'. (Cooks with no decided review, or a rejected one, cannot sell.)
CREATE FUNCTION enforce_approved_cook_on_accept() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    latest TEXT;
BEGIN
    IF NEW.status = 'accepted_by_cook' AND (TG_OP = 'INSERT' OR OLD.status IS DISTINCT FROM 'accepted_by_cook') THEN
        SELECT status INTO latest FROM certificate_reviews
         WHERE cook_id = NEW.cook_id AND status <> 'pending'
         ORDER BY reviewed_at DESC NULLS LAST, submitted_at DESC LIMIT 1;
        IF latest IS DISTINCT FROM 'approved'
           OR NOT EXISTS (SELECT 1 FROM cooks WHERE id = NEW.cook_id AND is_verified) THEN
            RAISE EXCEPTION 'cook % has no approved certificate review', NEW.cook_id USING ERRCODE = 'check_violation';
        END IF;
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER orders_accept_requires_approved_cook BEFORE INSERT OR UPDATE ON orders
    FOR EACH ROW EXECUTE FUNCTION enforce_approved_cook_on_accept();
