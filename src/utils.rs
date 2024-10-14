use std::sync::Arc;

use dotenvy_macro::dotenv;
use jsonwebtoken::{decode, Algorithm, DecodingKey, Validation};
use serde::{Deserialize, Serialize};
use sqlx::types::Uuid;
use tokio::{sync::Mutex, task::JoinSet};
use tonic::{metadata::MetadataMap, Status};

use crate::{
    protos::stratsync::{event_response, EventResponse},
    types::*,
};

#[derive(Debug, Serialize, Deserialize)]
struct Claims {
    pub aud: String,
    pub exp: usize,
    pub iat: usize,
    pub iss: String,
    pub sub: String,
}

pub fn parse_string_to_uuid(id: &str, message: impl Into<String>) -> Result<Uuid, Status> {
    Uuid::parse_str(id).or(Err(Status::invalid_argument(message)))
}

pub fn parse_authorization_header(metadata: &MetadataMap) -> Result<Option<Uuid>, Status> {
    if let Some(authorization) = metadata.get("authorization") {
        let authorization_as_string = authorization.to_str().or(Err(Status::invalid_argument(
            "Invalid authorization header",
        )))?;

        let (token_type, token) = authorization_as_string
            .split_once(" ")
            .ok_or(Status::invalid_argument("Invalid authorization header"))?;

        if token_type != "Bearer" {
            return Err(Status::invalid_argument("Invalid token type"));
        }

        let mut validation = Validation::new(Algorithm::HS256);
        validation.set_audience(&["authenticated"]);

        let claims = decode::<Claims>(
            token,
            &DecodingKey::from_secret(dotenv!("JWT_SECRET").as_ref()),
            &validation,
        )
        .or(Err(Status::invalid_argument("Invalid token")))?
        .claims;

        let user_id = parse_string_to_uuid(&claims.sub, "sub has an invalid format")?;

        Ok(Some(user_id))
    } else {
        Ok(None)
    }
}

type InitializationContext = (Arc<PeerContext>, Arc<StrategyContext>, Arc<Mutex<()>>);

impl StratSyncService {
    pub fn open_strategy(
        &self,
        token: &String,
        check_elevated: bool,
    ) -> Result<InitializationContext, Status> {
        let peer_context = self
            .peer_context
            .get(token)
            .ok_or(Status::unauthenticated("Token not found"))?;

        let strategy_context = self
            .strategy_context
            .get(&peer_context.strategy_id)
            .ok_or(Status::unauthenticated("Strategy not opened"))?;

        if check_elevated && !strategy_context.elevated_peers.iter().any(|s| s == token) {
            return Err(Status::permission_denied("Not elevated"));
        }

        let lock = self.strategy_lock.get(&peer_context.strategy_id).unwrap();

        Ok((peer_context, strategy_context, lock))
    }

    pub async fn broadcast(
        &self,
        token: &String,
        strategy_context: &Arc<StrategyContext>,
        event: event_response::Event,
    ) {
        let mut tasks = JoinSet::new();

        for peer in &strategy_context.peers {
            if token == peer {
                continue;
            }

            let tx = self.peer_context.get(peer).unwrap().tx.clone();
            let event = event.clone();

            if tx.is_closed() {
                self.peer_context.invalidate(peer);
                continue;
            }

            tasks.spawn(async move {
                tx.send(Ok(EventResponse { event: Some(event) }))
                    .await
                    .unwrap()
            });
        }

        while (tasks.join_next().await).is_some() {}
    }
}
