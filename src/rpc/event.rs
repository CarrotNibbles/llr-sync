use crate::protos::stratsync::*;
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

        let row = sqlx::query!(
            r#"SELECT raid, author, is_public
                 FROM public.strategies
                WHERE id = $1"#,
            strategy_id
        )
        .fetch_one(&self.pool)
        .await
        .or(Err(Status::permission_denied("Access denied to strategy")))?;

        let raid_id = row.raid;

        let is_author = utils::get_user_id_from_authorization(&metadata)?
            .map(|user_id| user_id == row.author)
            .unwrap_or(false);

        if !row.is_public && !is_author {
            return Err(Status::permission_denied("Access denied to strategy"));
        }

        if !self.raid_cache.contains_key(&raid_id) {
            let (damages, row) = tokio::try_join!(
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
            .unwrap();

            self.raid_cache.insert(
                raid_id,
                Arc::new(RaidInfo {
                    duration: row.duration,
                    headcount: row.headcount,
                    damages,
                }),
            );
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

        let peers: Vec<String> =
            if let Some(strategy_context) = self.strategy_context.get(&strategy_id) {
                strategy_context
                    .peers
                    .iter()
                    .chain([token.clone()].iter())
                    .map(|el| el.to_owned())
                    .collect()
            } else {
                vec![token.clone()]
            };

        let elevated_peers: Vec<String> =
            if let Some(strategy_context) = self.strategy_context.get(&strategy_id) {
                strategy_context.elevated_peers.clone()
            } else {
                vec![]
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
            let mut strategy_context =
                (*self.strategy_context.get(&strategy_id).unwrap()).to_owned();
            strategy_context.peers = peers;
            strategy_context.elevated_peers = elevated_peers;

            players = strategy_context.players.clone();
            damage_options = strategy_context.damage_options.clone();
            entries = strategy_context.entries.clone();

            self.strategy_context
                .insert(strategy_id, Arc::new(strategy_context))
        } else {
            let damage_options_raw: Vec<_>;
            (players, damage_options_raw, entries) = tokio::try_join!(
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
            .unwrap();

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
                    raid_id,
                    peers,
                    elevated_peers,
                    players: players.clone(),
                    damage_options: damage_options.clone(),
                    entries: entries.clone(),
                }),
            );
        }

        let (tx, rx) = mpsc::channel(32);
        self.peer_context.insert(
            token.clone(),
            Arc::new(PeerContext {
                strategy_id,
                raid_id,
                is_author,
                tx: tx.clone(),
            }),
        );

        tx.send(Ok(EventResponse {
            event: Some(event_response::Event::InitializationEvent(
                InitializationEvent {
                    token,
                    players,
                    damage_options,
                    entries,
                },
            )),
        }))
        .await
        .unwrap();

        Ok(Response::new(ReceiverStream::new(rx)))
    }
}
