use std::{future::Future, net::SocketAddr, sync::Arc, time::Duration};

use anyhow::Result;
use clippy_utilities::{Cast, OverflowArithmetic};
use curp::{members::ClusterMember, server::Rpc, ProtocolServer, SnapshotAllocator};
use event_listener::Event;
use jsonwebtoken::{DecodingKey, EncodingKey};
use tokio::net::TcpListener;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::Server;
use utils::config::{ClientTimeout, CurpConfig, ServerTimeout, StorageConfig};

use super::{
    auth_server::AuthServer,
    barriers::{IdBarrier, IndexBarrier},
    command::{Command, CommandExecutor},
    kv_server::KvServer,
    lease_server::LeaseServer,
    lock_server::LockServer,
    maintenance::MaintenanceServer,
    watch_server::{WatchServer, CHANNEL_SIZE},
};
use crate::{
    header_gen::HeaderGenerator,
    id_gen::IdGenerator,
    revision_number::RevisionNumberGenerator,
    rpc::{
        AuthServer as RpcAuthServer, KvServer as RpcKvServer, LeaseServer as RpcLeaseServer,
        LockServer as RpcLockServer, MaintenanceServer as RpcMaintenanceServer,
        WatchServer as RpcWatchServer,
    },
    state::State,
    storage::{
        index::Index,
        kvwatcher::KvWatcher,
        lease_store::LeaseCollection,
        snapshot_allocator::{MemorySnapshotAllocator, RocksSnapshotAllocator},
        storage_api::StorageApi,
        AuthStore, KvStore, LeaseStore,
    },
};

/// Rpc Server of curp protocol
type CurpServer<S> = Rpc<Command, State<S>>;

/// Xline server
#[derive(Debug)]
pub struct XlineServer {
    /// cluster information
    cluster_info: Arc<ClusterMember>,
    /// is leader
    is_leader: bool,
    /// Curp server timeout
    curp_cfg: Arc<CurpConfig>,
    /// Client timeout
    client_timeout: ClientTimeout,
    /// Storage config,
    storage_cfg: StorageConfig,
    /// Server timeout
    server_timeout: ServerTimeout,
    /// Shutdown trigger
    shutdown_trigger: Arc<Event>,
}

impl XlineServer {
    /// New `XlineServer`
    ///
    /// # Panics
    ///
    /// panic when peers do not contain leader address
    #[inline]
    #[must_use]
    pub fn new(
        cluster_info: Arc<ClusterMember>,
        is_leader: bool,
        curp_config: CurpConfig,
        client_timeout: ClientTimeout,
        server_timeout: ServerTimeout,
        storage_config: StorageConfig,
    ) -> Self {
        Self {
            cluster_info,
            is_leader,
            curp_cfg: Arc::new(curp_config),
            client_timeout,
            storage_cfg: storage_config,
            server_timeout,
            shutdown_trigger: Arc::new(event_listener::Event::new()),
        }
    }

    /// Construct a `LeaseCollection`
    #[inline]
    fn construct_lease_collection(
        heartbeat_interval: Duration,
        candidate_timeout_ticks: u8,
    ) -> Arc<LeaseCollection> {
        let min_ttl = 3 * heartbeat_interval * candidate_timeout_ticks.cast() / 2;
        // Safe ceiling
        let min_ttl_secs = min_ttl
            .as_secs()
            .overflow_add((min_ttl.subsec_nanos() > 0).cast());
        Arc::new(LeaseCollection::new(min_ttl_secs.cast()))
    }

    /// Construct underlying storages, including `KvStore`, `LeaseStore`, `AuthStore`
    #[allow(clippy::type_complexity)] // it is easy to read
    #[inline]
    fn construct_underlying_storages<S: StorageApi>(
        &self,
        persistent: Arc<S>,
        lease_collection: Arc<LeaseCollection>,
        header_gen: Arc<HeaderGenerator>,
        auth_revision_gen: Arc<RevisionNumberGenerator>,
        key_pair: Option<(EncodingKey, DecodingKey)>,
    ) -> Result<(
        Arc<KvStore<S>>,
        Arc<LeaseStore<S>>,
        Arc<AuthStore<S>>,
        Arc<KvWatcher<S>>,
    )> {
        let index = Arc::new(Index::new());
        let (kv_update_tx, kv_update_rx) = tokio::sync::mpsc::channel(CHANNEL_SIZE);
        let kv_storage = Arc::new(KvStore::new(
            kv_update_tx.clone(),
            Arc::clone(&lease_collection),
            Arc::clone(&header_gen),
            Arc::clone(&persistent),
            Arc::clone(&index),
        ));
        let lease_storage = Arc::new(LeaseStore::new(
            Arc::clone(&lease_collection),
            Arc::clone(&header_gen),
            Arc::clone(&persistent),
            index,
            kv_update_tx,
            self.is_leader,
        ));
        let auth_storage = Arc::new(AuthStore::new(
            lease_collection,
            key_pair,
            header_gen,
            persistent,
            auth_revision_gen,
        ));
        let watcher = KvWatcher::new_arc(
            Arc::clone(&kv_storage),
            kv_update_rx,
            Arc::clone(&self.shutdown_trigger),
            *self.server_timeout.sync_victims_interval(),
        );
        // lease storage must recover before kv storage
        lease_storage.recover()?;
        kv_storage.recover()?;
        auth_storage.recover()?;
        Ok((kv_storage, lease_storage, auth_storage, watcher))
    }

    /// Construct a header generator
    #[inline]
    fn construct_generator(
        cluster_info: &Arc<ClusterMember>,
    ) -> (
        Arc<HeaderGenerator>,
        Arc<IdGenerator>,
        Arc<RevisionNumberGenerator>,
    ) {
        let member_id = cluster_info.gen_member_id("");
        let cluster_id = cluster_info.gen_cluster_id("");
        (
            Arc::new(HeaderGenerator::new(cluster_id, member_id)),
            Arc::new(IdGenerator::new(member_id)),
            Arc::new(RevisionNumberGenerator::default()),
        )
    }

    /// Start `XlineServer`
    ///
    /// # Errors
    ///
    /// Will return `Err` when `tonic::Server` serve return an error
    #[inline]
    pub async fn start<S: StorageApi>(
        &self,
        addr: SocketAddr,
        persistent: Arc<S>,
        key_pair: Option<(EncodingKey, DecodingKey)>,
    ) -> Result<()> {
        let (
            kv_server,
            lock_server,
            lease_server,
            auth_server,
            watch_server,
            maintenance_server,
            curp_server,
        ) = self.init_servers(persistent, key_pair).await?;
        Ok(Server::builder()
            .add_service(RpcLockServer::new(lock_server))
            .add_service(RpcKvServer::new(kv_server))
            .add_service(RpcLeaseServer::from_arc(lease_server))
            .add_service(RpcAuthServer::new(auth_server))
            .add_service(RpcWatchServer::new(watch_server))
            .add_service(RpcMaintenanceServer::new(maintenance_server))
            .add_service(ProtocolServer::new(curp_server))
            .serve(addr)
            .await?)
    }

    /// Start `XlineServer` from listeners
    ///
    /// # Errors
    ///
    /// Will return `Err` when `tonic::Server` serve return an error
    #[inline]
    pub async fn start_from_listener_shutdown<F, S: StorageApi>(
        &self,
        xline_listener: TcpListener,
        signal: F,
        persistent: Arc<S>,
        key_pair: Option<(EncodingKey, DecodingKey)>,
    ) -> Result<()>
    where
        F: Future<Output = ()>,
    {
        let (
            kv_server,
            lock_server,
            lease_server,
            auth_server,
            watch_server,
            maintenance_server,
            curp_server,
        ) = self.init_servers(persistent, key_pair).await?;
        Ok(Server::builder()
            .add_service(RpcLockServer::new(lock_server))
            .add_service(RpcKvServer::new(kv_server))
            .add_service(RpcLeaseServer::from_arc(lease_server))
            .add_service(RpcAuthServer::new(auth_server))
            .add_service(RpcWatchServer::new(watch_server))
            .add_service(RpcMaintenanceServer::new(maintenance_server))
            .add_service(ProtocolServer::new(curp_server))
            .serve_with_incoming_shutdown(TcpListenerStream::new(xline_listener), signal)
            .await?)
    }

    /// Init `KvServer`, `LockServer`, `LeaseServer`, `WatchServer` and `CurpServer`
    /// for the Xline Server.
    #[allow(clippy::type_complexity)] // it is easy to read
    async fn init_servers<S: StorageApi>(
        &self,
        persistent: Arc<S>,
        key_pair: Option<(EncodingKey, DecodingKey)>,
    ) -> Result<(
        KvServer<S>,
        LockServer,
        Arc<LeaseServer<S>>,
        AuthServer<S>,
        WatchServer<S>,
        MaintenanceServer<S>,
        CurpServer<S>,
    )> {
        let (header_gen, id_gen, auth_revision_gen) = Self::construct_generator(&self.cluster_info);
        let lease_collection = Self::construct_lease_collection(
            self.curp_cfg.heartbeat_interval,
            self.curp_cfg.candidate_timeout_ticks,
        );

        let (kv_storage, lease_storage, auth_storage, watcher) = self
            .construct_underlying_storages(
                Arc::clone(&persistent),
                lease_collection,
                Arc::clone(&header_gen),
                Arc::clone(&auth_revision_gen),
                key_pair,
            )?;

        let index_barrier = Arc::new(IndexBarrier::new());
        let id_barrier = Arc::new(IdBarrier::new());

        let state = State::new(Arc::clone(&lease_storage));
        let ce = CommandExecutor::new(
            Arc::clone(&kv_storage),
            Arc::clone(&auth_storage),
            Arc::clone(&lease_storage),
            Arc::clone(&persistent),
            Arc::clone(&index_barrier),
            Arc::clone(&id_barrier),
            header_gen.revision_arc(),
            Arc::clone(&auth_revision_gen),
        );
        let snapshot_allocator: Box<dyn SnapshotAllocator> = match self.storage_cfg {
            StorageConfig::Memory => Box::new(MemorySnapshotAllocator),
            StorageConfig::RocksDB(_) => Box::new(RocksSnapshotAllocator),
            #[allow(clippy::unimplemented)]
            _ => unimplemented!(),
        };

        let curp_server = CurpServer::new(
            Arc::clone(&self.cluster_info),
            self.is_leader,
            ce,
            snapshot_allocator,
            state,
            Arc::clone(&self.curp_cfg),
        )
        .await;

        let client = Arc::new(curp_server.inner_client(self.client_timeout).await);

        Ok((
            KvServer::new(
                kv_storage,
                Arc::clone(&auth_storage),
                index_barrier,
                id_barrier,
                *self.server_timeout.range_retry_timeout(),
                Arc::clone(&client),
                self.cluster_info.self_id().clone(),
            ),
            LockServer::new(
                Arc::clone(&client),
                Arc::clone(&id_gen),
                self.cluster_info.self_id().clone(),
                self.cluster_info.self_address().to_owned(),
            ),
            LeaseServer::new(
                lease_storage,
                Arc::clone(&auth_storage),
                Arc::clone(&client),
                id_gen,
                Arc::clone(&self.cluster_info),
            ),
            AuthServer::new(auth_storage, client, self.cluster_info.self_id().clone()),
            WatchServer::new(
                watcher,
                Arc::clone(&header_gen),
                *self.server_timeout.watch_progress_notify_interval(),
            ),
            MaintenanceServer::new(persistent, header_gen),
            curp_server,
        ))
    }
}

impl Drop for XlineServer {
    #[inline]
    fn drop(&mut self) {
        self.shutdown_trigger.notify(usize::MAX);
    }
}
