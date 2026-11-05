use std::collections::HashMap;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use tokio::sync::Mutex;
use twoeat_domain::{CertDecision, CertificationStatus, CertificationSubmission, Cook, Dish, Order};
use twoeat_geo::{NearbyQuery, rank_nearby};
use uuid::Uuid;

use crate::{NearbyCook, OrderMutation, Store, StoreError, StoreResult};

#[derive(Default)]
struct Inner {
    cooks: HashMap<Uuid, Cook>,
    submissions: Vec<CertificationSubmission>,
    dishes: HashMap<Uuid, Dish>,
    orders: HashMap<Uuid, Order>,
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

#[async_trait]
impl Store for MemoryStore {
    async fn register_cook(&self, cook: &Cook, submission: &CertificationSubmission, _terms_accepted_at: DateTime<Utc>) -> StoreResult<()> {
        let mut g = self.inner.lock().await;
        g.cooks.insert(cook.id, cook.clone());
        g.submissions.push(submission.clone());
        Ok(())
    }

    async fn get_cook(&self, id: Uuid) -> StoreResult<Cook> {
        self.inner.lock().await.cooks.get(&id).cloned().ok_or(StoreError::NotFound("cook"))
    }

    async fn latest_submission(&self, cook_id: Uuid) -> StoreResult<Option<CertificationSubmission>> {
        let g = self.inner.lock().await;
        Ok(g.submissions.iter().filter(|s| s.cook_id == cook_id).max_by_key(|s| s.submitted_at).cloned())
    }

    async fn review_certification(&self, cook_id: Uuid, decision: CertDecision, now: DateTime<Utc>) -> StoreResult<Cook> {
        let mut g = self.inner.lock().await;
        let cook = g.cooks.get_mut(&cook_id).ok_or(StoreError::NotFound("cook"))?;
        let status = match decision {
            CertDecision::Approve { expires_at } => {
                cook.approve_certification(expires_at, now)?;
                CertificationStatus::Approved
            }
            CertDecision::Reject => {
                cook.revoke_verification();
                CertificationStatus::Rejected
            }
        };
        let cook = cook.clone();
        if let Some(s) = g
            .submissions
            .iter_mut()
            .filter(|s| s.cook_id == cook_id && s.status == CertificationStatus::Pending)
            .max_by_key(|s| s.submitted_at)
        {
            s.status = status;
            s.reviewed_at = Some(now);
        }
        Ok(cook)
    }

    async fn revoke_verification(&self, cook_id: Uuid) -> StoreResult<Cook> {
        let mut g = self.inner.lock().await;
        let cook = g.cooks.get_mut(&cook_id).ok_or(StoreError::NotFound("cook"))?;
        cook.revoke_verification();
        Ok(cook.clone())
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
        f(&cook, &mut updated)?;
        g.orders.insert(id, updated.clone());
        Ok(updated)
    }
}
