use crate::protos::stratsync::*;
use moka::sync::Cache;
use sqlx::{types::Uuid, Pool, Postgres};
use std::sync::Arc;
use strum_macros::EnumString;
use tokio::sync::{mpsc::Sender, Mutex};
use tonic::Status;

pub const MAX_COUNTDOWN: i32 = 1800;

#[derive(Clone, Debug, PartialEq, PartialOrd, sqlx::Type, EnumString)]
#[sqlx(type_name = "job")]
pub enum Job {
    PLD,
    WAR,
    DRK,
    GNB,
    WHM,
    AST,
    SCH,
    SGE,
    MNK,
    DRG,
    NIN,
    SAM,
    RPR,
    BRD,
    MCH,
    DNC,
    BLM,
    RDM,
    SMN,
    BLU,
    LB,
    PCT,
    VPR,
}

#[derive(Debug, Clone)]
pub struct PeerContext {
    pub strategy_id: Uuid,
    pub raid_id: Uuid,
    pub is_author: bool,
    pub tx: Sender<Result<EventResponse, Status>>,
}

#[derive(Debug, Clone)]
pub struct StrategyContext {
    pub raid_id: Uuid,
    pub peers: Vec<String>,
    pub elevated_peers: Vec<String>,
    pub players: Vec<Player>,
    pub damage_options: Vec<DamageOption>,
    pub entries: Vec<Entry>,
}

#[derive(Debug, Clone)]
pub struct Damage {
    pub id: Uuid,
    pub max_shared: i32,
    pub num_targets: i32,
}

#[derive(Debug, Clone)]
pub struct RaidInfo {
    pub duration: i32,
    pub headcount: i32,
    pub damages: Vec<Damage>,
}

#[derive(Debug, Clone)]
pub struct ActionInfo {
    pub id: Uuid,
    pub cooldown: i32,
    pub charges: i32,
}

pub struct StratSyncService {
    pub pool: Pool<Postgres>,
    pub action_cache: Cache<String, Arc<Vec<ActionInfo>>>,
    pub raid_cache: Cache<Uuid, Arc<RaidInfo>>,
    pub strategy_lock: Cache<Uuid, Arc<Mutex<()>>>,
    pub strategy_context: Cache<Uuid, Arc<StrategyContext>>,
    pub peer_context: Cache<String, Arc<PeerContext>>,
}
