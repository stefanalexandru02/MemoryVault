//! UUIDv7 ids (RFC 9562), generated server-side for every row.

use chrono::{DateTime, Utc};
use uuid::Uuid;

/// A new time-ordered UUIDv7. `uuid` keeps ids from one process monotonic.
pub fn new_id() -> Uuid {
    Uuid::now_v7()
}

/// Creation time embedded in a UUIDv7 (`None` for other versions).
pub fn id_time(id: &Uuid) -> Option<DateTime<Utc>> {
    let (secs, nanos) = id.get_timestamp()?.to_unix();
    DateTime::from_timestamp(i64::try_from(secs).ok()?, nanos)
}

/// Short display id used in citation tags (`obs#a1b2c3`): the last 6 hex chars (random part).
pub fn short_id(id: &Uuid) -> String {
    let s = id.simple().to_string();
    s[s.len() - 6..].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid7_version_order_time() {
        let ids: Vec<Uuid> = (0..5000).map(|_| new_id()).collect();
        assert!(ids.iter().all(|u| u.get_version_num() == 7 && u.get_variant() == uuid::Variant::RFC4122));
        let mut sorted = ids.clone();
        sorted.sort();
        assert_eq!(ids, sorted);
        sorted.dedup();
        assert_eq!(sorted.len(), ids.len());
        let t = id_time(&ids[0]).unwrap();
        assert!((Utc::now() - t).num_seconds().abs() < 2);
        assert_eq!(short_id(&ids[0]).len(), 6);
        assert!(ids[0].simple().to_string().ends_with(&short_id(&ids[0])));
    }
}
