extern crate undermoon;

mod connection;
mod redis_client;

#[cfg(test)]
mod tests {
    use super::*;

    use arc_swap::ArcSwap;
    use connection::DummyOkConnFactory;
    use futures_timer::Delay;
    use redis_client::DummyOkClientFactory;
    use std::convert::TryFrom;
    use std::num::NonZeroUsize;
    use std::str;
    use std::sync::atomic::AtomicI64;
    use std::sync::{Arc, RwLock};
    use std::time::Duration;
    use tokio;
    use undermoon::common::cluster::ClusterName;
    use undermoon::common::proto::ProxyClusterMeta;
    use undermoon::common::response::{ERR_BACKEND_CONNECTION, ERR_CLUSTER_NOT_FOUND, OK_REPLY};
    use undermoon::common::track::TrackedFutureRegistry;
    use undermoon::common::utils::pretty_print_bytes;
    use undermoon::protocol::{Array, BulkStr, Resp, RespPacket, VFunctor};
    use undermoon::proxy::command::{new_command_pair, CmdReplyReceiver, Command};
    use undermoon::proxy::manager::MetaManager;
    use undermoon::proxy::manager::MetaMap;
    use undermoon::proxy::service::ServerProxyConfig;
    use undermoon::proxy::session::CmdCtx;

    const TEST_CLUSTER: &str = "test_cluster";

    fn gen_config() -> ServerProxyConfig {
        ServerProxyConfig {
            address: "localhost:5299".to_string(),
            announce_address: "localhost:5299".to_string(),
            auto_select_cluster: true,
            slowlog_len: NonZeroUsize::new(1024).unwrap(),
            slowlog_log_slower_than: AtomicI64::new(0),
            thread_number: NonZeroUsize::new(2).unwrap(),
            session_channel_size: 1024,
            backend_channel_size: 1024,
            backend_conn_num: NonZeroUsize::new(2).unwrap(),
            backend_batch_min_time: 10000,
            backend_batch_max_time: 10000,
            backend_batch_buf: NonZeroUsize::new(50).unwrap(),
            session_batch_min_time: 10000,
            session_batch_max_time: 10000,
            session_batch_buf: NonZeroUsize::new(50).unwrap(),
        }
    }

    fn gen_testing_manager() -> MetaManager<DummyOkClientFactory, DummyOkConnFactory> {
        let config = Arc::new(gen_config());
        let client_factory = Arc::new(DummyOkClientFactory {});
        let conn_factory = Arc::new(DummyOkConnFactory {});
        let meta_map = Arc::new(ArcSwap::new(Arc::new(MetaMap::new())));
        let future_registry = Arc::new(TrackedFutureRegistry::default());
        MetaManager::new(
            config,
            client_factory,
            conn_factory,
            meta_map,
            future_registry,
        )
    }

    fn gen_set_command() -> (CmdCtx, CmdReplyReceiver) {
        let cluster_name = Arc::new(RwLock::new(ClusterName::try_from(TEST_CLUSTER).unwrap()));
        let resp = RespPacket::Data(Resp::Arr(Array::Arr(vec![
            Resp::Bulk(BulkStr::Str(b"SET".to_vec())),
            Resp::Bulk(BulkStr::Str(b"key".to_vec())),
            Resp::Bulk(BulkStr::Str(b"value".to_vec())),
        ])));
        let command = Command::new(Box::new(resp));
        let (s, r) = new_command_pair(&command);
        let cmd_ctx = CmdCtx::new(cluster_name, command, s, 233);
        (cmd_ctx, r)
    }

    fn gen_proxy_cluster_meta() -> ProxyClusterMeta {
        let mut iter = "1 NOFLAGS test_cluster 127.0.0.1:6379 1 0-16383"
            .split(' ')
            .map(|s| s.to_string())
            .peekable();
        let (meta, extended_args) = ProxyClusterMeta::parse(&mut iter).unwrap();
        assert!(extended_args.is_ok());
        meta
    }

    #[tokio::test]
    async fn test_cluster_not_found() {
        let manager = gen_testing_manager();
        let (cmd_ctx, reply_receiver) = gen_set_command();

        manager.send(cmd_ctx);

        let result = reply_receiver.await;
        assert!(result.is_ok());
        let (_, response, _) = result.unwrap().into_inner();
        let resp = response.into_resp_vec();
        let err = match resp {
            Resp::Error(err_str) => err_str,
            other => panic!(format!("unexpected pattern {:?}", other)),
        };
        let err_str = str::from_utf8(&err).unwrap();
        assert!(err_str.starts_with(ERR_CLUSTER_NOT_FOUND));
    }

    #[tokio::test]
    async fn test_data_command() {
        let meta = gen_proxy_cluster_meta();
        let manager = gen_testing_manager();

        manager.set_meta(meta).unwrap();

        loop {
            let (cmd_ctx, reply_receiver) = gen_set_command();
            manager.send(cmd_ctx);

            let result = reply_receiver.await;
            let (_, response, _) = result.unwrap().into_inner();
            let resp = response.into_resp_vec();
            let s = match resp {
                Resp::Simple(s) => s,
                Resp::Error(err_str)
                    if str::from_utf8(err_str.as_slice())
                        .unwrap()
                        .starts_with(ERR_BACKEND_CONNECTION) =>
                {
                    // The connection future is not ready.
                    Delay::new(Duration::from_millis(1)).await;
                    continue;
                }
                other => panic!(format!(
                    "unexpected pattern {:?}",
                    other.map(|b| pretty_print_bytes(b.as_slice()))
                )),
            };
            assert_eq!(s, OK_REPLY.as_bytes());
            break;
        }
    }
}
