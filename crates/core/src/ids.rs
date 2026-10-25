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

const SHORT_ID_LEN: usize = 6;

/// Short display id used in citation tags (`obs#a1b2c3`): the last 6 hex chars (random part).
pub fn short_id(id: &Uuid) -> String {
    let s = id.simple().to_string();
    s[s.len() - SHORT_ID_LEN..].to_string()
}

/// Display ids for `ids`, in order: the [`short_id`], lengthened to the shortest ending that no other id of the set
/// shares. The tools resolve any such ending, so a table never shows more characters than it must.
pub fn short_ids(ids: &[Uuid]) -> Vec<String> {
    let simple: Vec<String> = ids.iter().map(|id| id.simple().to_string()).collect();
    simple
        .iter()
        .map(|own| {
            let len = (SHORT_ID_LEN..own.len())
                .find(|&n| simple.iter().all(|other| other == own || other[other.len() - n..] != own[own.len() - n..]))
                .unwrap_or(own.len());
            own[own.len() - len..].to_string()
        })
        .collect()
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

    #[test]
    fn short_ids_lengthen_only_where_endings_collide() {
        let (a, b, c) = (Uuid::from_u128(0x1_abcdef), Uuid::from_u128(0x2_abcdef), Uuid::from_u128(0x3_ffffff));
        assert_eq!(short_ids(&[a, b, c]), ["1abcdef", "2abcdef", "ffffff"]);
        assert_eq!(short_ids(&[a, a, c]), ["abcdef", "abcdef", "ffffff"], "the same id is not a collision");
        assert!(short_ids(&[]).is_empty());
    }
}
