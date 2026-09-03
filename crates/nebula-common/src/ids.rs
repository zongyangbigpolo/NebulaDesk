//! Strongly-typed identifiers.
//!
//! Every entity gets its own newtype so a `MachineId` can never be passed
//! where a `UserId` is expected — a class of bug that plagued the V1 code
//! where everything was a bare `String`.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

macro_rules! uuid_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(pub Uuid);

        impl $name {
            /// Time-ordered v7 UUID: keeps database B-tree inserts sequential.
            #[must_use]
            pub fn new() -> Self {
                Self(Uuid::now_v7())
            }

            #[must_use]
            pub const fn from_uuid(id: Uuid) -> Self {
                Self(id)
            }

            #[must_use]
            pub const fn as_uuid(&self) -> Uuid {
                self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(&self.0, f)
            }
        }

        impl FromStr for $name {
            type Err = uuid::Error;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Ok(Self(Uuid::parse_str(s)?))
            }
        }

        impl From<Uuid> for $name {
            fn from(id: Uuid) -> Self {
                Self(id)
            }
        }

        impl From<$name> for Uuid {
            fn from(id: $name) -> Uuid {
                id.0
            }
        }
    };
}

uuid_id!(
    /// A tenant (organisation). Every other entity is scoped to one.
    TenantId
);
uuid_id!(
    /// A human account.
    UserId
);
uuid_id!(
    /// A registered *server machine* running `nebula-agent`.
    MachineId
);
uuid_id!(
    /// A published resource — either a full desktop or a single application.
    ResourceId
);
uuid_id!(
    /// One remote-desktop session, from brokering to teardown.
    SessionId
);
uuid_id!(
    /// A relay data-plane node.
    RelayId
);
uuid_id!(
    /// A gateway edge node.
    GatewayId
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_roundtrip_through_string() {
        let id = SessionId::new();
        let parsed: SessionId = id.to_string().parse().unwrap();
        assert_eq!(id, parsed);
    }

    #[test]
    fn v7_ids_are_time_ordered() {
        let a = SessionId::new();
        let b = SessionId::new();
        assert!(a < b, "v7 UUIDs must sort by creation time");
    }

    #[test]
    fn ids_serialize_transparently() {
        let id = MachineId::new();
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, format!("\"{id}\""));
    }
}
