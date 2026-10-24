//! Shared application state handed to every router (`Router<AppState>`) and background task.

use std::sync::Arc;

use anyhow::{Context, Result};

use crate::appliers::AppliersRegistry;
use crate::config::Settings;
use crate::db::Db;
use crate::prompts::PromptRegistry;
use crate::schemas::validate::OutputSchemas;
use crate::tei::{Embedder, build_embedder};

#[derive(Clone)]
pub struct AppState {
    pub settings: Arc<Settings>,
    pub db: Db,
    pub tei: Arc<dyn Embedder>,
    pub prompts: Arc<PromptRegistry>,
    pub schemas: Arc<OutputSchemas>,
    pub appliers: Arc<AppliersRegistry>,
}

impl std::fmt::Debug for AppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppState")
            .field("db", &self.db)
            .field("tei_model", &self.tei.model())
            .field("prompts", &self.prompts.len())
            .field("schemas", &self.schemas)
            .field("appliers", &self.appliers)
            .finish()
    }
}

impl AppState {
    /// Build from settings and already-opened pools: TEI client (or FakeTei), prompts from
    /// `PROMPTS_DIR`, output schemas from `SCHEMAS_DIR`.
    pub fn new(settings: Settings, db: Db, appliers: AppliersRegistry) -> Result<Self> {
        let prompts = PromptRegistry::load(&settings.prompts_dir)?;
        let schemas = OutputSchemas::load(&settings.schemas_dir).context("loading output schemas")?;
        Ok(Self {
            tei: build_embedder(&settings),
            settings: Arc::new(settings),
            db,
            prompts: Arc::new(prompts),
            schemas: Arc::new(schemas),
            appliers: Arc::new(appliers),
        })
    }

    /// [`Db::connect`] then [`AppState::new`].
    pub async fn connect(settings: Settings, appliers: AppliersRegistry) -> Result<Self> {
        let db = Db::connect(&settings).await.context("connecting to the database")?;
        Self::new(settings, db, appliers)
    }
}
