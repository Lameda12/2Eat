# 2Eat

Community food marketplace for **home-based food businesses (HBFBs) in Singapore**. Eaters order from nearby home cooks for **pickup or cook-delivered delivery**.

The platform does not tell cooks they need an SFA licence (HBFBs don't). It enforces what actually applies:

| Rule | Where it is enforced |
|---|---|
| Food Safety Course Level 1 (WSQ) certificate verified and unexpired before a cook can list dishes or accept orders | `Cook::can_sell`, checked in `Dish::create`, `Order::place`, `Order::accept`; **and** by the `Store` (an order cannot enter `AcceptedByCook` unless the cook's latest decided certificate review is `Approved`), **and** by a DB trigger |
| No catering, no sale to food retailers, no ready-to-eat raw seafood | `Dish::create` / `Order::place` return `DomainError::RegulatoryViolation`; DB `CHECK`s as a backstop |
| No external signage, no non-household employees (not enforceable in code) | Cook onboarding: `GET /cooks/onboarding-terms`; registration requires `accept_hbfb_terms=true` |
| Every order has a full refund incl. platform and delivery fee | `Order::refund` always refunds `total_cents`; DB `CHECK (refund = total)` |

## Layout

```
crates/
  domain/       pure rules: Cook, Dish, Order state machine, RefundPolicy, Role, DomainError (no I/O; time is injected)
  geo/          haversine, bounding box, NearbyQuery
  payments/     PaymentGateway trait, MockPaymentGateway, StripeConnectGateway (stub)
  persistence/  Store, AuthStore, CookLocationRepo traits; PgStore + MemoryStore; migrations/
  api/          Axum server + OpenAPI (GET /openapi.json)
```

## Run

```sh
# in-memory store, mock payments (data lost on exit):
cargo run -p twoeat-api

# PostgreSQL (migrations run on start); create the first admin once:
export DATABASE_URL=postgres://user:pass@localhost/twoeat
printf '%s\n' "$ADMIN_PASSWORD" | cargo run -p twoeat-api -- create-admin you@example.com
cargo run -p twoeat-api
```

Env: `DATABASE_URL`, `BIND_ADDR` (`0.0.0.0:3000`), `UPLOAD_DIR` (`uploads`), `PLATFORM_FEE_BPS` (500), `DELIVERY_FEE_CENTS` (500), `MAX_PORTIONS_PER_ORDER` (20), `TRUST_FORWARDED_FOR` (`1` only behind a proxy you control: uses the right-most `X-Forwarded-For` entry for rate limiting).

### Cargo features (`twoeat-api`)

| Feature | Effect |
|---|---|
| `postgis` | Discovery runs on PostGIS (`PostgisCookLocationRepo`). Refuses to start if PostGIS or `cooks.location` is missing. Default: bounding box + haversine. |
| `dev-auth` | **Dev only.** Accepts `x-user-id` (and `x-user-role`) headers as identity so the Phase 1 test suite and curl demos work without tokens. **The server refuses to start if this is compiled into a release build** (`try_router` returns an error; `router` panics). Never set it in production. |
| `dev-email` | Logs the (stubbed) verification email. Dev only; addresses are personal data. |

## Authentication

* **Accounts**: `users(id, email citext unique, password_hash, role eater|cook|admin, created_at)`. Passwords are hashed with **Argon2id using the `argon2` crate's default parameters**, stored as PHC strings via `password-hash`. Hashing runs on the blocking pool.
* **Tokens**: **opaque bearer tokens**, not JWTs. 32 bytes from `OsRng`, base64url (43 chars). Only the **SHA-256 digest** is stored (`sessions.token_hash`); the token exists in memory once and in the one response that returns it. Sessions last **30 days**, can be revoked, and are checked against `revoked_at`/`expires_at` on every request.
* **Endpoints**: `POST /auth/register`, `/auth/login`, `/auth/logout`, `/auth/refresh`. Refresh revokes the presented token and issues a new one **in one transaction**; two concurrent refreshes cannot both succeed.
* **Middleware**: `Authorization: Bearer <token>` becomes a `CurrentUser { id, role }` request extension. Protected handlers take `Extension<CurrentUser>`. Unknown, expired or revoked tokens are simply anonymous and get `401` + `WWW-Authenticate: Bearer` on protected routes; non-admins get `403` on `/admin/*`.
* **Roles**: self-registration creates `eater` or `cook` accounts only; admins are created with `twoeat-api create-admin <email>` (password read from stdin, never argv/env). A cook's user id **is** their cook id; `POST /cooks/register` needs a `cook` account.
* **No account enumeration**: login returns the same `401 invalid_credentials` for an unknown email, a wrong password and an oversized password (and does the same Argon2 work in all three cases). `POST /auth/register` returns the same `202` whether or not the email already exists (an existing account is left untouched). Duplicate emails are not reported.
* **Rate limits** (per client IP *and* per normalised email, `429` + `Retry-After`): login 20/min per IP and 5/min per email; register 5/min per IP and 3/min per email. Implemented with `governor` directly (the library `tower-governor` is built on) because the per-email limit needs the parsed body. Defaults live in `RateLimitConfig`.
* **Secrets hygiene**: types holding a password hash or token digest/plaintext (`NewUser`, `UserRecord`, `NewSession`, `BearerToken`, `TokenResponse`, `RegisterRequest`, `LoginRequest`) derive `Zeroize, ZeroizeOnDrop` and have redacting `Debug`. A test captures all `tracing` output at TRACE level across register/login/refresh/logout and asserts no token, password or hash appears.
* **Email**: sending is stubbed. `Mailer::send_verification` is called on registration; with `dev-email` it logs, otherwise it is a no-op. Verification is **not enforced yet**.

## Admin flow

Admins authenticate like everyone else (the old `x-admin-token` is gone).

1. A cook registers (`POST /cooks/register`, certificate PDF/PNG/JPEG upload) → a `certificate_reviews` row `pending`. The cook cannot list dishes, appear in discovery or accept orders.
2. `GET /admin/reviews?status=pending&limit=&offset=` lists the queue (oldest first, paginated).
3. `POST /admin/reviews/{id}/approve` `{ "expires_at": "...", "notes": "..." }` or `.../reject` `{ "reason": ..., "notes": "..." }`. Reasons: `illegible_document`, `expired_certificate`, `wrong_certificate_type`, `name_mismatch`, `suspected_fraud`, or `{"other": "free text"}`. Decisions are final (`409` if already decided).
4. A cook with a `Rejected` latest review, or no decided review at all, cannot sell. This is enforced in the domain **and** again in the `Store` and by a Postgres trigger on `orders`, so a bug or a manual edit of `cooks.is_verified` cannot get an order accepted.
5. Cook-keyed shortcuts `/admin/cooks/{id}/certification[/approve|/reject]` and `POST /admin/cooks/{id}/revoke` remain.

**Audit log** (`audit_log`, append-only: triggers reject `UPDATE`/`DELETE`/`TRUNCATE`): written for every certificate decision, cook revocation, flagged-dish decision, refund request, refund decision, refund settlement and cook flag. Each entry is written **in the same transaction** as the mutation (audit first, mutation after), so a failure rolls both back (tested with an injected failure). Read with `GET /admin/audit?target_type=&target_id=&action=`.

## Refund policy

`POST /orders/{id}/refund` (the order's eater, its cook, or an admin) returns `{ "status": "approved" | "pending_review", "refund_id": "..." }`.

* **Eater requests** are auto-approved if the eater has **fewer than 3** approved refunds in the trailing **30 days**; otherwise `202 pending_review` and **no money moves** until an admin calls `POST /admin/refunds/{id}/approve` or `/deny`. So refunds #1 to #3 are automatic and #4 goes to review.
* **Cook- and admin-initiated** refunds are auto-approved (they are not the eater-abuse case).
* **One refund request per order, ever** (unique constraint). Asking again returns the same refund and retries a failed payout; it can never be counted twice. Cancelling an unaccepted order (nothing captured) is a voided hold and is **not** counted.
* **Cook signal**: when a cook has **more than 5** approved refunds against them in the trailing 30 days, a `cook.flagged_for_review` audit entry is written. The cook is **not** auto-suspended.
* **Full amount, always**: an approved refund, whether automatic or `pending_review → AdminApproved`, returns `items + delivery fee + platform fee` (the order's `total_cents`). Tested on both paths, including the amount the payment provider is asked for.
* The thresholds are **platform policy, not law** and live in one place, `domain::refund::policy`. The policy is a swappable trait, `RefundPolicy`; the store supplies the window's counts inside the same transaction (serialised per eater with an advisory lock) so two concurrent requests cannot both slip under the limit.

## Delivery

Delivery is **performed by the cook**: `DeliveryDetails.delivered_by: DeliveredBy::Cook` (the enum has no other variant). **2Eat does not dispatch third-party couriers in Singapore**, and has no courier model, routing or tracking. The delivery fee is a flat platform setting.

## Dish review and the raw-seafood keyword list

* A dish **declared** `is_raw_seafood: true` is a hard `RegulatoryViolation` (`422 raw_seafood_prohibited`).
* A dish whose name/description merely **matches a keyword** is created **inactive** with `review_status: pending_review`; it is not listed or orderable until an admin approves it (`GET /admin/dishes/flagged`, `POST /admin/dishes/{id}/approve|reject`).
* The keyword list (`domain::dish::RAW_SEAFOOD_REVIEW_TERMS`) is **non-exhaustive**: `sashimi`, `ceviche`, `crudo`, `carpaccio`, `tartare`, `poke`, `oysters`, `raw oyster`, `raw fish`, `raw salmon`, `raw tuna`, `raw prawn`, `raw shrimp`, `raw scallop`, `raw seafood`. It is a tripwire for common cases, not a legal test; a dish that matches nothing is not thereby permitted. Matching is whole-word and case-insensitive ("spoken" and "oyster sauce" do not match).

## Discovery and PostGIS

`CookLocationRepo::find_nearby(center, radius_km, limit, now)` has three implementations: in-memory, Postgres bounding box + haversine (default), and PostGIS (`--features postgis`, a GiST-indexed `cooks.location geography(Point,4326)` generated from `lat`/`lng`). The PostGIS migration is best-effort: on a database without PostGIS it is skipped, and the `postgis` build then refuses to start rather than degrade.

PostGIS uses the **spherical** model (`use_spheroid = false`) so it agrees with haversine instead of differing by up to ~0.5 % from the WGS84 spheroid. A parity test runs the same 7 queries over 150 deterministic cooks (including a just-inside/just-outside pair at a radius edge, expired and rejected cooks) against all implementations and compares **distance-ordered ids**, with distances within 0.5 m.

Privacy is unchanged: the repo returns exact metres internally, the API returns distance **rounded up to the nearest 100 m**, and neither ever returns coordinates (asserted over HTTP against both Postgres implementations).

## API

OpenAPI 3 at `GET /openapi.json` (security scheme `bearer_auth`).

| Area | Endpoints |
|---|---|
| Auth | `POST /auth/register`, `/auth/login`, `/auth/logout`, `/auth/refresh` |
| Cooks | `GET /cooks/onboarding-terms`, `POST /cooks/register` (multipart), `GET /cooks/nearby`, `GET /cooks/{id}/dishes` |
| Dishes | `POST /dishes` |
| Orders | `POST /orders`, `GET /orders/{id}`, `POST /orders/{id}/accept\|ready\|complete\|cancel\|refund` |
| Admin | `GET /admin/reviews`, `POST /admin/reviews/{id}/approve\|reject`, `GET /admin/refunds`, `POST /admin/refunds/{id}/approve\|deny`, `GET /admin/dishes/flagged`, `POST /admin/dishes/{id}/approve\|reject`, `GET /admin/audit`, cook-keyed shortcuts |

Errors: `{"error": {"code", "type", "message"}}`. Regulatory failures are `422` with `type: "regulatory_violation"` and a stable code.

## Order lifecycle

```
Placed ──accept──▶ AcceptedByCook ──ready──▶ ReadyForPickup (pickup) ──▶ Completed
  │                     │                └──▶ OutForDelivery (delivery, by the cook) ──▶ Completed
  └─cancel─▶ Cancelled  └──────── refund (any time after accept) ────────▶ Refunded
```

Money: authorize at placement, capture on acceptance. `Cancelled` = hold voided; `Refunded` = captured funds returned in full.

## Concurrency

`Store::transition_order` runs in one transaction: `SELECT … FOR SHARE` on the cook, then `FOR UPDATE` on the order, then the domain mutation. Revoking, approving or rejecting a cook takes the cook row exclusively, so an acceptance can never slip past an in-flight revocation (`pg_accept_waits_for_in_flight_revocation_then_fails`).

## Testing

```sh
# Postgres-backed tests run only when DATABASE_URL is set (each uses a throwaway schema).
# The database needs the citext extension, and PostGIS for the postgis feature
# (e.g. `apt install postgresql-16-postgis-3`).
export DATABASE_URL=postgres://user:pass@localhost/twoeat

cargo test --workspace                         # real auth only
cargo test --workspace --features postgis      # + PostGIS parity
cargo test --workspace --all-features          # + dev-auth Phase 1 suite, dev-email
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

`crates/api/tests/api.rs` is the Phase 1 end-to-end suite; it authenticates with `x-user-id` so it only builds with `--features dev-auth`.

## Known gaps

* No email verification (the mailer is a stub), no password reset, no "log out everywhere".
* No admin UI or document viewer: reviewers get `document_ref` but no endpoint to fetch the file.
* `StripeConnectGateway` is a stub. Only the mock moves "money".
* No GST handling. Delivery is a flat fee.
* The refund thresholds, `MAX_PORTIONS_PER_ORDER`, the keyword list and the default fee are platform policy/heuristics, not statutory figures. Onboarding and terms wording should get legal review before launch.
