//! Configures and runs the inbound proxy.
//!
//! The inbound proxy is responsible for terminating traffic from other network
//! endpoints inbound to the local application.

#![deny(warnings, rust_2018_idioms)]
#![forbid(unsafe_code)]

pub mod direct;
pub mod http;
pub mod port_policies;
mod server;
pub mod target;
#[cfg(any(test, fuzzing))]
pub(crate) mod test_util;

pub use self::port_policies::PortPolicies;
use linkerd_app_core::{
    config::{ConnectConfig, ProxyConfig, ServerConfig},
    drain, io, metrics, profiles,
    proxy::tcp,
    serve, svc,
    transport::{self, listen::Bind, ClientAddr, Local, OrigDstAddr, Remote, ServerAddr},
    Error, NameMatch, ProxyRuntime,
};
use std::{fmt::Debug, future::Future, time::Duration};
use tracing::debug_span;

#[derive(Clone, Debug)]
pub struct Config {
    pub allow_discovery: NameMatch,
    pub proxy: ProxyConfig,
    pub port_policies: PortPolicies,
    pub profile_idle_timeout: Duration,
}

#[derive(Clone)]
pub struct Inbound<S> {
    config: Config,
    runtime: ProxyRuntime,
    stack: svc::Stack<S>,
}

// === impl Inbound ===

impl<S> Inbound<S> {
    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn runtime(&self) -> &ProxyRuntime {
        &self.runtime
    }

    pub fn into_stack(self) -> svc::Stack<S> {
        self.stack
    }

    pub fn into_inner(self) -> S {
        self.stack.into_inner()
    }

    /// Creates a new `Inbound` by replacing the inner stack, as modified by `f`.
    fn map_stack<T>(
        self,
        f: impl FnOnce(&Config, &ProxyRuntime, svc::Stack<S>) -> svc::Stack<T>,
    ) -> Inbound<T> {
        let stack = f(&self.config, &self.runtime, self.stack);
        Inbound {
            config: self.config,
            runtime: self.runtime,
            stack,
        }
    }
}

impl Inbound<()> {
    pub fn new(config: Config, runtime: ProxyRuntime) -> Self {
        Self {
            config,
            runtime,
            stack: svc::stack(()),
        }
    }

    pub fn with_stack<S>(self, stack: S) -> Inbound<S> {
        self.map_stack(move |_, _, _| svc::stack(stack))
    }

    /// Readies the inbound stack to make TCP connections (for both TCP
    // forwarding and HTTP proxying).
    pub fn into_tcp_connect<T>(
        self,
        proxy_port: u16,
    ) -> Inbound<
        impl svc::Service<
                T,
                Response = impl io::AsyncRead + io::AsyncWrite + Send,
                Error = Error,
                Future = impl Send,
            > + Clone,
    >
    where
        T: svc::Param<u16> + 'static,
    {
        self.map_stack(|config, _, _| {
            // Establishes connections to remote peers (for both TCP
            // forwarding and HTTP proxying).
            let ConnectConfig {
                ref keepalive,
                ref timeout,
                ..
            } = config.proxy.connect;

            #[derive(Debug, thiserror::Error)]
            #[error("inbound connection must not target port {0}")]
            struct Loop(u16);

            svc::stack(transport::ConnectTcp::new(*keepalive))
                // Limits the time we wait for a connection to be established.
                .push_connect_timeout(*timeout)
                // Prevent connections that would target the inbound proxy port from looping.
                .push_request_filter(move |t: T| {
                    let port = t.param();
                    if port == proxy_port {
                        return Err(Loop(port));
                    }
                    Ok(Remote(ServerAddr(([127, 0, 0, 1], port).into())))
                })
        })
    }

    pub fn serve<B, G, GSvc, P>(
        self,
        bind: B,
        profiles: P,
        gateway: G,
    ) -> (Local<ServerAddr>, impl Future<Output = ()> + Send)
    where
        B: Bind<ServerConfig>,
        B::Addrs: svc::Param<Remote<ClientAddr>>
            + svc::Param<Local<ServerAddr>>
            + svc::Param<OrigDstAddr>,
        G: svc::NewService<direct::GatewayConnection, Service = GSvc>,
        G: Clone + Send + Sync + Unpin + 'static,
        GSvc: svc::Service<direct::GatewayIo<io::ScopedIo<B::Io>>, Response = ()> + Send + 'static,
        GSvc::Error: Into<Error>,
        GSvc::Future: Send,
        P: profiles::GetProfile<profiles::LookupAddr> + Clone + Send + Sync + Unpin + 'static,
        P::Error: Send,
        P::Future: Send,
    {
        let (Local(ServerAddr(la)), listen) = bind
            .bind(&self.config.proxy.server)
            .expect("Failed to bind inbound listener");

        let serve = async move {
            let shutdown = self.runtime.drain.clone().signaled();
            let stack = self
                .into_tcp_connect(la.port())
                .push_server(la.port(), profiles, gateway)
                .into_inner();
            serve::serve(listen, stack, shutdown).await
        };

        (Local(ServerAddr(la)), serve)
    }
}

impl<S> Inbound<S> {
    pub fn push<L: svc::layer::Layer<S>>(self, layer: L) -> Inbound<L::Service> {
        self.map_stack(|_, _, stack| stack.push(layer))
    }

    // Forwards TCP streams that cannot be decoded as HTTP.
    //
    // Looping is always prevented.
    pub fn push_tcp_forward<T, I>(
        self,
    ) -> Inbound<
        svc::BoxNewService<
            T,
            impl svc::Service<I, Response = (), Error = Error, Future = impl Send> + Clone,
        >,
    >
    where
        T: svc::Param<transport::labels::Key> + Clone + Send + 'static,
        I: io::AsyncRead + io::AsyncWrite,
        I: Debug + Send + Sync + Unpin + 'static,
        S: svc::Service<T> + Clone + Send + Sync + Unpin + 'static,
        S::Response: io::AsyncRead + io::AsyncWrite + Send + Unpin + 'static,
        S::Error: Into<Error>,
        S::Future: Send,
    {
        self.map_stack(|_, rt, conn| {
            conn.push(rt.metrics.transport.layer_connect())
                .push_make_thunk()
                .push_on_response(
                    svc::layers()
                        .push(tcp::Forward::layer())
                        .push(drain::Retain::layer(rt.drain.clone())),
                )
                .instrument(|_: &_| debug_span!("tcp"))
                .push(svc::BoxNewService::layer())
                .check_new::<T>()
        })
    }
}

fn stack_labels(proto: &'static str, name: &'static str) -> metrics::StackLabels {
    metrics::StackLabels::inbound(proto, name)
}
