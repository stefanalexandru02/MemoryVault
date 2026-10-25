//! Denormalization (spec 12.1 step 6): `facts.learned_on_client_ids` from evidence ->
//! observations -> sessions -> clients.

use sqlx::PgConnection;
use uuid::Uuid;

/// Refresh the client ids of live facts from their evidence. A fact whose evidence resolves to no
/// session (explicit writes, deleted observations) keeps what it has. Returns the rows changed.
pub async fn refresh_learned_on(conn: &mut PgConnection, space_id: Uuid) -> Result<u64, sqlx::Error> {
    Ok(sqlx::query(
        "UPDATE facts f SET learned_on_client_ids = d.clients
           FROM (SELECT f2.id, array_agg(DISTINCT s.client_id ORDER BY s.client_id) AS clients
                   FROM facts f2
                   JOIN LATERAL unnest(f2.evidence_observation_ids) AS ev(observation_id) ON true
                   JOIN observations o ON o.id = ev.observation_id AND o.space_id = f2.space_id
                   JOIN sessions s ON s.id = o.session_id
                  WHERE f2.space_id = $1 AND f2.status IN ('candidate', 'active')
                  GROUP BY f2.id) d
          WHERE f.id = d.id AND f.learned_on_client_ids IS DISTINCT FROM d.clients",
    )
    .bind(space_id)
    .execute(conn)
    .await?
    .rows_affected())
}
