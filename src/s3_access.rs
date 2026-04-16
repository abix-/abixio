use std::sync::Arc;

use s3s::S3Result;
use s3s::access::{S3Access, S3AccessContext};
use s3s::s3_error;

use crate::cluster::ClusterManager;
use crate::policy::{op_to_action, Args, Policy};
use crate::storage::Store;
use crate::storage::volume_pool::VolumePool;

pub struct AbixioAccess {
    cluster: Arc<ClusterManager>,
    store: Arc<VolumePool>,
    /// Root access key; requests from this key evaluate as bucket owner
    /// and are allowed by default unless an explicit Deny matches.
    owner_access_key: String,
}

impl AbixioAccess {
    pub fn new(
        cluster: Arc<ClusterManager>,
        store: Arc<VolumePool>,
        owner_access_key: String,
    ) -> Self {
        Self { cluster, store, owner_access_key }
    }
}

#[async_trait::async_trait]
impl S3Access for AbixioAccess {
    async fn check(&self, cx: &mut S3AccessContext<'_>) -> S3Result<()> {
        if self.cluster.blocks_data_plane() {
            return Err(s3_error!(ServiceUnavailable));
        }

        let path = cx.s3_path();
        let (bucket_opt, key_opt) = match path.as_object() {
            Some((b, k)) => (Some(b), Some(k)),
            None => (path.as_bucket(), None),
        };
        let Some(bucket) = bucket_opt else {
            // Bucketless ops (ListBuckets, root) aren't gated by bucket policy.
            return Ok(());
        };

        let creds = cx.credentials();
        let access_key = creds.map(|c| c.access_key.as_str()).unwrap_or("");
        // Empty owner_access_key means "no auth configured at server
        // startup" -- treat every caller as owner so bucket policy
        // doesn't fence out anonymous test harnesses. When an owner
        // is explicitly configured, match exact (and only non-empty)
        // access keys against it.
        let is_owner = if self.owner_access_key.is_empty() {
            true
        } else {
            !access_key.is_empty() && access_key == self.owner_access_key
        };

        let settings = match self.store.get_bucket_settings(bucket).await {
            Ok(s) => s,
            Err(_) => {
                // Bucket missing: SigV4 handler will surface the right error.
                return Ok(());
            }
        };
        let policy_json = match settings.policy {
            Some(v) if !v.is_null() => v,
            _ => {
                // No policy attached. Behave as before: only the
                // authenticated owner may proceed unless the caller is
                // anonymous, in which case the existing auth layer has
                // already rejected them when auth is required.
                return Ok(());
            }
        };

        let Some(policy) = Policy::from_json(&policy_json) else {
            // Unparseable policy: fail closed -- safer than ignoring.
            return Err(s3_error!(AccessDenied, "bucket policy unparseable"));
        };

        let action = op_to_action(cx.s3_op().name());
        let args = Args {
            bucket,
            key: key_opt,
            action,
            access_key,
            is_owner,
        };
        if policy.is_allowed(&args) {
            Ok(())
        } else {
            Err(s3_error!(AccessDenied))
        }
    }
}
