use crate::protos::stratsync::*;
use crate::types::*;

use moka::sync::Cache;
use sqlx::{postgres::PgPoolOptions, types::Uuid};
use std::{env, sync::Arc, time::Duration};
use strat_sync_server::{StratSync, StratSyncServer};
use tokio::sync::{mpsc, Mutex};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};

pub const MAX_PEERS_PER_STRATEGY: usize = 64;
pub const MAX_CONNECTIONS_PER_USER: usize = 16;
pub const MAX_CONNECTIONS_PER_ANONYMOUS: usize = 256;

const STRATEGY_CAPACITY: u64 = 65536;
const STRATEGY_TTI: Duration = Duration::from_secs(24 * 60 * 60); // 24 hours
const PEER_CAPACITY: u64 = 65536;
const PEER_TTI: Duration = Duration::from_secs(12 * 60 * 60); // 12 hours

#[tonic::async_trait]
impl StratSync for StratSyncService {
    type EventStream = ReceiverStream<Result<EventResponse, Status>>;

    async fn event(
        &self,
        request: Request<SubscriptionRequest>,
    ) -> Result<Response<Self::EventStream>, Status> {
        self.rpc_event(request).await
    }

    async fn clear_other_sessions(
        &self,
        request: Request<ClearOtherSessionsRequest>,
    ) -> Result<Response<()>, Status> {
        self.rpc_clear_other_sessions(request).await
    }

    async fn elevate(&self, request: Request<ElevationRequest>) -> Result<Response<()>, Status> {
        self.rpc_elevate(request).await
    }

    async fn upsert_damage_option(
        &self,
        request: Request<UpsertDamageOptionRequest>,
    ) -> Result<Response<()>, Status> {
        self.rpc_upsert_damage_option(request).await
    }

    async fn mutate_entries(
        &self,
        request: Request<MutateEntriesRequest>,
    ) -> Result<Response<()>, Status> {
        self.rpc_mutate_entries(request).await
    }

    async fn update_player_job(
        &self,
        request: Request<UpdatePlayerJobRequest>,
    ) -> Result<Response<()>, Status> {
        self.rpc_update_player_job(request).await
    }

    async fn upsert_note(
        &self,
        request: Request<UpsertNoteRequest>,
    ) -> Result<Response<()>, Status> {
        self.rpc_upsert_note(request).await
    }

    async fn delete_note(
        &self,
        request: Request<DeleteNoteRequest>,
    ) -> Result<Response<()>, Status> {
        self.rpc_delete_note(request).await
    }
}

pub async fn build_stratsync() -> StratSyncServer<StratSyncService> {
    tracing::info!("Initializing StratSync service");

    let database_url =
        env::var("DATABASE_URL").expect("DATABASE_URL must be set on the environment");

    tracing::info!("Connecting to database");
    let pool = PgPoolOptions::new()
        .max_connections(8)
        .connect(&database_url)
        .await
        .expect("Unable to connect to database");

    tracing::info!("Database connection established");

    let action_cache: Cache<String, Arc<Vec<ActionInfo>>> = Cache::builder().build();

    tracing::info!("Loading action definitions from database");
    if let Ok(actions) = sqlx::query!(
        r#"SELECT id, job AS "job: String", cooldown, charges
           FROM public.actions"#
    )
    .fetch_all(&pool)
    .await
    {
        let action_count = actions.len();
        actions.iter().for_each(|row| {
            let mut abilities =
                (*action_cache.get(&row.job).unwrap_or(Arc::new(vec![]))).to_owned();
            abilities.push(ActionInfo {
                id: row.id,
                cooldown: row.cooldown,
                charges: row.charges,
            });

            action_cache.insert(row.job.to_owned(), Arc::new(abilities))
        });
        tracing::info!("Loaded {} action definitions", action_count);
    } else {
        tracing::warn!("Failed to load action definitions from database");
    }

    let raid_cache: Cache<Uuid, Arc<RaidInfo>> = Cache::builder().build();

    let strategy_lock: Cache<Uuid, Arc<Mutex<()>>> = Cache::builder().build();
    let strategy_context: Cache<Uuid, Arc<StrategyContext>> = Cache::builder()
        .max_capacity(STRATEGY_CAPACITY)
        .time_to_idle(STRATEGY_TTI)
        .build();

    tracing::info!(
        "Initialized caches: strategy capacity={}, TTI={:?}s, peer capacity={}, TTI={:?}s",
        STRATEGY_CAPACITY,
        STRATEGY_TTI.as_secs(),
        PEER_CAPACITY,
        PEER_TTI.as_secs()
    );

    // Create cleanup channel for eviction listener
    let (cleanup_tx, mut cleanup_rx) = mpsc::unbounded_channel::<CleanupTask>();
    let cleanup_tx_cloned = cleanup_tx.clone();

    let peer_context: Cache<String, Arc<PeerContext>> = Cache::builder()
        .max_capacity(PEER_CAPACITY)
        .time_to_idle(PEER_TTI)
        .eviction_listener(move |k: Arc<String>, v: Arc<PeerContext>, _| {
            tracing::debug!("Peer eviction triggered for peer {}", k);

            if !v.tx.is_closed() {
                let cloned_tx = v.tx.clone();
                let peer_id = k.to_string();
                tokio::spawn(async move {
                    tracing::debug!("Sending session expired message to peer {}", peer_id);
                    cloned_tx
                        .send(Err(Status::aborted("Session expired")))
                        .await
                        .ok();
                });
            }

            // Queue cleanup task instead of executing immediately
            tracing::debug!(
                "Queuing cleanup task for peer {}, strategy {}",
                k,
                v.strategy_id
            );
            cleanup_tx_cloned
                .send(CleanupTask {
                    peer_id: k.to_string(),
                    strategy_id: v.strategy_id,
                })
                .ok();
        })
        .build();

    // Spawn cleanup task processor
    let strategy_lock_for_cleanup = strategy_lock.clone();
    let strategy_context_for_cleanup = strategy_context.clone();
    let _peer_context_for_cleanup = peer_context.clone();

    tracing::info!("Starting cleanup task processor");
    tokio::spawn(async move {
        tracing::debug!("Cleanup task processor started");
        while let Some(task) = cleanup_rx.recv().await {
            tracing::debug!(
                "Processing cleanup task for peer {} in strategy {}",
                task.peer_id,
                task.strategy_id
            );

            let Some(strategy_ctx) = strategy_context_for_cleanup.get(&task.strategy_id) else {
                tracing::warn!(
                    "Strategy context not found for cleanup: {}",
                    task.strategy_id
                );
                continue;
            };

            let mut context = (*strategy_ctx).clone();
            let old_version = context.version;
            context.version += 1;

            let peers_before = context.peers.len();
            let peers_after: Vec<_> = context
                .peers
                .iter()
                .filter(|&peer_id| *peer_id != task.peer_id)
                .map(|peer_id| peer_id.to_owned())
                .collect();

            let elevated_peers_after: Vec<_> = context
                .elevated_peers
                .iter()
                .filter(|&peer_id| *peer_id != task.peer_id)
                .map(|peer_id| peer_id.to_owned())
                .collect();

            if peers_after.is_empty() {
                tracing::info!(
                    "Last peer removed, cleaning up strategy {} (version {})",
                    task.strategy_id,
                    old_version
                );
                strategy_lock_for_cleanup.invalidate(&task.strategy_id);
                strategy_context_for_cleanup.invalidate(&task.strategy_id);
            } else {
                tracing::debug!(
                    "Peer removed from strategy {}: {} -> {} peers, version {} -> {}",
                    task.strategy_id,
                    peers_before,
                    peers_after.len(),
                    old_version,
                    context.version
                );
                context.peers = peers_after;
                context.elevated_peers = elevated_peers_after;
                strategy_context_for_cleanup.insert(task.strategy_id, Arc::new(context));
            }
        }
        tracing::warn!("Cleanup task processor terminated");
    });

    tracing::info!("StratSync service initialization complete");

    StratSyncServer::new(StratSyncService {
        pool,
        action_cache,
        raid_cache,
        strategy_lock,
        strategy_context,
        peer_context,
        cleanup_tx,
    })
}
