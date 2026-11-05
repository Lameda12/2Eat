# 2Eat

Community food marketplace for **home-based food businesses (HBFBs) in Singapore**. Eaters order from nearby home cooks for **pickup or delivery**.

The platform does not tell cooks they need an SFA licence (HBFBs don't). It enforces what actually applies:

| Rule | Where it is enforced |
|---|---|
| Food Safety Course Level 1 (WSQ) certificate verified and unexpired before a cook can list dishes or accept orders | `Cook::can_sell` / `ensure_can_sell`, checked in `Dish::create`, `Order::place`, `Order::accept` |
| No catering, no sale to food retailers, no ready-to-eat raw seafood | `Dish::create` and `Order::place` return `DomainError::RegulatoryViolation`; DB `CHECK`s as a backstop |
| No external signage, no non-household employees (not enforceable in code) | Cook onboarding: `GET /cooks/onboarding-terms`, and registration requires `accept_hbfb_terms=true` |
| Every order has a full refund incl. platform fee | `Order::refund` always refunds `total_cents`; DB `CHECK (refund = total)` |

## Layout

```
crates/
  domain/       pure rules: Cook, Dish, Order state machine, DomainError (no I/O, time is injected)
  geo/          haversine, bounding box, NearbyQuery
  payments/     PaymentGateway trait, MockPaymentGateway, StripeConnectGateway (stub)
  persistence/  Store trait; PgStore (sqlx/PostgreSQL) and MemoryStore; migrations/
  api/          Axum server + OpenAPI (GET /openapi.json)
```

## Run

```sh
# in-memory store, mock payments:
cargo run -p twoeat-api

# with PostgreSQL (migrations run on start):
DATABASE_URL=postgres://user:pass@localhost/twoeat ADMIN_TOKEN=change-me cargo run -p twoeat-api
```

Env: `DATABASE_URL`, `ADMIN_TOKEN` (random per run if unset), `BIND_ADDR` (`0.0.0.0:3000`), `UPLOAD_DIR` (`uploads`), `PLATFORM_FEE_BPS` (500), `DELIVERY_FEE_CENTS` (500), `MAX_PORTIONS_PER_ORDER` (20).

Tests (Postgres-backed tests run only when `DATABASE_URL` is set; each uses a throwaway schema):

```sh
DATABASE_URL=postgres://user:pass@localhost/twoeat cargo test --workspace
```

## API

OpenAPI 3 document at `GET /openapi.json`. Identity is the `x-user-id` header (a cook's user id is their cook id), and admin endpoints take `x-admin-token`. This is an MVP stand-in for real auth (`crates/api/src/auth.rs` is the only place to replace).

| Method | Path | Purpose |
|---|---|---|
| GET | `/cooks/onboarding-terms` | Conditions cooks must accept |
| POST | `/cooks/register` | multipart: `display_name`, `lat`, `lng`, `accept_hbfb_terms`, `cert_document` (PDF/PNG/JPEG ≤ 5 MB) |
| GET | `/cooks/nearby?lat&lng&radius_m&limit` | Verified, currently-certified cooks only; distance rounded up to 100 m, no coordinates |
| GET | `/cooks/{id}/dishes` | Active dishes (empty unless the cook can currently sell) |
| POST | `/dishes` | Create dish; 422 on prohibited items or uncertified cook |
| POST | `/orders` | Place order (`pickup` or `delivery`); authorizes payment |
| GET | `/orders/{id}` | Eater, cook or admin |
| POST | `/orders/{id}/accept` | Cook accepts; certification re-checked under a row lock; captures payment |
| POST | `/orders/{id}/ready` | → `ready_for_pickup` / `out_for_delivery` |
| POST | `/orders/{id}/complete` | → `completed` |
| POST | `/orders/{id}/cancel` | Before acceptance; releases the hold |
| POST | `/orders/{id}/refund` | Full refund, fee included |
| POST | `/admin/cooks/{id}/certification/approve` `/reject`, `/admin/cooks/{id}/revoke`, GET `/admin/cooks/{id}/certification` | Certificate review |

Errors: `{"error": {"code", "type", "message"}}`. Regulatory failures are `422` with `type: "regulatory_violation"` and a stable code (`certification_expired`, `catering_prohibited`, `raw_seafood_prohibited`, ...).

## Order lifecycle

```
Placed ──accept──▶ AcceptedByCook ──ready──▶ ReadyForPickup (pickup) ──▶ Completed
  │                     │                └──▶ OutForDelivery (delivery) ──▶ Completed
  └─cancel─▶ Cancelled  └──────── refund (any time after accept) ────────▶ Refunded
```

Money: authorize at placement, capture on acceptance. `Cancelled` means the hold was voided (nothing charged); `Refunded` means captured funds were returned in full. A completed order is refundable only as `incorrect_order` or `not_received`.

## Concurrency

`Store::transition_order` runs in one transaction: `SELECT … FOR SHARE` on the cook, then `FOR UPDATE` on the order, then applies the domain mutation. Revoking or approving a cook takes the cook row exclusively, so an acceptance can never slip past an in-flight revocation (covered by `pg_accept_waits_for_in_flight_revocation_then_fails`).

## Known gaps (MVP)

* No real authentication; no admin UI or document viewer for certificate review.
* `StripeConnectGateway` is a stub (intended wiring is documented on the type). Only the mock moves "money".
* Delivery is a flat fee with no courier integration. No GST handling.
* Geo discovery uses a bounding box + haversine in SQL; switch to PostGIS `geography` + GiST when the extension is available.
* `MAX_PORTIONS_PER_ORDER`, the raw-seafood keyword list and the default fee are platform policy/heuristics, not statutory figures. Onboarding and terms wording should get legal review before launch.
