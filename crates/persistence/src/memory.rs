use std::collections::HashMap;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::json;
use tokio::sync::Mutex;
use twoeat_domain::{
    CertDecision, CertificationStatus, CertificationSubmission, Cook, Dish, DishReview, LatLng, Order, OrderStatus, RefundDecision,
    RefundHistory, RefundInitiator, RefundPolicy, RefundRecord, RefundSettlement, RefundStatus, RegulatoryViolation,
};
use twoeat_geo::{NearbyQuery, rank_nearby};
use uuid::Uuid;

use crate::{
    AuditEntry, AuditFilter, AuditRecord, AuthStore, CookLocationRepo, NearbyCook, NewRefund, NewSession, NewUser, OrderMutation, Page,
    RefundOutcome, SessionRecord, Store, StoreError, StoreResult, UserRecord, location::query_of,
};

struct MemSession {
    id: Uuid,
    user_id: Uuid,
    token_hash: [u8; 32],
    expires_at: DateTime<Utc>,
    revoked_at: Option<DateTime<Utc>>,
}

#[derive(Default)]
struct Inner {
    cooks: HashMap<Uuid, Cook>,
    reviews: Vec<CertificationSubmission>,
    dishes: HashMap<Uuid, Dish>,
    orders: HashMap<Uuid, Order>,
    refunds: Vec<RefundRecord>,
    audit: Vec<AuditRecord>,
    users: HashMap<Uuid, UserRecord>,
    emails: HashMap<String, Uuid>,
    sessions: Vec<MemSession>,
}

impl Inner {
    fn audit(&mut self, e: AuditEntry) {
        self.audit.push(AuditRecord { id: Uuid::new_v4(), entry: e });
    }

    /// Latest *decided* review is Approved (no review / pending only / rejected => false).
    fn has_approved_review(&self, cook_id: Uuid) -> bool {
        self.reviews
            .iter()
            .filter(|r| r.cook_id == cook_id && r.status != CertificationStatus::Pending)
            .max_by_key(|r| (r.reviewed_at, r.submitted_at))
            .is_some_and(|r| r.status == CertificationStatus::Approved)
    }

    fn eater_refunds_since(&self, eater: Uuid, since: DateTime<Utc>, except_order: Uuid) -> u32 {
        self.refunds
            .iter()
            .filter(|r| r.initiated_by == eater && r.order_id != except_order && r.settlement == RefundSettlement::Refund)
            .filter(|r| r.status() == RefundStatus::Approved && r.created_at > since)
            .count() as u32
    }

    fn cook_refunds_since(&self, cook: Uuid, since: DateTime<Utc>) -> u32 {
        self.refunds
            .iter()
            .filter(|r| r.settlement == RefundSettlement::Refund && r.status() == RefundStatus::Approved && r.created_at > since)
            .filter(|r| self.orders.get(&r.order_id).is_some_and(|o| o.cook_id == cook))
            .count() as u32
    }

    /// Write `cook.flagged_for_review` when the policy says the cook has too many refunds.
    fn maybe_flag_cook(&mut self, cook_id: Uuid, policy: &dyn RefundPolicy, now: DateTime<Utc>, trigger: Uuid) {
        let count = self.cook_refunds_since(cook_id, now - policy.window());
        let h = RefundHistory { eater_refunds_in_window: 0, cook_refunds_in_window: count };
        if policy.should_flag_cook(&h, cook_id, now) {
            self.audit(AuditEntry::new(
                None,
                "cook.flagged_for_review",
                "cook",
                cook_id,
                json!({"refunds_in_window": count, "window_days": policy.window().num_days(), "triggered_by_refund": trigger}),
                now,
            ));
        }
    }
}

/// Single-mutex in-memory store: every operation is atomic, mirroring the
/// serialisable outcome the Postgres adapter gets from row locks.
#[derive(Default)]
pub struct MemoryStore {
    inner: Mutex<Inner>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }
}

fn page<T: Clone>(all: Vec<T>, limit: i64, offset: i64) -> Page<T> {
    let total = all.len() as i64;
    let items = all.into_iter().skip(offset.max(0) as usize).take(limit.max(0) as usize).collect();
    Page { items, total }
}

#[async_trait]
impl Store for MemoryStore {
    async fn register_cook(&self, cook: &Cook, submission: &CertificationSubmission, _terms_accepted_at: DateTime<Utc>) -> StoreResult<()> {
        let mut g = self.inner.lock().await;
        if g.cooks.contains_key(&cook.id) {
            return Err(StoreError::Conflict("cook profile already exists"));
        }
        g.cooks.insert(cook.id, cook.clone());
        g.reviews.push(submission.clone());
        Ok(())
    }

    async fn get_cook(&self, id: Uuid) -> StoreResult<Cook> {
        self.inner.lock().await.cooks.get(&id).cloned().ok_or(StoreError::NotFound("cook"))
    }

    async fn latest_submission(&self, cook_id: Uuid) -> StoreResult<Option<CertificationSubmission>> {
        let g = self.inner.lock().await;
        Ok(g.reviews.iter().filter(|s| s.cook_id == cook_id).max_by_key(|s| s.submitted_at).cloned())
    }

    async fn get_review(&self, id: Uuid) -> StoreResult<CertificationSubmission> {
        self.inner.lock().await.reviews.iter().find(|r| r.id == id).cloned().ok_or(StoreError::NotFound("certificate review"))
    }

    async fn list_reviews(
        &self,
        status: Option<CertificationStatus>,
        limit: i64,
        offset: i64,
    ) -> StoreResult<Page<CertificationSubmission>> {
        let g = self.inner.lock().await;
        let mut v: Vec<_> = g.reviews.iter().filter(|r| status.is_none_or(|s| r.status == s)).cloned().collect();
        v.sort_by_key(|r| (r.submitted_at, r.id));
        Ok(page(v, limit, offset))
    }

    async fn decide_review(
        &self,
        review_id: Uuid,
        actor: Option<Uuid>,
        decision: CertDecision,
        notes: Option<String>,
        now: DateTime<Utc>,
    ) -> StoreResult<(CertificationSubmission, Cook)> {
        let mut g = self.inner.lock().await;
        let idx = g.reviews.iter().position(|r| r.id == review_id).ok_or(StoreError::NotFound("certificate review"))?;
        let mut review = g.reviews[idx].clone();
        if review.status != CertificationStatus::Pending {
            return Err(StoreError::Conflict("certificate review already decided"));
        }
        let mut cook = g.cooks.get(&review.cook_id).cloned().ok_or(StoreError::NotFound("cook"))?;
        let metadata = match &decision {
            CertDecision::Approve { expires_at } => {
                cook.approve_certification(*expires_at, now)?;
                review.status = CertificationStatus::Approved;
                json!({"cook_id": cook.id, "expires_at": expires_at, "notes": notes})
            }
            CertDecision::Reject { reason } => {
                reason.validate()?;
                cook.revoke_verification();
                review.status = CertificationStatus::Rejected;
                review.rejection_reason = Some(reason.clone());
                let (kind, detail) = reason.to_parts();
                json!({"cook_id": cook.id, "reason": kind, "reason_detail": detail, "notes": notes})
            }
        };
        review.reviewed_at = Some(now);
        review.reviewer_id = actor;
        review.notes = notes;
        let action = if review.status == CertificationStatus::Approved { "certificate.approved" } else { "certificate.rejected" };
        g.audit(AuditEntry::new(actor, action, "certificate_review", review_id, metadata, now));
        g.cooks.insert(cook.id, cook.clone());
        g.reviews[idx] = review.clone();
        Ok((review, cook))
    }

    async fn revoke_cook(&self, actor: Option<Uuid>, cook_id: Uuid, now: DateTime<Utc>) -> StoreResult<Cook> {
        let mut g = self.inner.lock().await;
        let cook = g.cooks.get_mut(&cook_id).ok_or(StoreError::NotFound("cook"))?;
        cook.revoke_verification();
        let cook = cook.clone();
        g.audit(AuditEntry::new(actor, "cook.revoked", "cook", cook_id, json!({}), now));
        Ok(cook)
    }

    async fn review_certification(&self, cook_id: Uuid, decision: CertDecision, now: DateTime<Utc>) -> StoreResult<Cook> {
        let review_id = {
            let g = self.inner.lock().await;
            g.reviews
                .iter()
                .filter(|r| r.cook_id == cook_id && r.status == CertificationStatus::Pending)
                .max_by_key(|r| r.submitted_at)
                .map(|r| r.id)
                .ok_or(StoreError::NotFound("pending certificate review"))?
        };
        Ok(self.decide_review(review_id, None, decision, None, now).await?.1)
    }

    async fn revoke_verification(&self, cook_id: Uuid) -> StoreResult<Cook> {
        self.revoke_cook(None, cook_id, Utc::now()).await
    }

    async fn nearby_cooks(&self, query: &NearbyQuery, now: DateTime<Utc>) -> StoreResult<Vec<NearbyCook>> {
        let g = self.inner.lock().await;
        let eligible = g.cooks.values().filter(|c| c.can_sell(now)).cloned();
        Ok(rank_nearby(eligible, query, |c| c.location)
            .into_iter()
            .map(|(c, d)| NearbyCook { id: c.id, display_name: c.display_name, distance_m: d })
            .collect())
    }

    async fn insert_dish(&self, dish: &Dish) -> StoreResult<()> {
        let mut g = self.inner.lock().await;
        if !g.cooks.contains_key(&dish.cook_id) {
            return Err(StoreError::NotFound("cook"));
        }
        g.dishes.insert(dish.id, dish.clone());
        Ok(())
    }

    async fn get_dish(&self, id: Uuid) -> StoreResult<Dish> {
        self.inner.lock().await.dishes.get(&id).cloned().ok_or(StoreError::NotFound("dish"))
    }

    async fn list_dishes(&self, cook_id: Uuid) -> StoreResult<Vec<Dish>> {
        let g = self.inner.lock().await;
        let mut v: Vec<_> = g.dishes.values().filter(|d| d.cook_id == cook_id).cloned().collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(v)
    }

    async fn list_flagged_dishes(&self, limit: i64, offset: i64) -> StoreResult<Page<Dish>> {
        let g = self.inner.lock().await;
        let mut v: Vec<_> = g.dishes.values().filter(|d| d.review == DishReview::PendingReview).cloned().collect();
        v.sort_by(|a, b| (&a.name, a.id).cmp(&(&b.name, b.id)));
        Ok(page(v, limit, offset))
    }

    async fn decide_dish_review(&self, dish_id: Uuid, actor: Uuid, approve: bool, now: DateTime<Utc>) -> StoreResult<Dish> {
        let mut g = self.inner.lock().await;
        let mut dish = g.dishes.get(&dish_id).cloned().ok_or(StoreError::NotFound("dish"))?;
        let flag = dish.review_flag.clone();
        if approve {
            dish.approve_review()?
        } else {
            dish.reject_review()?
        }
        let action = if approve { "dish.review_approved" } else { "dish.review_rejected" };
        g.audit(AuditEntry::new(Some(actor), action, "dish", dish_id, json!({"cook_id": dish.cook_id, "flag": flag}), now));
        g.dishes.insert(dish_id, dish.clone());
        Ok(dish)
    }

    async fn insert_order(&self, order: &Order) -> StoreResult<()> {
        let mut g = self.inner.lock().await;
        if !g.cooks.contains_key(&order.cook_id) {
            return Err(StoreError::NotFound("cook"));
        }
        g.orders.insert(order.id, order.clone());
        Ok(())
    }

    async fn get_order(&self, id: Uuid) -> StoreResult<Order> {
        self.inner.lock().await.orders.get(&id).cloned().ok_or(StoreError::NotFound("order"))
    }

    async fn transition_order(&self, id: Uuid, f: OrderMutation) -> StoreResult<Order> {
        let mut g = self.inner.lock().await;
        let order = g.orders.get(&id).ok_or(StoreError::NotFound("order"))?;
        let cook = g.cooks.get(&order.cook_id).ok_or(StoreError::NotFound("cook"))?.clone();
        let mut updated = order.clone();
        let before = updated.status;
        f(&cook, &mut updated)?;
        if before != OrderStatus::AcceptedByCook && updated.status == OrderStatus::AcceptedByCook && !g.has_approved_review(cook.id) {
            return Err(twoeat_domain::DomainError::from(RegulatoryViolation::CertificationNotVerified).into());
        }
        g.orders.insert(id, updated.clone());
        Ok(updated)
    }

    async fn request_refund(&self, req: NewRefund, policy: &dyn RefundPolicy) -> StoreResult<RefundOutcome> {
        let mut g = self.inner.lock().await;
        let mut order = g.orders.get(&req.order_id).cloned().ok_or(StoreError::NotFound("order"))?;
        if let Some(existing) = g.refunds.iter().find(|r| r.order_id == req.order_id) {
            return Ok(RefundOutcome { record: existing.clone(), created: false, order_cancelled: false });
        }

        let (settlement, cancel) = if order.status == OrderStatus::Placed {
            (RefundSettlement::Void, true)
        } else {
            order.check_refundable(req.reason)?;
            (RefundSettlement::Refund, false)
        };

        let since = req.now - policy.window();
        let (decision, decided_by) = match req.initiator {
            RefundInitiator::Admin => (Some(RefundDecision::AdminApproved), Some(req.initiated_by)),
            RefundInitiator::Cook => (Some(RefundDecision::Auto), None),
            RefundInitiator::Eater if settlement == RefundSettlement::Void => (Some(RefundDecision::Auto), None),
            RefundInitiator::Eater => {
                let h = RefundHistory {
                    eater_refunds_in_window: g.eater_refunds_since(order.eater_id, since, order.id),
                    cook_refunds_in_window: g.cook_refunds_since(order.cook_id, since),
                };
                (policy.should_auto_approve(&h, order.eater_id, order.cook_id, req.now).then_some(RefundDecision::Auto), None)
            }
        };

        let record = RefundRecord {
            id: Uuid::new_v4(),
            order_id: order.id,
            initiated_by: req.initiated_by,
            reason: req.reason,
            amount_cents: order.total_cents,
            settlement,
            decision,
            decided_by,
            decided_at: decision.map(|_| req.now),
            created_at: req.now,
        };
        g.audit(AuditEntry::new(
            Some(req.initiated_by),
            "refund.requested",
            "refund",
            record.id,
            json!({"order_id": order.id, "status": record.status().as_str(), "reason": req.reason.as_str(),
                   "settlement": settlement.as_str(), "amount_cents": record.amount_cents}),
            req.now,
        ));
        if cancel {
            order.cancel(req.now)?;
            g.orders.insert(order.id, order.clone());
        }
        g.refunds.push(record.clone());
        if settlement == RefundSettlement::Refund && record.status() == RefundStatus::Approved {
            g.maybe_flag_cook(order.cook_id, policy, req.now, record.id);
        }
        Ok(RefundOutcome { record, created: true, order_cancelled: cancel })
    }

    async fn get_refund(&self, id: Uuid) -> StoreResult<RefundRecord> {
        self.inner.lock().await.refunds.iter().find(|r| r.id == id).cloned().ok_or(StoreError::NotFound("refund"))
    }

    async fn get_refund_for_order(&self, order_id: Uuid) -> StoreResult<Option<RefundRecord>> {
        Ok(self.inner.lock().await.refunds.iter().find(|r| r.order_id == order_id).cloned())
    }

    async fn list_refunds(&self, status: Option<RefundStatus>, limit: i64, offset: i64) -> StoreResult<Page<RefundRecord>> {
        let g = self.inner.lock().await;
        let mut v: Vec<_> = g.refunds.iter().filter(|r| status.is_none_or(|s| r.status() == s)).cloned().collect();
        v.sort_by_key(|r| (r.created_at, r.id));
        Ok(page(v, limit, offset))
    }

    async fn decide_refund(
        &self,
        refund_id: Uuid,
        admin: Uuid,
        approve: bool,
        notes: Option<String>,
        now: DateTime<Utc>,
        policy: &dyn RefundPolicy,
    ) -> StoreResult<RefundRecord> {
        let mut g = self.inner.lock().await;
        let idx = g.refunds.iter().position(|r| r.id == refund_id).ok_or(StoreError::NotFound("refund"))?;
        if g.refunds[idx].decision.is_some() {
            return Err(StoreError::Conflict("refund already decided"));
        }
        let decision = if approve { RefundDecision::AdminApproved } else { RefundDecision::AdminDenied };
        let order_id = g.refunds[idx].order_id;
        g.audit(AuditEntry::new(
            Some(admin),
            if approve { "refund.admin_approved" } else { "refund.admin_denied" },
            "refund",
            refund_id,
            json!({"order_id": order_id, "notes": notes}),
            now,
        ));
        let r = &mut g.refunds[idx];
        r.decision = Some(decision);
        r.decided_by = Some(admin);
        r.decided_at = Some(now);
        let record = r.clone();
        if approve && let Some(o) = g.orders.get(&order_id).cloned() {
            g.maybe_flag_cook(o.cook_id, policy, now, refund_id);
        }
        Ok(record)
    }

    async fn settle_refund(&self, refund_id: Uuid, now: DateTime<Utc>) -> StoreResult<(RefundRecord, Order)> {
        let mut g = self.inner.lock().await;
        let record = g.refunds.iter().find(|r| r.id == refund_id).cloned().ok_or(StoreError::NotFound("refund"))?;
        if record.status() != RefundStatus::Approved || record.settlement != RefundSettlement::Refund {
            return Err(StoreError::Conflict("refund is not an approved payout"));
        }
        let mut order = g.orders.get(&record.order_id).cloned().ok_or(StoreError::NotFound("order"))?;
        if order.status == OrderStatus::Refunded {
            return Ok((record, order));
        }
        order.refund(record.reason, now)?;
        g.audit(AuditEntry::new(
            None,
            "refund.settled",
            "refund",
            refund_id,
            json!({"order_id": order.id, "amount_cents": order.total_cents}),
            now,
        ));
        g.orders.insert(order.id, order.clone());
        Ok((record, order))
    }

    async fn audit_entries(&self, f: AuditFilter, limit: i64) -> StoreResult<Vec<AuditRecord>> {
        let g = self.inner.lock().await;
        Ok(g.audit
            .iter()
            .filter(|a| f.target_type.as_ref().is_none_or(|t| &a.entry.target_type == t))
            .filter(|a| f.target_id.is_none_or(|t| a.entry.target_id == t))
            .filter(|a| f.action.as_ref().is_none_or(|t| &a.entry.action == t))
            .take(limit.max(0) as usize)
            .cloned()
            .collect())
    }
}

#[async_trait]
impl AuthStore for MemoryStore {
    async fn create_user(&self, user: NewUser) -> StoreResult<()> {
        let mut g = self.inner.lock().await;
        let key = user.email.to_lowercase();
        if g.emails.contains_key(&key) {
            return Err(StoreError::Conflict("email already registered"));
        }
        g.emails.insert(key, user.id);
        let rec = UserRecord {
            id: user.id,
            email: user.email.clone(),
            password_hash: user.password_hash.clone(),
            role: user.role,
            created_at: user.created_at,
        };
        g.users.insert(user.id, rec);
        Ok(())
    }

    async fn find_user_by_email(&self, email: &str) -> StoreResult<Option<UserRecord>> {
        let g = self.inner.lock().await;
        Ok(g.emails.get(&email.to_lowercase()).and_then(|id| g.users.get(id)).cloned())
    }

    async fn get_user(&self, id: Uuid) -> StoreResult<UserRecord> {
        self.inner.lock().await.users.get(&id).cloned().ok_or(StoreError::NotFound("user"))
    }

    async fn create_session(&self, s: NewSession) -> StoreResult<()> {
        let mut g = self.inner.lock().await;
        if !g.users.contains_key(&s.user_id) {
            return Err(StoreError::NotFound("user"));
        }
        g.sessions.push(MemSession { id: s.id, user_id: s.user_id, token_hash: s.token_hash, expires_at: s.expires_at, revoked_at: None });
        Ok(())
    }

    async fn find_session(&self, token_hash: &[u8; 32]) -> StoreResult<Option<SessionRecord>> {
        let g = self.inner.lock().await;
        Ok(g.sessions.iter().find(|s| &s.token_hash == token_hash).and_then(|s| {
            g.users.get(&s.user_id).map(|u| SessionRecord {
                session_id: s.id,
                user_id: s.user_id,
                role: u.role,
                expires_at: s.expires_at,
                revoked_at: s.revoked_at,
            })
        }))
    }

    async fn revoke_session(&self, session_id: Uuid, now: DateTime<Utc>) -> StoreResult<bool> {
        let mut g = self.inner.lock().await;
        match g.sessions.iter_mut().find(|s| s.id == session_id && s.revoked_at.is_none()) {
            Some(s) => {
                s.revoked_at = Some(now);
                Ok(true)
            }
            None => Ok(false),
        }
    }

    async fn rotate_session(&self, old: Uuid, new: NewSession, now: DateTime<Utc>) -> StoreResult<()> {
        let mut g = self.inner.lock().await;
        let s = g
            .sessions
            .iter_mut()
            .find(|s| s.id == old && s.revoked_at.is_none() && s.expires_at > now)
            .ok_or(StoreError::NotFound("active session"))?;
        s.revoked_at = Some(now);
        g.sessions.push(MemSession {
            id: new.id,
            user_id: new.user_id,
            token_hash: new.token_hash,
            expires_at: new.expires_at,
            revoked_at: None,
        });
        Ok(())
    }
}

#[async_trait]
impl CookLocationRepo for MemoryStore {
    async fn find_nearby(&self, center: LatLng, radius_km: f64, limit: u32, now: DateTime<Utc>) -> StoreResult<Vec<NearbyCook>> {
        let q = query_of(center, radius_km, limit)?;
        self.nearby_cooks(&q, now).await
    }
}
