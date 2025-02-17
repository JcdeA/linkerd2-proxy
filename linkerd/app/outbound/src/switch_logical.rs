use crate::{endpoint::Endpoint, logical::Logical, tcp, transport::OrigDstAddr, Outbound};
use linkerd_app_core::{io, profiles, svc, Error, Infallible};
use std::fmt;

impl<S> Outbound<S> {
    /// Wraps an endpoint stack to switch to an alternate logical stack when an appropriate profile
    /// is provided:
    ///
    /// - When a profile includes endpoint information, it is used to build an endpoint stack;
    /// - Otherwise, if the profile indicates the target is logical, a logical stack is built;
    /// - Otherwise, we assume the target is not part of the mesh and we should connect to the
    ///   original destination.
    pub fn push_switch_logical<T, I, N, NSvc, SSvc>(
        self,
        logical: N,
    ) -> Outbound<svc::BoxNewTcp<(Option<profiles::Receiver>, T), I>>
    where
        Self: Clone + 'static,
        T: svc::Param<OrigDstAddr> + Clone + Send + Sync + 'static,
        I: io::AsyncRead + io::AsyncWrite + io::PeerAddr + fmt::Debug + Send + Unpin + 'static,
        N: svc::NewService<tcp::Logical, Service = NSvc> + Clone + Send + Sync + 'static,
        NSvc: svc::Service<I, Response = (), Error = Error> + Send + 'static,
        NSvc::Future: Send,
        S: svc::NewService<tcp::Endpoint, Service = SSvc> + Clone + Send + Sync + 'static,
        SSvc: svc::Service<I, Response = (), Error = Error> + Send + 'static,
        SSvc::Future: Send,
    {
        let no_tls_reason = self.no_tls_reason();
        self.map_stack(|_, _, endpoint| {
            endpoint
                .push_switch(
                    move |(profile, target): (Option<profiles::Receiver>, T)| -> Result<_, Infallible> {
                        if let Some(rx) = profile {
                            // If the profile provides an endpoint, then the target is single endpoint and
                            // not a logical/load-balanced service.
                            if let Some((addr, metadata)) = rx.endpoint() {
                                return Ok(svc::Either::A(Endpoint::from_metadata(
                                    addr,
                                    metadata,
                                    no_tls_reason,
                                    rx.is_opaque_protocol(),
                                )));
                            }

                            // Otherwise, if the profile provides a (named) logical address, then we build a
                            // logical stack so we apply routes, traffic splits, and load balancing.
                            if let Some(logical_addr) = rx.logical_addr() {
                                return Ok(svc::Either::B(Logical::new(logical_addr, rx)));
                            }
                        }

                        // If there was no profile or it didn't include any useful metadata, create a bare
                        // endpoint from the original destination address.
                        Ok(svc::Either::A(Endpoint::forward(
                            target.param(),
                            no_tls_reason,
                        )))
                    },
                    logical,
                )
                .push_on_response(svc::BoxService::layer())
                .push(svc::BoxNewService::layer())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::*;
    use linkerd_app_core::{
        proxy::api_resolve::Metadata,
        svc::{NewService, Param, ServiceExt},
        NameAddr,
    };
    use std::net::{IpAddr, SocketAddr};
    use thiserror::Error;

    #[derive(Debug, Error, Default)]
    #[error("wrong stack built")]
    struct WrongStack;

    #[tokio::test(flavor = "current_thread")]
    async fn no_profile() {
        let _trace = linkerd_tracing::test::trace_init();

        let endpoint = |ep: tcp::Endpoint| {
            assert_eq!(ep.addr.as_ref().ip(), IpAddr::from([192, 0, 2, 20]));
            assert_eq!(ep.addr.as_ref().port(), 2020);
            assert!(!ep.opaque_protocol);
            svc::mk(|_: io::DuplexStream| future::ok::<(), Error>(()))
        };

        let (rt, _shutdown) = runtime();
        let mut stack = Outbound::new(default_config(), rt)
            .with_stack(endpoint)
            .push_switch_logical(svc::Fail::<_, WrongStack>::default())
            .into_inner();

        let orig_dst = OrigDstAddr(SocketAddr::new([192, 0, 2, 20].into(), 2020));
        let svc = stack.new_service((None, orig_dst));
        let (server_io, _client_io) = io::duplex(1);
        svc.oneshot(server_io).await.expect("service must succeed");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn profile_endpoint() {
        let _trace = linkerd_tracing::test::trace_init();

        let endpoint = |ep: tcp::Endpoint| {
            assert_eq!(ep.addr.as_ref().ip(), IpAddr::from([192, 0, 2, 10]));
            assert_eq!(ep.addr.as_ref().port(), 1010);
            assert!(ep.opaque_protocol);
            svc::mk(|_: io::DuplexStream| future::ok::<(), Error>(()))
        };

        let (rt, _shutdown) = runtime();
        let mut stack = Outbound::new(default_config(), rt)
            .with_stack(endpoint)
            .push_switch_logical(svc::Fail::<_, WrongStack>::default())
            .into_inner();

        let (_tx, profile) = tokio::sync::watch::channel(profiles::Profile {
            endpoint: Some((
                SocketAddr::new([192, 0, 2, 10].into(), 1010),
                Metadata::default(),
            )),
            opaque_protocol: true,
            // logical addr does not influence use of endpoint
            addr: Some(profiles::LogicalAddr(
                NameAddr::from_str_and_port("foo.example.com", 3030).unwrap(),
            )),
            ..Default::default()
        });

        let orig_dst = OrigDstAddr(SocketAddr::new([192, 0, 2, 20].into(), 2020));
        let svc = stack.new_service((Some(profile.into()), orig_dst));
        let (server_io, _client_io) = io::duplex(1);
        svc.oneshot(server_io).await.expect("service must succeed");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn profile_logical() {
        let _trace = linkerd_tracing::test::trace_init();

        let logical = |t: tcp::Logical| {
            assert_eq!(t.logical_addr.to_string(), "foo.example.com:3030");
            let skip: Option<crate::http::detect::Skip> = t.param();
            assert!(skip.is_some());
            svc::mk(|_: io::DuplexStream| future::ok::<(), Error>(()))
        };

        let (rt, _shutdown) = runtime();
        let mut stack = Outbound::new(default_config(), rt)
            .with_stack(svc::Fail::<_, WrongStack>::default())
            .push_switch_logical(logical)
            .into_inner();

        let (_tx, profile) = tokio::sync::watch::channel(profiles::Profile {
            addr: Some(profiles::LogicalAddr(
                NameAddr::from_str_and_port("foo.example.com", 3030).unwrap(),
            )),
            opaque_protocol: true,
            ..Default::default()
        });

        let orig_dst = OrigDstAddr(SocketAddr::new([192, 0, 2, 20].into(), 2020));
        let svc = stack.new_service((Some(profile.into()), orig_dst));
        let (server_io, _client_io) = io::duplex(1);
        svc.oneshot(server_io).await.expect("service must succeed");
    }
}
