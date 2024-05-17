use std::any::Any;

use yellowstone_grpc_proto::geyser::SubscribeUpdate;

pub trait GrpcConsumer: Any + Send + Sync + std::fmt::Debug {
    fn consume(&self, message: &SubscribeUpdate) -> Result<(), String>;
}
