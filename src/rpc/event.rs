use crate::protos::stratsync::*;
use crate::service::{
    MAX_CONNECTIONS_PER_ANONYMOUS, MAX_CONNECTIONS_PER_USER, MAX_PEERS_PER_STRATEGY,
};
use crate::types::*;
use crate::utils;

use sqlx::types::Uuid;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};

impl StratSyncService {
    pub async fn rpc_event(
        &self,
        request: Request<SubscriptionRequest>,
    ) -> Result<Response<ReceiverStream<Result<EventResponse, Status>>>, Status> {
        let metadata = request.metadata().to_owned();
        let payload = request.into_inner();

        let strategy_id =
            utils::parse_string_to_uuid(&payload.strategy, "Strategy id has an invalid format")?;

        tracing::info!("Event connection attempt for strategy {}", strategy_id);

        let row = utils::with_db_timeout(
            sqlx::query!(
                r#"SELECT raid, author, is_public
                     FROM public.strategies
                    WHERE id = $1"#,
                strategy_id
            )
            .fetch_one(&self.pool),
        )
        .await
        .map_err(|e| {
            tracing::warn!("Failed to fetch strategy {}: {:?}", strategy_id, e);
            Status::permission_denied("Access denied to strategy")
        })?;

        let raid_id = row.raid;
        tracing::debug!("Strategy {} associated with raid {}", strategy_id, raid_id);

        let user_id = utils::parse_authorization_header(&metadata)?;
        let is_author = user_id.map(|uid| Some(uid) == row.author).unwrap_or(false);

        // Check connection limits
        let user_connections = self
            .peer_context
            .iter()
            .filter(|(_, ctx)| ctx.user_id == user_id)
            .count();
        let user_conn_limit = if user_id.is_some() {
            MAX_CONNECTIONS_PER_USER
        } else {
            MAX_CONNECTIONS_PER_ANONYMOUS
        };

        if user_connections >= user_conn_limit {
            tracing::warn!(
                "Connection limit exceeded for user {:?}: {} connections, limit {}",
                user_id,
                user_connections,
                user_conn_limit
            );
            return Err(Status::resource_exhausted(
                "Too many connections for this user",
            ));
        }

        tracing::debug!(
            "User {:?} has {} active connections",
            user_id,
            user_connections
        );

        if !row.is_public && !is_author {
            tracing::warn!(
                "Unauthorized access attempt to private strategy {} by user {:?}",
                strategy_id,
                user_id
            );
            return Err(Status::permission_denied("Access denied to strategy"));
        }

        if !self.raid_cache.contains_key(&raid_id) {
            tracing::debug!("Loading raid {} data into cache", raid_id);
            let (damages, row) = utils::with_db_timeout(async {
                tokio::try_join!(
                    sqlx::query_as!(
                        Damage,
                        r#"SELECT d.id, max_shared, num_targets
                             FROM public.damages AS d
                                  JOIN public.gimmicks AS g
                                  ON d.gimmick = g.id
                            WHERE g.raid = $1"#,
                        raid_id
                    )
                    .fetch_all(&self.pool),
                    sqlx::query!(
                        r#"SELECT duration, headcount
                             FROM public.raids
                            WHERE id = $1"#,
                        raid_id
                    )
                    .fetch_one(&self.pool),
                )
            })
            .await?;

            self.raid_cache.insert(
                raid_id,
                Arc::new(RaidInfo {
                    duration: row.duration,
                    headcount: row.headcount,
                    damages,
                }),
            );
            tracing::info!("Loaded raid {} data into cache", raid_id);
        }

        let lock = if let Some(lock) = self.strategy_lock.get(&strategy_id) {
            lock
        } else {
            let lock = Arc::new(Mutex::new(()));
            self.strategy_lock.insert(strategy_id, lock.clone());
            lock
        };
        let _guard = lock.lock().await;

        let token = Uuid::new_v4().to_string();

        let peers: Vec<String> = match self.strategy_context.get(&strategy_id) {
            Some(strategy_context) => {
                // Check peer limit
                if strategy_context.peers.len() >= MAX_PEERS_PER_STRATEGY {
                    tracing::warn!(
                        "Peer limit exceeded for strategy {}: {} peers",
                        strategy_id,
                        strategy_context.peers.len()
                    );
                    return Err(Status::resource_exhausted(
                        "Strategy has reached maximum number of peers",
                    ));
                }
                strategy_context
                    .peers
                    .iter()
                    .chain([token.clone()].iter())
                    .map(|el| el.to_owned())
                    .collect()
            }
            None => vec![token.clone()],
        };

        let elevated_peers: Vec<String> = match self.strategy_context.get(&strategy_id) {
            Some(strategy_context) => strategy_context.elevated_peers.clone(),
            None => vec![],
        }
        .iter()
        .chain(
            if is_author {
                vec![token.clone()]
            } else {
                vec![]
            }
            .iter(),
        )
        .map(|el| el.to_owned())
        .collect();

        let players: Vec<Player>;
        let damage_options: Vec<DamageOption>;
        let entries: Vec<Entry>;
        if peers.len() > 1 {
            let strategy_ctx = self
                .strategy_context
                .get(&strategy_id)
                .ok_or_else(|| Status::internal("Strategy context not found"))?;
            let mut strategy_context = (*strategy_ctx).to_owned();
            let old_version = strategy_context.version;
            strategy_context.version += 1;
            strategy_context.peers = peers;
            strategy_context.elevated_peers = elevated_peers;

            players = strategy_context.players.clone();
            damage_options = strategy_context.damage_options.clone();
            entries = strategy_context.entries.clone();

            self.strategy_context
                .insert(strategy_id, Arc::new(strategy_context));

            tracing::info!(
                "Peer joined existing strategy {}, version {} -> {}, {} total peers",
                strategy_id,
                old_version,
                old_version + 1,
                players.len()
            );
        } else {
            tracing::info!("Loading initial data for strategy {}", strategy_id);
            let damage_options_raw: Vec<_>;
            (players, damage_options_raw, entries) = utils::with_db_timeout(async {
                tokio::try_join!(
                    sqlx::query_as!(
                        Player,
                        r#"  WITH ordered_table AS (SELECT *
                                                    FROM public.strategy_players
                                                    ORDER BY "order")
                           SELECT id, job AS "job: String", "order"
                             FROM ordered_table
                            WHERE strategy = $1"#,
                        strategy_id
                    )
                    .fetch_all(&self.pool),
                    sqlx::query!(
                        r#"SELECT damage, num_shared, primary_target
                             FROM public.strategy_damage_options
                            WHERE strategy = $1"#,
                        strategy_id
                    )
                    .fetch_all(&self.pool),
                    sqlx::query_as!(
                        Entry,
                        r#"SELECT e.id AS id, player, action, use_at
                             FROM public.strategy_player_entries AS e
                                  JOIN public.strategy_players AS p
                                  ON e.player = p.id
                            WHERE p.strategy = $1"#,
                        strategy_id
                    )
                    .fetch_all(&self.pool),
                )
            })
            .await?;

            damage_options = damage_options_raw
                .iter()
                .map(|record| DamageOption {
                    damage: record.damage.to_string(),
                    num_shared: record.num_shared,
                    primary_target: record.primary_target.map(|s| s.to_string()),
                })
                .collect();

            self.strategy_context.insert(
                strategy_id,
                Arc::new(StrategyContext {
                    version: 1,
                    raid_id,
                    peers,
                    elevated_peers,
                    players: players.clone(),
                    damage_options: damage_options.clone(),
                    entries: entries.clone(),
                }),
            );

            tracing::info!(
                "Created new strategy context {} with {} players, {} entries",
                strategy_id,
                players.len(),
                entries.len()
            );
        }

        let (tx, rx) = mpsc::channel(32);
        self.peer_context.insert(
            token.clone(),
            Arc::new(PeerContext {
                strategy_id,
                raid_id,
                user_id,
                is_author,
                tx: tx.clone(),
            }),
        );

        tx.send(Ok(EventResponse {
            event: Some(event_response::Event::InitializationEvent(
                InitializationEvent {
                    token: token.clone(),
                    players,
                    damage_options,
                    entries,
                },
            )),
        }))
        .await
        .map_err(|_| {
            tracing::error!(
                "Failed to send initialization event for strategy {}",
                strategy_id
            );
            Status::internal("Failed to send initialization event")
        })?;

        tracing::info!(
            "Connection established for strategy {}, token: {}, user: {:?}, author: {}",
            strategy_id,
            token,
            user_id,
            is_author
        );

        Ok(Response::new(ReceiverStream::new(rx)))
    }
}
