//! Profile blocks (spec 4.4, 12.1 step 5): the top active facts of a scope rendered to markdown
//! within 400 tokens, cached in `profiles` with a `facts_version` so a re-render happens only when
//! the facts behind it changed.

use chrono::{DateTime, Utc};
use memvault_core::estimate_tokens;
use memvault_core::ids::short_id;
use memvault_core::invalidate::notify_invalidate;
use memvault_core::schemas::common::ScopeKind;
use memvault_core::schemas::memories::{UNCONFIRMED_AFTER_DAYS, fact_caution};
use sqlx::PgConnection;
use uuid::Uuid;

/// Token budget of one rendered profile.
pub const PROFILE_MAX_TOKENS: i32 = 400;

#[derive(Debug, sqlx::FromRow)]
struct ProfileFact {
    id: Uuid,
    subject: String,
    predicate: String,
    value_text: String,
    confidence: f64,
    first_seen_at: DateTime<Utc>,
    last_confirmed_at: DateTime<Utc>,
    pinned: bool,
    disputed: bool,
    devices: String,
}

/// One profile line: `- <subject>: <predicate> = <value> [fact#abc123 · 0.83 · laptop+desktop · 2026-10-02]`,
/// with `disputed` or `unconfirmed N months` as a last tag part for a fact to treat with care.
fn render_line(f: &ProfileFact, now: DateTime<Utc>) -> String {
    let mut tag = vec![format!("fact#{}", short_id(&f.id)), format!("{:.2}", f.confidence)];
    if !f.devices.is_empty() {
        tag.push(f.devices.clone());
    }
    tag.push(f.first_seen_at.format("%Y-%m-%d").to_string());
    tag.extend(fact_caution(f.confidence, f.last_confirmed_at, f.disputed, f.pinned, now));
    format!("- {}: {} = {} [{}]", f.subject, f.predicate, f.value_text.replace('\n', " "), tag.join(" · "))
}

/// Lines in rank order until the next one would exceed `max_tokens`.
fn fill(lines: impl IntoIterator<Item = String>, max_tokens: i32) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut used = 0;
    for line in lines {
        let cost = estimate_tokens(&line) + 1;
        if used + cost > max_tokens {
            break;
        }
        used += cost;
        out.push(line);
    }
    out.join("\n")
}

/// Hash of everything a profile line is rendered from; changes iff the rendering could change (the
/// caution tag counts: the dispute flag, and the months since confirmation once past the threshold).
/// Secret facts are left out of profiles (they never enter an automatic context pack), so they are
/// left out of the hash too.
async fn facts_version(
    conn: &mut PgConnection,
    space_id: Uuid,
    kind: ScopeKind,
    key: &str,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT hashtextextended(coalesce(string_agg(
                  id::text || '|' || confidence::text || '|' || importance::text || '|' || pinned::text || '|'
                  || value_text || '|' || learned_on_client_ids::text || '|' || (disputed_at IS NOT NULL)::text || '|'
                  || CASE WHEN now() - last_confirmed_at >= make_interval(days => $4)
                          THEN floor(extract(epoch FROM now() - last_confirmed_at) / 2592000)::text ELSE '' END,
                  ';' ORDER BY id), ''), 0)
           FROM facts WHERE space_id = $1 AND scope_kind = $2 AND scope_key = $3 AND status = 'active'
            AND sensitivity <> 'secret'",
    )
    .bind(space_id)
    .bind(kind)
    .bind(key)
    .bind(i32::try_from(UNCONFIRMED_AFTER_DAYS).unwrap_or(i32::MAX))
    .fetch_one(conn)
    .await
}

/// Re-render the profile of one scope when its active facts changed since the stored
/// `facts_version`; a scope without active facts loses its profile row. Returns whether anything
/// was written, in which case the context cache of the scope is invalidated.
pub async fn refresh_scope(
    conn: &mut PgConnection,
    space_id: Uuid,
    kind: ScopeKind,
    key: &str,
) -> Result<bool, sqlx::Error> {
    let version = facts_version(conn, space_id, kind, key).await?;
    let stored: Option<i64> = sqlx::query_scalar(
        "SELECT facts_version FROM profiles WHERE space_id = $1 AND scope_kind = $2 AND scope_key = $3",
    )
    .bind(space_id)
    .bind(kind)
    .bind(key)
    .fetch_optional(&mut *conn)
    .await?;
    let facts: Vec<ProfileFact> = sqlx::query_as(
        "SELECT f.id, e.canonical_name AS subject, f.predicate, f.value_text, f.confidence::float8 AS confidence,
                f.first_seen_at, f.last_confirmed_at, f.pinned, f.disputed_at IS NOT NULL AS disputed,
                coalesce((SELECT string_agg(c.device_id, '+' ORDER BY c.device_id)
                            FROM clients c WHERE c.id = ANY (f.learned_on_client_ids)), '') AS devices
           FROM facts f JOIN entities e ON e.id = f.subject_id
          WHERE f.space_id = $1 AND f.scope_kind = $2 AND f.scope_key = $3 AND f.status = 'active'
            AND f.sensitivity <> 'secret'
          ORDER BY f.pinned DESC, f.importance * f.confidence DESC, f.id
          LIMIT 200",
    )
    .bind(space_id)
    .bind(kind)
    .bind(key)
    .fetch_all(&mut *conn)
    .await?;
    if facts.is_empty() {
        if stored.is_none() {
            return Ok(false);
        }
        sqlx::query("DELETE FROM profiles WHERE space_id = $1 AND scope_kind = $2 AND scope_key = $3")
            .bind(space_id)
            .bind(kind)
            .bind(key)
            .execute(&mut *conn)
            .await?;
    } else {
        if stored == Some(version) {
            return Ok(false);
        }
        let now = Utc::now();
        let body = fill(facts.iter().map(|f| render_line(f, now)), PROFILE_MAX_TOKENS);
        sqlx::query(
            "INSERT INTO profiles (space_id, scope_kind, scope_key, body_md, token_estimate, facts_version)
             VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT (space_id, scope_kind, scope_key) DO UPDATE
               SET body_md = EXCLUDED.body_md, token_estimate = EXCLUDED.token_estimate,
                   facts_version = EXCLUDED.facts_version, rendered_at = now()",
        )
        .bind(space_id)
        .bind(kind)
        .bind(key)
        .bind(&body)
        .bind(estimate_tokens(&body))
        .bind(version)
        .execute(&mut *conn)
        .await?;
    }
    notify_invalidate(conn, space_id, kind, key).await?;
    Ok(true)
}

/// [`refresh_scope`] for every scope that has facts or a profile (planner step 5). Returns the
/// number of profiles written or removed.
pub async fn refresh_space(conn: &mut PgConnection, space_id: Uuid) -> Result<i64, sqlx::Error> {
    let scopes: Vec<(ScopeKind, String)> = sqlx::query_as(
        "SELECT scope_kind, scope_key FROM facts WHERE space_id = $1 AND status = 'active'
         UNION SELECT scope_kind, scope_key FROM profiles WHERE space_id = $1
         ORDER BY 1, 2",
    )
    .bind(space_id)
    .fetch_all(&mut *conn)
    .await?;
    let mut changed = 0;
    for (kind, key) in scopes {
        changed += i64::from(refresh_scope(conn, space_id, kind, &key).await?);
    }
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fact(n: usize) -> ProfileFact {
        ProfileFact {
            id: Uuid::nil(),
            subject: "alex".into(),
            predicate: "prefers.editor".into(),
            value_text: "x".repeat(n),
            confidence: 0.83,
            first_seen_at: seen(),
            last_confirmed_at: seen(),
            pinned: false,
            disputed: false,
            devices: "desktop+laptop".into(),
        }
    }

    fn seen() -> DateTime<Utc> {
        DateTime::from_timestamp(1_790_000_000, 0).unwrap()
    }

    #[test]
    fn line_format() {
        let line = render_line(&fact(3), seen());
        assert_eq!(line, "- alex: prefers.editor = xxx [fact#000000 · 0.83 · desktop+laptop · 2026-09-21]");
    }

    #[test]
    fn line_carries_the_caution_tag() {
        let later = seen() + chrono::Duration::days(125);
        assert!(render_line(&fact(3), later).ends_with("· 2026-09-21 · unconfirmed 4 months]"));
        let disputed = ProfileFact { disputed: true, ..fact(3) };
        assert!(render_line(&disputed, seen()).ends_with("· 2026-09-21 · disputed]"));
        let pinned = ProfileFact { disputed: true, pinned: true, ..fact(3) };
        assert!(render_line(&pinned, later).ends_with("· 2026-09-21]"));
    }

    #[test]
    fn fill_stops_at_budget() {
        let lines: Vec<String> = (0..10).map(|_| render_line(&fact(200), seen())).collect();
        let body = fill(lines.clone(), PROFILE_MAX_TOKENS);
        assert!(estimate_tokens(&body) <= PROFILE_MAX_TOKENS);
        assert!(body.lines().count() < 10 && body.lines().count() >= 5);
        assert_eq!(fill(lines, 0), "");
    }
}
