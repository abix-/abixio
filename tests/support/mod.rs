use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use abixio::admin::HealStats;
use abixio::admin::handlers::{AdminConfig, AdminHandler};
use abixio::cluster::{ClusterConfig, ClusterVolumeStatus, ClusterManager, ClusterNodeStatus, ServiceState};
use abixio::cluster::placement::PlacementVolume;
use abixio::cluster::topology::{StaticTopology, TopologyVolume, TopologyNode};
use abixio::heal::mrf::MrfQueue;
use abixio::s3::auth::AuthConfig;
use abixio::s3::handlers::S3Handler;
use abixio::storage::local_volume::LocalVolume;
use abixio::storage::volume_pool::VolumePool;
use abixio::storage::metadata::{BucketSettings, ObjectMeta};
use abixio::storage::{Backend, BackendInfo, StorageError};
use tempfile::TempDir;

#[derive(Clone)]
struct AvailabilityController {
    inner: Arc<RwLock<HashMap<String, bool>>>,
}

impl AvailabilityController {
    fn new(node_ids: &[String]) -> Self {
        Self {
            inner: Arc::new(RwLock::new(
                node_ids.iter().cloned().map(|id| (id, true)).collect(),
            )),
        }
    }

    fn set_node_up(&self, node_id: &str, up: bool) {
        self.inner.write().unwrap().insert(node_id.to_string(), up);
    }

    fn is_node_up(&self, node_id: &str) -> bool {
        self.inner.read().unwrap().get(node_id).copied().unwrap_or(true)
    }
}

struct ControlledBackend {
    inner: LocalVolume,
    owner_node_id: String,
    controller: AvailabilityController,
}

impl ControlledBackend {
    fn new(root: &Path, owner_node_id: impl Into<String>, controller: AvailabilityController) -> Self {
        Self {
            inner: LocalVolume::new(root).unwrap(),
            owner_node_id: owner_node_id.into(),
            controller,
        }
    }

    fn available(&self) -> bool {
        self.controller.is_node_up(&self.owner_node_id)
    }

    fn ensure_available(&self) -> Result<(), StorageError> {
        if self.available() {
            Ok(())
        } else {
            Err(StorageError::Io(std::io::Error::new(
                std::io::ErrorKind::ConnectionRefused,
                format!("node {} is down", self.owner_node_id),
            )))
        }
    }
}

impl Backend for ControlledBackend {
    fn write_shard(
        &self,
        bucket: &str,
        key: &str,
        data: &[u8],
        meta: &ObjectMeta,
    ) -> Result<(), StorageError> {
        self.ensure_available()?;
        self.inner.write_shard(bucket, key, data, meta)
    }

    fn read_shard(&self, bucket: &str, key: &str) -> Result<(Vec<u8>, ObjectMeta), StorageError> {
        self.ensure_available()?;
        self.inner.read_shard(bucket, key)
    }

    fn delete_object(&self, bucket: &str, key: &str) -> Result<(), StorageError> {
        self.ensure_available()?;
        self.inner.delete_object(bucket, key)
    }

    fn list_objects(&self, bucket: &str, prefix: &str) -> Result<Vec<String>, StorageError> {
        self.ensure_available()?;
        self.inner.list_objects(bucket, prefix)
    }

    fn list_buckets(&self) -> Result<Vec<String>, StorageError> {
        self.ensure_available()?;
        self.inner.list_buckets()
    }

    fn make_bucket(&self, bucket: &str) -> Result<(), StorageError> {
        self.ensure_available()?;
        self.inner.make_bucket(bucket)
    }

    fn delete_bucket(&self, bucket: &str) -> Result<(), StorageError> {
        self.ensure_available()?;
        self.inner.delete_bucket(bucket)
    }

    fn bucket_exists(&self, bucket: &str) -> bool {
        self.available() && self.inner.bucket_exists(bucket)
    }

    fn bucket_created_at(&self, bucket: &str) -> u64 {
        if self.available() {
            self.inner.bucket_created_at(bucket)
        } else {
            0
        }
    }

    fn stat_object(&self, bucket: &str, key: &str) -> Result<ObjectMeta, StorageError> {
        self.ensure_available()?;
        self.inner.stat_object(bucket, key)
    }

    fn update_meta(&self, bucket: &str, key: &str, meta: &ObjectMeta) -> Result<(), StorageError> {
        self.ensure_available()?;
        self.inner.update_meta(bucket, key, meta)
    }

    fn read_meta_versions(&self, bucket: &str, key: &str) -> Result<Vec<ObjectMeta>, StorageError> {
        self.ensure_available()?;
        self.inner.read_meta_versions(bucket, key)
    }

    fn write_meta_versions(
        &self,
        bucket: &str,
        key: &str,
        versions: &[ObjectMeta],
    ) -> Result<(), StorageError> {
        self.ensure_available()?;
        self.inner.write_meta_versions(bucket, key, versions)
    }

    fn write_versioned_shard(
        &self,
        bucket: &str,
        key: &str,
        version_id: &str,
        data: &[u8],
        meta: &ObjectMeta,
    ) -> Result<(), StorageError> {
        self.ensure_available()?;
        self.inner.write_versioned_shard(bucket, key, version_id, data, meta)
    }

    fn read_versioned_shard(
        &self,
        bucket: &str,
        key: &str,
        version_id: &str,
    ) -> Result<(Vec<u8>, ObjectMeta), StorageError> {
        self.ensure_available()?;
        self.inner.read_versioned_shard(bucket, key, version_id)
    }

    fn delete_version_data(
        &self,
        bucket: &str,
        key: &str,
        version_id: &str,
    ) -> Result<(), StorageError> {
        self.ensure_available()?;
        self.inner.delete_version_data(bucket, key, version_id)
    }

    fn read_bucket_settings(&self, bucket: &str) -> BucketSettings {
        if self.available() {
            self.inner.read_bucket_settings(bucket)
        } else {
            BucketSettings::default()
        }
    }

    fn write_bucket_settings(
        &self,
        bucket: &str,
        settings: &BucketSettings,
    ) -> Result<(), StorageError> {
        self.ensure_available()?;
        self.inner.write_bucket_settings(bucket, settings)
    }

    fn info(&self) -> BackendInfo {
        if self.available() {
            self.inner.info()
        } else {
            BackendInfo {
                label: self.inner.root().display().to_string(),
                backend_type: "local".to_string(),
                total_bytes: None,
                used_bytes: None,
                free_bytes: None,
            }
        }
    }
}

pub struct TestNode {
    pub node_id: String,
    pub addr: SocketAddr,
    pub cluster: Arc<ClusterManager>,
    handle: tokio::task::JoinHandle<()>,
}

impl TestNode {
    pub fn endpoint(&self) -> String {
        format!("http://{}", self.addr)
    }
}

pub struct ClusterHarness {
    _tmp: TempDir,
    pub nodes: Vec<TestNode>,
    pub disk_paths: Vec<PathBuf>,
    pub placement_volumes: Vec<PlacementVolume>,
    controller: AvailabilityController,
}

impl ClusterHarness {
    pub async fn new() -> Self {
        let tmp = TempDir::new().unwrap();
        let node_ids = (1..=4).map(|index| format!("node-{}", index)).collect::<Vec<_>>();
        let controller = AvailabilityController::new(&node_ids);

        let mut disk_paths = Vec::new();
        let mut placement_volumes = Vec::new();
        for node_index in 0..4 {
            for disk_index in 0..2 {
                let root = tmp
                    .path()
                    .join(format!("node-{}-disk-{}", node_index + 1, disk_index + 1));
                std::fs::create_dir_all(&root).unwrap();
                let backend_index = disk_paths.len();
                disk_paths.push(root.clone());
                placement_volumes.push(PlacementVolume {
                    backend_index,
                    node_id: format!("node-{}", node_index + 1),
                    volume_id: format!("node-{}-vol-{}", node_index + 1, disk_index + 1),
                });
            }
        }

        let _topology = StaticTopology {
            cluster_id: "cluster-harness".to_string(),
            epoch_id: 7,
            pool_id: "cluster-set-4x2".to_string(),
            nodes: (0..4)
                .map(|node_index| TopologyNode {
                    node_id: format!("node-{}", node_index + 1),
                    advertise_s3: "http://127.0.0.1:0".to_string(),
                    advertise_cluster: "http://127.0.0.1:0".to_string(),
                    volumes: (0..2)
                        .map(|disk_index| {
                            let backend_index = node_index * 2 + disk_index;
                            TopologyVolume {
                                volume_id: format!(
                                    "node-{}-vol-{}",
                                    node_index + 1,
                                    disk_index + 1
                                ),
                                path: disk_paths[backend_index].clone(),
                            }
                        })
                        .collect(),
                })
                .collect(),
        };

        let mut nodes = Vec::new();
        for node_id in &node_ids {
            let node_index = node_id
                .trim_start_matches("node-")
                .parse::<usize>()
                .unwrap()
                - 1;
            let store = Arc::new(Self::build_store(
                &disk_paths,
                &placement_volumes,
                controller.clone(),
            ));
            let cluster = Arc::new(
                ClusterManager::new(ClusterConfig {
                    node_id: node_id.clone(),
                    advertise_s3: "http://127.0.0.1:0".to_string(),
                    advertise_cluster: "http://127.0.0.1:0".to_string(),
                    nodes: Vec::new(),
                    access_key: String::new(),
                    secret_key: String::new(),
                    no_auth: true,
                    disk_paths: vec![
                        disk_paths[node_index * 2].clone(),
                        disk_paths[node_index * 2 + 1].clone(),
                    ],
                })
                .unwrap(),
            );
            let (addr, handle) = start_server(node_id, Arc::clone(&store), Arc::clone(&cluster)).await;
            nodes.push(TestNode {
                node_id: node_id.clone(),
                addr,
                cluster,
                handle,
            });
        }

        let harness = Self {
            _tmp: tmp,
            nodes,
            disk_paths,
            placement_volumes,
            controller,
        };
        harness.sync_topology();
        harness
    }

    fn build_store(
        disk_paths: &[PathBuf],
        placement_volumes: &[PlacementVolume],
        controller: AvailabilityController,
    ) -> VolumePool {
        let backends = placement_volumes
            .iter()
            .map(|disk| {
                Box::new(ControlledBackend::new(
                    &disk_paths[disk.backend_index],
                    disk.node_id.clone(),
                    controller.clone(),
                )) as Box<dyn Backend>
            })
            .collect::<Vec<_>>();
        let mut set = VolumePool::new(backends).unwrap();
        set.set_mrf(Arc::new(MrfQueue::new(1000)));
        set.set_placement_topology(7, "cluster-set-4x2", placement_volumes.to_vec())
            .unwrap();
        set
    }

    pub fn sync_topology(&self) {
        let nodes = self
            .nodes
            .iter()
            .map(|node| ClusterNodeStatus {
                node_id: node.node_id.clone(),
                advertise_s3: node.endpoint(),
                advertise_cluster: node.endpoint(),
                state: if self.controller.is_node_up(&node.node_id) {
                    ServiceState::Ready
                } else {
                    ServiceState::Fenced
                },
                voter: true,
                reachable: self.controller.is_node_up(&node.node_id),
                total_disks: 2,
                last_heartbeat_unix_secs: 0,
            })
            .collect::<Vec<_>>();
        let disks = self
            .placement_volumes
            .iter()
            .map(|disk| ClusterVolumeStatus {
                volume_id: disk.volume_id.clone(),
                node_id: disk.node_id.clone(),
                path: self.disk_paths[disk.backend_index].display().to_string(),
            })
            .collect::<Vec<_>>();
        for node in &self.nodes {
            node.cluster
                .install_topology(7, "node-1", nodes.clone(), disks.clone());
            if !self.controller.is_node_up(&node.node_id) {
                node.cluster.force_fence("node stopped");
            }
        }
    }

    pub fn stop_node(&mut self, index: usize) {
        let node = &self.nodes[index];
        self.controller.set_node_up(&node.node_id, false);
        node.cluster.force_fence("node stopped");
        node.handle.abort();
        self.sync_topology();
    }

    pub fn fence_node(&self, index: usize, reason: &str) {
        self.nodes[index].cluster.force_fence(reason);
    }

    pub fn shard_root(&self, volume_id: &str) -> PathBuf {
        let disk = self
            .placement_volumes
            .iter()
            .find(|disk| disk.volume_id == volume_id)
            .unwrap();
        self.disk_paths[disk.backend_index].clone()
    }
}

async fn start_server(
    node_id: &str,
    store: Arc<VolumePool>,
    cluster: Arc<ClusterManager>,
) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let heal_disks: Arc<Vec<Box<dyn Backend>>> = Arc::new(Vec::new());
    let heal_stats = Arc::new(HealStats::new());
    let admin = Arc::new(AdminHandler::new(
        Arc::clone(&store),
        heal_disks,
        Arc::new(MrfQueue::new(1000)),
        heal_stats,
        AdminConfig {
            listen: ":0".to_string(),
            total_disks: 8,
            auth_enabled: false,
            scan_interval: "10m".to_string(),
            heal_interval: "24h".to_string(),
            mrf_workers: 2,
            node_id: node_id.to_string(),
            advertise_s3: "http://127.0.0.1:0".to_string(),
            advertise_cluster: "http://127.0.0.1:0".to_string(),
            node_count: 3,
        },
        Arc::clone(&cluster),
    ));
    let mut handler = S3Handler::new(
        store,
        AuthConfig {
            access_key: String::new(),
            secret_key: String::new(),
            no_auth: true,
        },
        cluster,
    );
    handler.set_admin(admin);
    let handler = Arc::new(handler);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let io = hyper_util::rt::TokioIo::new(stream);
            let handler = handler.clone();
            tokio::spawn(async move {
                let service = hyper::service::service_fn(move |req| {
                    let handler = handler.clone();
                    async move { Ok::<_, hyper::Error>(handler.dispatch(req).await) }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(io, service)
                    .await;
            });
        }
    });
    (addr, handle)
}
