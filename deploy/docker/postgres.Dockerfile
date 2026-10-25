# syntax=docker/dockerfile:1
#
# CloudNativePG-compatible PostgreSQL 17 with pgvector and ParadeDB pg_search.
#
# First choice: ParadeDB publishes CNPG-usable images that already bundle pg_search and pgvector
# (paradedb/paradedb:<tag>, PostgreSQL 17 variant). Point postgres-cluster.yaml's imageName at that
# image and skip this file when the operator accepts it.
#
# Fallback (this file): the CNPG base image plus the pgvector package and, optionally, the pg_search
# .deb from the ParadeDB release page. Build from this directory:
#   docker build -f deploy/docker/postgres.Dockerfile \
#     --build-arg PG_SEARCH_DEB_URL=<url of the pg_search .deb for postgresql-17, amd64> \
#     -t memvault-postgres:17 .
#
# pg_search is optional at runtime. When the extension is absent, migration 0011 and the code path
# in core fall back to tsvector + websearch_to_tsquery + ts_rank_cd, so the image builds and runs
# without PG_SEARCH_DEB_URL.
#
# The package names assume the CNPG base image has the PGDG apt repository configured. Confirm
# with `apt-cache policy postgresql-17-pgvector` inside the base image before the first build.

ARG CNPG_BASE=ghcr.io/cloudnative-pg/postgresql:17
FROM ${CNPG_BASE}

ARG PGVECTOR_PKG=postgresql-17-pgvector
ARG PG_SEARCH_DEB_URL=

USER root
RUN apt-get update \
 && apt-get install -y --no-install-recommends "${PGVECTOR_PKG}" ca-certificates curl \
 && if [ -n "${PG_SEARCH_DEB_URL}" ]; then \
      curl -fsSL -o /tmp/pg_search.deb "${PG_SEARCH_DEB_URL}" \
      && apt-get install -y --no-install-recommends /tmp/pg_search.deb \
      && rm -f /tmp/pg_search.deb; \
    fi \
 && apt-get purge -y curl \
 && apt-get autoremove -y \
 && rm -rf /var/lib/apt/lists/*

# 26 is the postgres user in CNPG images; the operator refuses to run other UIDs.
USER 26
