use serde::{Deserialize, Serialize};

/// What a user may do on the platform. Admins are never self-registered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Eater,
    Cook,
    Admin,
}

impl Role {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Eater => "eater",
            Self::Cook => "cook",
            Self::Admin => "admin",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "eater" => Self::Eater,
            "cook" => Self::Cook,
            "admin" => Self::Admin,
            _ => return None,
        })
    }
}

/// Authenticated caller, as established by the auth middleware.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CurrentUser {
    pub id: uuid::Uuid,
    pub role: Role,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_round_trips() {
        for r in [Role::Eater, Role::Cook, Role::Admin] {
            assert_eq!(Role::parse(r.as_str()), Some(r));
        }
        assert_eq!(Role::parse("root"), None);
    }
}
