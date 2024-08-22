use std::sync::Arc;
use std::{collections::HashMap, time::Duration};

use cadence_macros::statsd_count;
use futures::{sink::SinkExt, stream::StreamExt};
use rand::distributions::Alphanumeric;
use rand::Rng;
use tokio::time::sleep;
use tracing::error;
use yellowstone_grpc_client::GeyserGrpcClient;
use yellowstone_grpc_proto::prelude::{
    subscribe_update::UpdateOneof, CommitmentLevel, SubscribeRequest, SubscribeRequestFilterBlocks,
    SubscribeRequestPing,
};
use yellowstone_grpc_proto::tonic::codec::CompressionEncoding;

use crate::grpc_consumer::GrpcConsumer;

pub struct GrpcGeyserImpl {
    endpoint: String,
    auth_header: Option<String>,
    consumers: Vec<Arc<dyn GrpcConsumer>>,
}

impl GrpcGeyserImpl {
    pub fn new(
        endpoint: String,
        auth_header: Option<String>,
        consumers: Vec<Arc<dyn GrpcConsumer>>,
    ) -> Self {
        let grpc_geyser = Self {
            endpoint,
            auth_header,
            consumers,
        };
        // polling with confirmed commitment to get confirmed transactions
        grpc_geyser.poll_blocks();
        grpc_geyser
    }

    fn poll_blocks(&self) {
        let endpoint = self.endpoint.clone();
        let auth_header = self.auth_header.clone();
        let consumers = self.consumers.clone();
        tokio::spawn(async move {
            loop {
                let mut grpc_tx;
                let mut grpc_rx;
                {
                    let grpc_client = GeyserGrpcClient::build_from_shared(endpoint.clone())
                        .unwrap()
                        .x_token(auth_header.clone())
                        .unwrap()
                        .connect_timeout(Duration::from_secs(10))
                        .max_decoding_message_size(50_000_000)
                        .accept_compressed(CompressionEncoding::Gzip)
                        .connect()
                        .await;

                    if let Err(e) = grpc_client {
                        error!("Error connecting to gRPC, waiting one second then retrying connect: {}", e);
                        statsd_count!("grpc_connect_error", 1);
                        sleep(Duration::from_secs(1)).await;
                        continue;
                    }
                    let mut grpc_client = grpc_client.unwrap();
                    let subscription = grpc_client
                        .subscribe_with_request(Some(get_block_subscribe_request()))
                        .await;
                    if let Err(e) = subscription {
                        error!("Error subscribing to gRPC stream, waiting one second then retrying connect: {}", e);
                        statsd_count!("grpc_subscribe_error", 1);
                        sleep(Duration::from_secs(1)).await;
                        continue;
                    }
                    (grpc_tx, grpc_rx) = subscription.unwrap();
                }
                while let Some(update) = grpc_rx.next().await {
                    match update {
                        Ok(update) => {
                            for consumer in consumers.clone() {
                                if let Err(e) = consumer.consume(&update) {
                                    error!("Error consuming update: {}", e);
                                    statsd_count!("grpc_consume_error", 1);
                                }
                            }
                            match update.update_oneof {
                                Some(UpdateOneof::Ping(_)) => {
                                    // This is necessary to keep load balancers that expect client pings alive. If your load balancer doesn't
                                    // require periodic client pings then this is unnecessary
                                    let ping = grpc_tx.send(ping()).await;
                                    if let Err(e) = ping {
                                        error!("Error sending ping: {}", e);
                                        statsd_count!("grpc_ping_error", 1);
                                        break;
                                    }
                                }
                                _ => {}
                            }
                        }
                        Err(error) => {
                            error!(
                                "error in block subscribe, resubscribing in 1 second: {error:?}"
                            );
                            statsd_count!("grpc_resubscribe", 1);
                            break;
                        }
                    }
                }
                sleep(Duration::from_secs(1)).await;
            }
        });
    }
}

fn get_block_subscribe_request() -> SubscribeRequest {
    SubscribeRequest {
        blocks: HashMap::from_iter(vec![(
            generate_random_string(20),
            SubscribeRequestFilterBlocks {
                account_include: vec![],
                include_transactions: Some(true),
                include_accounts: Some(true),
                include_entries: Some(false),
            },
        )]),
        commitment: Some(CommitmentLevel::Confirmed.into()),
        ..Default::default()
    }
}

fn generate_random_string(len: usize) -> String {
    rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(len)
        .map(char::from)
        .collect()
}

fn ping() -> SubscribeRequest {
    SubscribeRequest {
        ping: Some(SubscribeRequestPing { id: 1 }),
        ..Default::default()
    }
}
