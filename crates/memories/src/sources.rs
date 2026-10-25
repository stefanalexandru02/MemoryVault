//! Document sources: `POST`, `GET` and `DELETE /v1/sources`.
//!
//! A source is `(space, scope, uri)` and its chunks are `documents` rows (see [`crate::chunk`]). Posting it
//! with the content hash it already has changes nothing; with another hash it retires the old chunks, drops
//! their embeddings and writes the new ones, each with an embed job, in one transaction. A transaction-scoped
//! advisory lock per source serialises concurrent posts of the same uri. Documents are not part of the
//! SessionStart block, so none of this signals the context cache.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use chrono::{DateTime, Utc};
use memvault_core::auth::{MEMORIES_READ, MEMORIES_WRITE, resolve_read_spaces, resolve_write_space};
use memvault_core::http::{Json, Query};
use memvault_core::ids::new_id;
use memvault_core::jobs::enqueue_embed;
use memvault_core::schemas::common::{ContentType, MemoryTable, Scope, ScopeKind, estimate_tokens};
use memvault_core::schemas::memories::{
    SourceDeleteQuery, SourceInfo, SourceList, SourceListQuery, SourcePut, SourceResponse, SourceStatus,
    validate_source_scope,
};
use memvault_core::{ApiError, AppState, Identity, require_scope, user_tx};
use serde_json::json;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::chunk::chunk;
use crate::memories::{Audit, record_audit};

pub(crate) async fn put(
    State(state): State<AppState>,
    identity: Identity,
    Json(body): Json<SourcePut>,
) -> Result<impl IntoResponse, ApiError> {
    require_scope(&state, &identity, MEMORIES_WRITE).await?;
    body.validate(state.settings.source_max_bytes)?;
    let mut tx = user_tx(&state.db.app, &identity).await?;
    let space = resolve_write_space(&mut tx, &identity, body.space_id).await?;
    let out = ingest(&mut tx, &identity, space, &body).await?;
    tx.commit().await?;
    let status = if out.status == SourceStatus::Unchanged { StatusCode::OK } else { StatusCode::CREATED };
    Ok((status, Json(out)))
}

pub(crate) async fn list(
    State(state): State<AppState>,
    identity: Identity,
    Query(query): Query<SourceListQuery>,
) -> Result<impl IntoResponse, ApiError> {
    require_scope(&state, &identity, MEMORIES_READ).await?;
    if let Some(scope) = &query.scope {
        validate_source_scope(scope)?;
    }
    let mut tx = user_tx(&state.db.app, &identity).await?;
    let mut spaces = resolve_read_spaces(&mut tx, &identity, query.space_id.as_ref().map(std::slice::from_ref)).await?;
    // Without `space_id` the list is that of the space a POST writes to: the default one, which comes first.
    if query.space_id.is_none() {
        spaces.truncate(1);
    }
    type Row = (Uuid, ScopeKind, String, String, String, ContentType, String, i64, DateTime<Utc>);
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT space_id, scope_kind, scope_key, source_uri, title, content_type, content_hash, count(*),
                max(created_at)
           FROM documents
          WHERE space_id = ANY($1) AND retired_at IS NULL
            AND ($2::scope_kind IS NULL OR (scope_kind = $2 AND scope_key = $3))
          GROUP BY space_id, scope_kind, scope_key, source_uri, title, content_type, content_hash
          ORDER BY scope_kind, scope_key, source_uri, space_id",
    )
    .bind(spaces)
    .bind(query.scope.as_ref().map(|s| s.kind))
    .bind(query.scope.as_ref().map(|s| s.key.as_str()))
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    let sources = rows
        .into_iter()
        .map(|(space_id, kind, key, uri, title, content_type, content_hash, chunks, ingested_at)| SourceInfo {
            space_id,
            scope: Scope::new(kind, key),
            uri,
            title,
            content_type,
            chunks,
            content_hash,
            ingested_at,
        })
        .collect();
    Ok(Json(SourceList { sources }))
}

pub(crate) async fn remove(
    State(state): State<AppState>,
    identity: Identity,
    Query(query): Query<SourceDeleteQuery>,
) -> Result<impl IntoResponse, ApiError> {
    require_scope(&state, &identity, MEMORIES_WRITE).await?;
    validate_source_scope(&query.scope)?;
    let mut tx = user_tx(&state.db.app, &identity).await?;
    let space = resolve_write_space(&mut tx, &identity, query.space_id).await?;
    lock_source(&mut tx, space, &query.scope, &query.uri).await?;
    let ids: Vec<Uuid> = sqlx::query_scalar(
        "DELETE FROM documents
          WHERE space_id = $1 AND scope_kind = $2 AND scope_key = $3 AND source_uri = $4
          RETURNING id",
    )
    .bind(space)
    .bind(query.scope.kind)
    .bind(&query.scope.key)
    .bind(&query.uri)
    .fetch_all(&mut *tx)
    .await?;
    let Some(first) = ids.first().copied() else {
        return Err(ApiError::not_found(format!("no source {} in {}", query.uri, query.scope)));
    };
    drop_embeddings(&mut tx, &ids).await?;
    let audit = Audit {
        space_id: space,
        action: "doc.delete",
        table: MemoryTable::Documents,
        target_id: first,
        before: Some(json!({"uri": query.uri, "scope": query.scope.as_text(), "chunks": ids.len()})),
        after: None,
        reason: Some("source removed"),
    };
    record_audit(&mut tx, &identity, audit).await?;
    tx.commit().await?;
    Ok(Json(json!({"uri": query.uri, "scope": query.scope, "space_id": space, "deleted": ids.len()})))
}

/// One source at a time per `(space, scope, uri)`: `FOR UPDATE` locks nothing when the source has no rows yet.
async fn lock_source(conn: &mut PgConnection, space: Uuid, scope: &Scope, uri: &str) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("source:{space}:{scope}:{uri}"))
        .execute(conn)
        .await?;
    Ok(())
}

async fn drop_embeddings(conn: &mut PgConnection, ids: &[Uuid]) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM embeddings WHERE owner_table = 'documents' AND owner_id = ANY($1)")
        .bind(ids)
        .execute(conn)
        .await?;
    Ok(())
}

/// The title to store: the request's, else the first top-level heading, else the last segment of the uri.
fn title_of(request: &SourcePut, top_heading: Option<String>) -> String {
    let given = request.title.as_deref().map(str::trim).filter(|t| !t.is_empty());
    let from_uri = || {
        let name = request.uri.trim_end_matches('/').rsplit(['/', '\\']).next().unwrap_or_default();
        if name.is_empty() { request.uri.clone() } else { name.to_string() }
    };
    given.map(String::from).or(top_heading).unwrap_or_else(from_uri)
}

async fn ingest(
    conn: &mut PgConnection,
    identity: &Identity,
    space: Uuid,
    request: &SourcePut,
) -> Result<SourceResponse, ApiError> {
    let scope = &request.scope;
    let chunked = chunk(request.content_type, &request.content);
    if chunked.chunks.is_empty() {
        return Err(ApiError::invalid("content has no text to keep"));
    }
    lock_source(conn, space, scope, &request.uri).await?;
    let live: Option<(String, i64)> = sqlx::query_as(
        "SELECT content_hash, count(*) FROM documents
          WHERE space_id = $1 AND scope_kind = $2 AND scope_key = $3 AND source_uri = $4 AND retired_at IS NULL
          GROUP BY content_hash",
    )
    .bind(space)
    .bind(scope.kind)
    .bind(&scope.key)
    .bind(&request.uri)
    .fetch_optional(&mut *conn)
    .await?;
    let respond = |status, chunks, retired| SourceResponse {
        uri: request.uri.clone(),
        scope: scope.clone(),
        space_id: space,
        status,
        chunks,
        retired,
        content_hash: request.content_hash.clone(),
    };
    if let Some((hash, chunks)) = &live
        && *hash == request.content_hash
    {
        return Ok(respond(SourceStatus::Unchanged, *chunks, 0));
    }

    let retired: Vec<Uuid> = sqlx::query_scalar(
        "UPDATE documents SET retired_at = now()
          WHERE space_id = $1 AND scope_kind = $2 AND scope_key = $3 AND source_uri = $4 AND retired_at IS NULL
          RETURNING id",
    )
    .bind(space)
    .bind(scope.kind)
    .bind(&scope.key)
    .bind(&request.uri)
    .fetch_all(&mut *conn)
    .await?;
    drop_embeddings(conn, &retired).await?;

    let title = title_of(request, chunked.top_heading);
    let mut first = None;
    for (index, c) in chunked.chunks.iter().enumerate() {
        let id = new_id();
        sqlx::query(
            "INSERT INTO documents (id, space_id, scope_kind, scope_key, source_uri, title, content_type, chunk_index,
                                    heading_path, content, content_hash, token_estimate, ingested_by)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)",
        )
        .bind(id)
        .bind(space)
        .bind(scope.kind)
        .bind(&scope.key)
        .bind(&request.uri)
        .bind(&title)
        .bind(request.content_type)
        .bind(i32::try_from(index).unwrap_or(i32::MAX))
        .bind(&c.heading_path)
        .bind(&c.content)
        .bind(&request.content_hash)
        .bind(estimate_tokens(&c.content))
        .bind(identity.client_id)
        .execute(&mut *conn)
        .await?;
        enqueue_embed(conn, MemoryTable::Documents.as_str(), id).await?;
        first.get_or_insert(id);
    }
    let chunks = i64::try_from(chunked.chunks.len()).unwrap_or(i64::MAX);
    let (status, action, before) = match &live {
        Some((hash, count)) => (
            SourceStatus::Replaced,
            "doc.replace",
            Some(json!({"uri": request.uri, "content_hash": hash, "chunks": count})),
        ),
        None => (SourceStatus::Created, "doc.ingest", None),
    };
    let audit = Audit {
        space_id: space,
        action,
        table: MemoryTable::Documents,
        target_id: first.expect("a source with chunks has a first one"),
        before,
        after: Some(
            json!({"uri": request.uri, "scope": scope.as_text(), "content_hash": request.content_hash, "chunks": chunks}),
        ),
        reason: Some("ingested"),
    };
    record_audit(conn, identity, audit).await?;
    Ok(respond(status, chunks, i64::try_from(retired.len()).unwrap_or(i64::MAX)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(uri: &str, title: Option<&str>) -> SourcePut {
        SourcePut {
            scope: Scope::global(),
            space_id: None,
            uri: uri.into(),
            title: title.map(String::from),
            content_type: ContentType::Markdown,
            content: "x".into(),
            content_hash: "h".into(),
        }
    }

    #[test]
    fn title_prefers_the_request_then_the_heading_then_the_file_name() {
        let r = request("docs/adr/0003-postgres.md", Some(" Use Postgres "));
        assert_eq!(title_of(&r, Some("Heading".into())), "Use Postgres");
        let r = request("docs/adr/0003-postgres.md", None);
        assert_eq!(title_of(&r, Some("Heading".into())), "Heading");
        assert_eq!(title_of(&r, None), "0003-postgres.md");
        assert_eq!(title_of(&request("runbooks/", Some("  ")), None), "runbooks");
        assert_eq!(title_of(&request("/", None), None), "/");
    }
}
