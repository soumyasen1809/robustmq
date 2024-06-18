use clients::poll::ClientPool;
// Copyright 2023 RobustMQ Team
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
use common_base::{
    config::broker_mqtt::{broker_mqtt_conf, BrokerMQTTConfig},
    log::info,
    runtime::create_runtime,
};
use core::metadata_cache::{load_metadata_cache, MetadataCacheManager};
use core::{
    client_keep_alive::ClientKeepAlive, heartbeat_cache::HeartbeatCache,
    session_expiry::SessionExpiry, HEART_CONNECT_SHARD_HASH_NUM,
};
use metadata_struct::mqtt::{cluster::MQTTCluster, user::MQTTUser};
use qos::{ack_manager::AckManager, memory::QosMemory};
use server::{
    grpc::server::GrpcServer,
    http::server::{start_http_server, HttpServerState},
    start_mqtt_server,
    tcp::packet::{RequestPackage, ResponsePackage},
};
use std::{sync::Arc, time::Duration};
use storage::{cluster::ClusterStorage, user::UserStorage};
use storage_adapter::{
    // memory::MemoryStorageAdapter,
    mysql::{build_mysql_conn_pool, MySQLStorageAdapter},
    // placement::PlacementStorageAdapter,
    storage::StorageAdapter,
};
use subscribe::{
    sub_exclusive::SubscribeExclusive, sub_share_follower::SubscribeShareFollower,
    sub_share_leader::SubscribeShareLeader, subscribe_cache::SubscribeCache,
};
use tokio::{
    runtime::Runtime,
    signal,
    sync::broadcast::{self, Sender},
    time,
};

mod core;
mod handler;
mod metrics;
mod qos;
mod security;
mod server;
mod storage;
mod subscribe;

pub fn start_mqtt_broker_server(stop_send: broadcast::Sender<bool>) {
    let conf = broker_mqtt_conf();
    let client_poll: Arc<ClientPool> = Arc::new(ClientPool::new(100));

    // build message storage driver
    let pool = build_mysql_conn_pool(&conf.mysql.server).unwrap();
    let message_storage_adapter = Arc::new(MySQLStorageAdapter::new(pool.clone()));

    let metadata_cache = Arc::new(MetadataCacheManager::new(conf.cluster_name.clone()));
    let server = MqttBroker::new(client_poll, message_storage_adapter, metadata_cache);
    server.start(stop_send)
}

pub struct MqttBroker<'a, S> {
    conf: &'a BrokerMQTTConfig,
    metadata_cache_manager: Arc<MetadataCacheManager>,
    heartbeat_manager: Arc<HeartbeatCache>,
    idempotent_manager: Arc<QosMemory>,
    runtime: Runtime,
    request_queue_sx4: Sender<RequestPackage>,
    request_queue_sx5: Sender<RequestPackage>,
    response_queue_sx4: Sender<ResponsePackage>,
    response_queue_sx5: Sender<ResponsePackage>,
    client_poll: Arc<ClientPool>,
    message_storage_adapter: Arc<S>,
    subscribe_manager: Arc<SubscribeCache>,
    ack_manager: Arc<AckManager>,
}

impl<'a, S> MqttBroker<'a, S>
where
    S: StorageAdapter + Sync + Send + 'static + Clone,
{
    pub fn new(
        client_poll: Arc<ClientPool>,
        message_storage_adapter: Arc<S>,
        metadata_cache: Arc<MetadataCacheManager>,
    ) -> Self {
        let conf = broker_mqtt_conf();
        let runtime = create_runtime("storage-engine-server-runtime", conf.runtime.worker_threads);

        let (request_queue_sx4, _) = broadcast::channel(1000);
        let (request_queue_sx5, _) = broadcast::channel(1000);
        let (response_queue_sx4, _) = broadcast::channel(1000);
        let (response_queue_sx5, _) = broadcast::channel(1000);

        let heartbeat_manager = Arc::new(HeartbeatCache::new(HEART_CONNECT_SHARD_HASH_NUM));

        let idempotent_manager: Arc<QosMemory> = Arc::new(QosMemory::new());
        let ack_manager: Arc<AckManager> = Arc::new(AckManager::new());
        let subscribe_manager = Arc::new(SubscribeCache::new(
            metadata_cache.clone(),
            client_poll.clone(),
        ));

        return MqttBroker {
            conf,
            runtime,
            metadata_cache_manager: metadata_cache,
            heartbeat_manager,
            idempotent_manager,
            request_queue_sx4,
            request_queue_sx5,
            response_queue_sx4,
            response_queue_sx5,
            client_poll,
            message_storage_adapter,
            subscribe_manager,
            ack_manager,
        };
    }

    pub fn start(&self, stop_send: broadcast::Sender<bool>) {
        self.register_node();
        self.start_grpc_server();
        self.start_mqtt_server();
        self.start_http_server();
        self.start_keep_alive_thread(stop_send.subscribe());
        self.start_session_expiry_thread();
        self.start_cluster_heartbeat_report(stop_send.subscribe());
        self.start_push_server();
        self.awaiting_stop(stop_send);
    }

    fn start_mqtt_server(&self) {
        let cache = self.metadata_cache_manager.clone();
        let heartbeat_manager = self.heartbeat_manager.clone();
        let message_storage_adapter = self.message_storage_adapter.clone();
        let idempotent_manager = self.idempotent_manager.clone();
        let subscribe_manager = self.subscribe_manager.clone();
        let ack_manager = self.ack_manager.clone();
        let client_poll = self.client_poll.clone();

        let request_queue_sx4 = self.request_queue_sx4.clone();
        let request_queue_sx5 = self.request_queue_sx5.clone();

        let response_queue_sx4 = self.response_queue_sx4.clone();
        let response_queue_sx5 = self.response_queue_sx5.clone();
        self.runtime.spawn(async move {
            start_mqtt_server(
                subscribe_manager,
                cache,
                heartbeat_manager,
                message_storage_adapter,
                idempotent_manager,
                ack_manager,
                client_poll,
                request_queue_sx4,
                request_queue_sx5,
                response_queue_sx4,
                response_queue_sx5,
            )
            .await
        });
    }

    fn start_grpc_server(&self) {
        let server = GrpcServer::new(
            self.conf.grpc_port.clone(),
            self.metadata_cache_manager.clone(),
            self.client_poll.clone(),
        );
        self.runtime.spawn(async move {
            server.start().await;
        });
    }

    fn start_http_server(&self) {
        let http_state = HttpServerState::new(
            self.metadata_cache_manager.clone(),
            self.heartbeat_manager.clone(),
            self.response_queue_sx4.clone(),
            self.response_queue_sx5.clone(),
            self.subscribe_manager.clone(),
            self.idempotent_manager.clone(),
        );
        self.runtime
            .spawn(async move { start_http_server(http_state).await });
    }

    fn start_cluster_heartbeat_report(&self, mut stop_send: broadcast::Receiver<bool>) {
        let client_poll = self.client_poll.clone();
        self.runtime.spawn(async move {
            time::sleep(Duration::from_millis(5000)).await;
            let cluster_storage = ClusterStorage::new(client_poll);
            loop {
                match stop_send.try_recv() {
                    Ok(flag) => {
                        if flag {
                            info("ReportClusterHeartbeat thread stopped successfully".to_string());
                            break;
                        }
                    }
                    Err(_) => {}
                }
                cluster_storage.heartbeat().await;
                time::sleep(Duration::from_millis(1000)).await;
            }
        });
    }

    fn start_push_server(&self) {
        let subscribe_manager = self.subscribe_manager.clone();
        self.runtime.spawn(async move {
            subscribe_manager.start().await;
        });

        let exclusive_sub = SubscribeExclusive::new(
            self.message_storage_adapter.clone(),
            self.metadata_cache_manager.clone(),
            self.response_queue_sx4.clone(),
            self.response_queue_sx5.clone(),
            self.subscribe_manager.clone(),
            self.ack_manager.clone(),
        );

        self.runtime.spawn(async move {
            exclusive_sub.start().await;
        });

        let leader_sub = SubscribeShareLeader::new(
            self.subscribe_manager.clone(),
            self.message_storage_adapter.clone(),
            self.response_queue_sx4.clone(),
            self.response_queue_sx5.clone(),
            self.metadata_cache_manager.clone(),
            self.ack_manager.clone(),
        );

        self.runtime.spawn(async move {
            leader_sub.start().await;
        });

        let follower_sub = SubscribeShareFollower::new(
            self.subscribe_manager.clone(),
            self.response_queue_sx4.clone(),
            self.response_queue_sx5.clone(),
            self.metadata_cache_manager.clone(),
            self.client_poll.clone(),
            self.ack_manager.clone(),
        );

        self.runtime.spawn(async move {
            follower_sub.start().await;
        });
    }

    fn start_keep_alive_thread(&self, stop_send: broadcast::Receiver<bool>) {
        let mut keep_alive = ClientKeepAlive::new(
            HEART_CONNECT_SHARD_HASH_NUM,
            self.heartbeat_manager.clone(),
            self.request_queue_sx4.clone(),
            self.request_queue_sx5.clone(),
            stop_send,
        );
        self.runtime.spawn(async move {
            keep_alive.start_heartbeat_check().await;
        });
    }

    fn start_session_expiry_thread(&self) {
        let sesssion_expiry = SessionExpiry::new();
        self.runtime.spawn(async move {
            sesssion_expiry.start_session_expire_check().await;
        });
    }

    pub fn awaiting_stop(&self, stop_send: broadcast::Sender<bool>) {
        // Wait for the stop signal
        self.runtime.block_on(async move {
            loop {
                signal::ctrl_c().await.expect("failed to listen for event");
                match stop_send.send(true) {
                    Ok(_) => {
                        info("When ctrl + c is received, the service starts to stop".to_string());
                        self.stop_server().await;
                        break;
                    }
                    Err(_) => {
                        break;
                    }
                }
            }
        });

        // todo tokio runtime shutdown
    }

    fn register_node(&self) {
        let metadata_cache = self.metadata_cache_manager.clone();
        let client_poll = self.client_poll.clone();
        self.runtime.block_on(async move {
            // init system user
            let conf = broker_mqtt_conf();
            let system_user_info = MQTTUser {
                username: conf.system.system_user.clone(),
                password: conf.system.system_password.clone(),
                is_superuser: true,
            };
            let user_storage = UserStorage::new(client_poll.clone());
            match user_storage.save_user(system_user_info.clone()).await {
                Ok(_) => {
                    metadata_cache.add_user(system_user_info);
                }
                Err(e) => {
                    panic!("{}", e.to_string());
                }
            }

            // metadata_cache.init_metadata_data(load_metadata_cache(metadata_storage_adapter).await);
            let (cluster, user_info, topic_info) = load_metadata_cache(client_poll.clone()).await;
            metadata_cache.set_cluster_info(MQTTCluster::new());

            for (_, user) in user_info {
                metadata_cache.add_user(user);
            }

            for (topic_name, topic) in topic_info {
                metadata_cache.add_topic(&topic_name, &topic);
            }
            let cluster_storage = ClusterStorage::new(client_poll.clone());
            cluster_storage.register_node().await;
        });
    }

    async fn stop_server(&self) {
        // unregister node
        let cluster_storage = ClusterStorage::new(self.client_poll.clone());
        cluster_storage.unregister_node().await;
    }
}
