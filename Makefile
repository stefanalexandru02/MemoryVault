# Local MemoryVault helpers. `make` lists the targets.
#
# Quick start:
#   make up                       # full stack with TEI (or `make up-lite` without the models)
#   make connect-local            # wire Claude Code on this machine to the stack (token, hooks, MCP)
#   make disconnect               # undo that
#   make connect-remote URL=https://memory.example.ts.net TOKEN=mem_...   # wire it to another server

COMPOSE ?= docker compose
HANDLE  ?= $(shell whoami)
NAME    ?= $(HANDLE)
CLIENT  ?= $(shell hostname -s 2>/dev/null || hostname)
KIND    ?= device
SERVICE ?=

ifneq (,$(wildcard .env))
include .env
endif
MEMVAULT_PORT ?= 8000
PG_PORT       ?= 55432
BIND_ADDR     ?= 127.0.0.1
# TEI's CPU images are single-architecture: arm64 has its own tag. cpu-1.8 was the default in .env files
# written before arm64 support and has no arm64 build, so it is treated as unset.
TEI_TAG       := $(if $(filter arm64 aarch64,$(shell uname -m)),cpu-arm64-1.9.4,cpu-1.9.4)
ifneq (,$(filter %:cpu-1.8,$(TEI_IMAGE)))
TEI_IMAGE     :=
endif
TEI_IMAGE     := $(or $(TEI_IMAGE),ghcr.io/huggingface/text-embeddings-inference:$(TEI_TAG))
export TEI_IMAGE
API_HOST      := $(if $(filter 0.0.0.0,$(BIND_ADDR)),127.0.0.1,$(BIND_ADDR))
MEMORY_URL    ?= http://$(API_HOST):$(MEMVAULT_PORT)

ADMIN   := $(COMPOSE) exec -T -e LOG_LEVEL=warn api memvault admin
CONNECT := ./clients/claude-code-plugin/connect.sh

.DEFAULT_GOAL := help
.PHONY: help env build up up-lite down restart ps logs migrate bootstrap user token tokens admin \
        consolidate reindex psql connect-local connect-remote connect-status disconnect install-plugin test clean

help: ## List the targets
	@awk 'BEGIN {FS = ":.*## "} /^[a-zA-Z_-]+:.*## / {printf "  \033[36m%-15s\033[0m %s\n", $$1, $$2}' $(MAKEFILE_LIST)
	@echo
	@echo "  Variables: HANDLE=$(HANDLE) NAME=$(NAME) CLIENT=$(CLIENT) KIND=$(KIND) MEMORY_URL=$(MEMORY_URL)"
	@echo "  TEI image: $(TEI_IMAGE)"

env: .env ## Create .env with fresh secrets (kept if it exists)

.env:
	@rand() { head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n'; }; \
	sed -e "s/^TOKEN_PEPPER=.*/TOKEN_PEPPER=$$(rand)/" \
	    -e "s/^POSTGRES_PASSWORD=.*/POSTGRES_PASSWORD=$$(rand)/" \
	    -e "s/^MEMSVC_APP_PASSWORD=.*/MEMSVC_APP_PASSWORD=$$(rand)/" \
	    -e "s/^MEMSVC_SYSTEM_PASSWORD=.*/MEMSVC_SYSTEM_PASSWORD=$$(rand)/" \
	    .env.example > .env
	@chmod 600 .env
	@echo "wrote .env with new secrets (back it up: TOKEN_PEPPER is needed to keep tokens valid)"

build: .env ## Build the memvault image
	$(COMPOSE) build

up: .env ## Start the full stack (Postgres, TEI, migrations, API, worker, scheduler)
	COMPOSE_PROFILES=tei FAKE_TEI=0 $(COMPOSE) up -d --build --wait
	@echo "API: $(MEMORY_URL)  (first start: TEI downloads its models, see 'make logs SERVICE=tei-embed')"

up-lite: .env ## Start without TEI, using fake deterministic embeddings
	FAKE_TEI=1 $(COMPOSE) up -d --build --wait
	@echo "API: $(MEMORY_URL)  (FAKE_TEI=1: run 'make reindex' after switching to 'make up')"

down: ## Stop the stack (data is kept)
	COMPOSE_PROFILES=tei $(COMPOSE) down

restart: down up ## Restart the full stack

ps: ## Show service status
	COMPOSE_PROFILES=tei $(COMPOSE) ps

logs: ## Follow logs (SERVICE=api for one service)
	COMPOSE_PROFILES=tei $(COMPOSE) logs -f --tail=100 $(SERVICE)

migrate: ## Apply pending migrations
	$(COMPOSE) run --rm migrate

bootstrap: user token ## Create user HANDLE and a token for CLIENT, print the client config

user: ## Create user HANDLE (NAME sets the display name)
	@$(ADMIN) user create "$(HANDLE)" --display-name "$(NAME)" >/dev/null 2>&1 \
	  && echo "created user $(HANDLE)" || echo "user $(HANDLE) not created (it may already exist)"

token: ## Mint a token for HANDLE on CLIENT (KIND=device|runner|admin)
	@out=$$($(ADMIN) token create --user "$(HANDLE)" --client "$(CLIENT)" --kind "$(KIND)") || exit 1; \
	tok=$$(printf '%s' "$$out" | sed -n 's/.*"token": *"\(mem_[^"]*\)".*/\1/p'); \
	[ -n "$$tok" ] || { echo "$$out"; exit 1; }; \
	echo; echo "Token for $(HANDLE) on $(CLIENT) ($(KIND)). It is shown once; put it in ~/.memory/config:"; echo; \
	echo "  MEMORY_URL=$(MEMORY_URL)"; echo "  MEMORY_TOKEN=$$tok"; echo "  DEVICE_ID=$(CLIENT)"; echo

tokens: ## List tokens for HANDLE
	$(ADMIN) token list --user "$(HANDLE)"

admin: ## Run an admin command: make admin ARGS="space list"
	$(ADMIN) $(ARGS)

consolidate: ## Run one consolidation planning pass now
	$(COMPOSE) exec -T api memvault planner

reindex: ## Re-embed everything with the current model (after leaving FAKE_TEI)
	$(ADMIN) reindex

psql: ## Open psql on the memsvc database as the superuser
	$(COMPOSE) exec postgres psql -U postgres -d memsvc

connect-local: .env ## One step: mint a token for this machine, install the hooks and MCP server in Claude Code
	@curl -fsS -o /dev/null -m 5 "$(MEMORY_URL)/health" \
	  || { echo "the API is not answering at $(MEMORY_URL): run 'make up' (or 'make up-lite') first" >&2; exit 1; }
	@set -e; tok=""; \
	if $(CONNECT) probe --url "$(MEMORY_URL)"; then \
	  echo "keeping the token already in ~/.memory/config"; \
	else \
	  $(ADMIN) user create "$(HANDLE)" --display-name "$(NAME)" >/dev/null 2>&1 || true; \
	  out=$$($(ADMIN) token create --user "$(HANDLE)" --client "$(CLIENT)" --kind "$(KIND)" 2>&1); \
	  tok=$$(printf '%s' "$$out" | sed -n 's/.*"token": *"\(mem_[^"]*\)".*/\1/p'); \
	  [ -n "$$tok" ] || { echo "$$out" >&2; exit 1; }; \
	  echo "minted a token for $(HANDLE) on $(CLIENT)"; \
	fi; \
	$(CONNECT) connect --url "$(MEMORY_URL)" $${tok:+--token "$$tok"} --device "$(CLIENT)"

connect-remote: ## Same, for a server somewhere else: make connect-remote URL=https://... TOKEN=mem_...
	@[ "$(origin URL)" = "command line" ] && [ "$(origin TOKEN)" = "command line" ] \
	  || { echo "usage: make connect-remote URL=https://memory.example.ts.net TOKEN=mem_... [CLIENT=device-name]" >&2; exit 2; }
	@$(CONNECT) connect --url "$(URL)" --token "$(TOKEN)" --device "$(CLIENT)"

connect-status: ## Show what Claude Code is connected to and whether it works
	@$(CONNECT) status

disconnect: ## Remove the hooks, MCP server and token from this machine (PURGE=1 also deletes ~/.memory)
	@# A token minted by this stack is revoked too. Tokens of other servers are left to their admins.
	@cfg="$${MEMORY_HOME:-$$HOME/.memory}/config"; \
	tok=$$(sed -n 's/^MEMORY_TOKEN=//p' "$$cfg" 2>/dev/null | tail -n 1); \
	url=$$(sed -n 's/^MEMORY_URL=//p' "$$cfg" 2>/dev/null | tail -n 1); \
	prefix=$$(printf '%s' "$$tok" | cut -d_ -f2); \
	revoked=""; \
	if [ -n "$$prefix" ] && [ "$$url" = "$(MEMORY_URL)" ] && $(ADMIN) token revoke "$$prefix" >/dev/null 2>&1; then \
	  revoked=1; echo "revoked token $$prefix on the local stack"; fi; \
	$(CONNECT) disconnect $(if $(PURGE),--purge); \
	if [ -n "$$prefix" ] && [ -z "$$revoked" ]; then echo "The token ($$url) still works on its server until an admin revokes it."; fi

install-plugin: ## Build mem-hook and set up ~/.memory on this machine
	./clients/claude-code-plugin/install.sh

test: .env ## Run the Rust test suite against the compose Postgres
	DATABASE_URL=postgres://postgres:$(POSTGRES_PASSWORD)@127.0.0.1:$(PG_PORT)/postgres \
	  MEMSVC_APP_PASSWORD=$(MEMSVC_APP_PASSWORD) cargo test --workspace --no-fail-fast

clean: ## Stop the stack and DELETE all data volumes (asks first)
	@printf "This deletes the database and the TEI model cache. Type 'yes' to continue: "; \
	read ans; [ "$$ans" = yes ] || { echo aborted; exit 1; }
	COMPOSE_PROFILES=tei $(COMPOSE) down -v --remove-orphans
