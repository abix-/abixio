use std::sync::Arc;

use s3s::S3Result;
use s3s::access::{S3Access, S3AccessContext};
use s3s::s3_error;

use crate::cluster::ClusterManager;

pub struct AbixioAccess {
    cluster: Arc<ClusterManager>,
}

impl AbixioAccess {
    pub fn new(cluster: Arc<ClusterManager>) -> Self {
        Self { cluster }
    }
}

#[async_trait::async_trait]
impl S3Access for AbixioAccess {
    async fn check(&self, _cx: &mut S3AccessContext<'_>) -> S3Result<()> {
        if self.cluster.blocks_data_plane() {
            return Err(s3_error!(ServiceUnavailable));
        }
        Ok(())
    }
}
