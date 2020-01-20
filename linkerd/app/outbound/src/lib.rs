//! Configures and runs the outbound proxy.
//!
//! The outound proxy is responsible for routing traffic from the local
//! application to external network endpoints.

#![deny(warnings, rust_2018_idioms)]

pub use self::endpoint::{
    Concrete, HttpEndpoint, Logical, LogicalPerRequest, Profile, ProfilePerTarget, Target,
    TcpEndpoint,
};
use futures::future;
use linkerd2_app_core::{
    classify,
    config::{ProxyConfig, ServerConfig},
    dns, drain, dst, errors,
    opencensus::proto::trace::v1 as oc,
    proxy::{
        self, core::resolve::Resolve, discover, fallback, http, identity, resolve::map_endpoint,
        tap, tcp, Server,
    },
    reconnect, retry, router, serve,
    spans::SpanConverter,
    svc::{self, NewService},
    trace_context,
    transport::{self, connect, tls, OrigDstAddr, SysOrigDstAddr},
    Conditional, DiscoveryRejected, Error, ProxyMetrics, CANONICAL_DST_HEADER, DST_OVERRIDE_HEADER,
    L5D_CLIENT_ID, L5D_REMOTE_IP, L5D_REQUIRE_ID, L5D_SERVER_ID,
};
use linkerd2_service_profiles as profiles;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::info_span;

#[allow(dead_code)] // TODO #2597
mod add_remote_ip_on_rsp;
#[allow(dead_code)] // TODO #2597
mod add_server_id_on_rsp;
mod endpoint;
mod orig_proto_upgrade;
mod require_identity_on_endpoint;

const EWMA_DEFAULT_RTT: Duration = Duration::from_millis(30);
const EWMA_DECAY: Duration = Duration::from_secs(10);

#[derive(Clone, Debug)]
pub struct Config<A: OrigDstAddr = SysOrigDstAddr> {
    pub proxy: ProxyConfig<A>,
    pub canonicalize_timeout: Duration,
}

pub struct Outbound {
    pub listen_addr: SocketAddr,
    pub serve: serve::Task,
}

impl<A: OrigDstAddr> Config<A> {
    pub fn with_orig_dst_addr<B: OrigDstAddr>(self, orig_dst_addr: B) -> Config<B> {
        Config {
            proxy: self.proxy.with_orig_dst_addr(orig_dst_addr),
            canonicalize_timeout: self.canonicalize_timeout,
        }
    }

    pub fn build<R, P>(
        self,
        local_identity: tls::Conditional<identity::Local>,
        resolve: R,
        dns_resolver: dns::Resolver,
        profiles_client: P,
        tap_layer: tap::Layer,
        metrics: ProxyMetrics,
        span_sink: Option<mpsc::Sender<oc::Span>>,
        drain: drain::Watch,
    ) -> Result<Outbound, Error>
    where
        A: Send + 'static,
        R: Resolve<Concrete<http::Settings>, Endpoint = proxy::api_resolve::Metadata>
            + Clone
            + Send
            + Sync
            + 'static,
        R::Future: Send,
        R::Resolution: Send,
        P: profiles::GetRoutes<Profile> + Clone + Send + 'static,
        P::Future: Send,
    {
        use proxy::core::listen::{Bind, Listen};
        let Config {
            canonicalize_timeout,
            proxy:
                ProxyConfig {
                    server: ServerConfig { bind, h2_settings },
                    connect,
                    cache_capacity,
                    cache_max_idle_age,
                    disable_protocol_detection_for_ports,
                    service_acquisition_timeout,
                    max_in_flight_requests,
                },
        } = self;

        let listen = bind.bind().map_err(Error::from)?;
        let listen_addr = listen.listen_addr();

        // The stack is served lazily since caching layers spawn tasks from
        // their constructor. This helps to ensure that tasks are spawned on the
        // same runtime as the proxy.
        let serve = Box::new(future::lazy(move || {
            // Establishes connections to remote peers (for both TCP
            // forwarding and HTTP proxying).
            let tcp_connect = svc::stack(connect::Connect::new(connect.keepalive))
                // Initiates mTLS if the target is configured with identity.
                .push(tls::client::Layer::new(local_identity))
                // Limits the time we wait for a connection to be established.
                .push_timeout(connect.timeout)
                .push(metrics.transport.layer_connect(TransportLabels));

            // Forwards TCP streams that cannot be decoded as HTTP.
            let tcp_forward = tcp_connect
                .clone()
                .push_map_target(|meta: tls::accept::Meta| {
                    TcpEndpoint::from(meta.addrs.target_addr())
                })
                .push(svc::layer::mk(tcp::Forward::new));

            // Registers the stack with Tap, Metrics, and OpenCensus tracing
            // export.
            let http_endpoint = {
                let observability = svc::layers()
                    .push(tap_layer.clone())
                    .push(metrics.http_endpoint.into_layer::<classify::Response>())
                    .push_per_service(trace_context::layer(
                        span_sink
                            .clone()
                            .map(|sink| SpanConverter::client(sink, trace_labels())),
                    ));

                // Checks the headers to validate that a client-specified required
                // identity matches the configured identity.
                let identity_headers = svc::layers()
                    .push_per_service(
                        svc::layers()
                            .push(http::strip_header::response::layer(L5D_REMOTE_IP))
                            .push(http::strip_header::response::layer(L5D_SERVER_ID))
                            .push(http::strip_header::request::layer(L5D_REQUIRE_ID)),
                    )
                    .push(require_identity_on_endpoint::layer());

                tcp_connect
                    .clone()
                    // Initiates an HTTP client on the underlying transport.
                    // Prior-knowledge HTTP/2 is typically used (i.e. when
                    // communicating with other proxies); though HTTP/1.x fallback
                    // is supported as needed.
                    .push(http::client::layer(connect.h2_settings))
                    // Re-establishes a connection when the client fails.
                    .push(reconnect::layer({
                        let backoff = connect.backoff.clone();
                        move |_| Ok(backoff.stream())
                    }))
                    .push(observability.clone())
                    .push(identity_headers.clone())
                    // Ensures that the request's URI is in the proper form.
                    .push(http::normalize_uri::layer())
                    // Upgrades HTTP/1 requests to be transported over HTTP/2
                    // connections.
                    //
                    // This sets headers so that the inbound proxy can downgrade the
                    // request properly.
                    .push(orig_proto_upgrade::layer())
                    .check_service::<Target<HttpEndpoint>>()
                    .push_trace(|endpoint: &Target<HttpEndpoint>| {
                        info_span!("endpoint", peer.addr = %endpoint.inner.addr, peer.id = ?endpoint.inner.identity)
                    })
            };

            // Resolves each target via the control plane on a background task,
            // buffering results.
            //
            // This buffer controls how many discovery updates may be
            // pending/unconsumed by the balancer before backpressure is applied
            // on the resolution stream. If the buffer is full for
            // `cache_max_idle_age`, then the resolution task fails.
            let discover = {
                const BUFFER_CAPACITY: usize = 10;
                let resolve = map_endpoint::Resolve::new(endpoint::FromMetadata, resolve.clone());
                discover::Layer::new(BUFFER_CAPACITY, cache_max_idle_age, resolve)
            };

            // If the balancer fails to be created, i.e., because it is
            // unresolvable, fall back to using a router that dispatches request
            // to the application-selected original destination.

            // Builds a balancer for each concrete destination.
            let http_balancer_cache = http_endpoint
                .clone()
                .check_make_service::<Target<HttpEndpoint>, http::Request<http::boxed::Payload>>()
                .push_per_service(http::box_request::Layer::new())
                .push_spawn_ready()
                .check_service::<Target<HttpEndpoint>>()
                .push(discover)
                .push_per_service(http::balance::layer(EWMA_DEFAULT_RTT, EWMA_DECAY))
                .push_pending()
                // Shares the balancer, ensuring discovery errors are propagated.
                .push_per_service(svc::lock::Layer::<DiscoveryError>::new())
                .spawn_cache(cache_capacity, cache_max_idle_age)
                .push_trace(|c: &Concrete<http::Settings>| info_span!("balance", addr = %c.addr));

            // Caches clients that bypass discovery/balancing.
            //
            // This is effectively the same as the endpoint stack; but the
            // client layer captures the requst body type (via PhantomData), so
            // the stack cannot be shared directly.
            let http_forward_cache = http_endpoint
                .check_make_service::<Target<HttpEndpoint>, http::Request<http::boxed::Payload>>()
                .push_per_service(http::box_request::Layer::new())
                .push_pending()
                .push_per_service(svc::layers().push_lock())
                .spawn_cache(cache_capacity, cache_max_idle_age)
                .push_trace(|endpoint: &Target<HttpEndpoint>| {
                    info_span!("forward", peer.addr = %endpoint.addr, peer.id = ?endpoint.inner.identity)
                })
                .push_map_target(|t: Concrete<HttpEndpoint>| Target {
                    addr: t.addr.into(),
                    inner: t.inner.inner,
                })
                .check_service::<Concrete<HttpEndpoint>>();

            let http_concrete = http_balancer_cache
                .push_map_target(|c: Concrete<HttpEndpoint>| c.map(|l| l.map(|e| e.settings)))
                .check_service::<Concrete<HttpEndpoint>>()
                .push_per_service(svc::layers().boxed_http_response())
                .push_make_ready()
                .push(
                    fallback::Layer::new(
                        http_forward_cache
                            .push_per_service(svc::layers().boxed_http_response())
                            .into_inner(),
                    )
                    .with_predicate(DiscoveryError::is_discovery_rejected),
                )
                .check_service::<Concrete<HttpEndpoint>>();

            let http_profile_route_proxy = svc::proxies()
                .check_new_clone_service::<dst::Route>()
                // Sets an optional retry policy.
                .push(retry::layer(metrics.http_route_retry))
                .check_new_clone_service::<dst::Route>()
                // Sets an optional request timeout.
                .push(http::timeout::layer())
                .check_new_clone_service::<dst::Route>()
                // Records per-route metrics.
                .push(metrics.http_route.into_layer::<classify::Response>())
                .check_new_clone_service::<dst::Route>()
                // Sets the per-route response classifier as a request
                // extension.
                .push(classify::Layer::new())
                .check_new_clone_service::<dst::Route>();

            // Routes `Logical` targets to a cached `Profile` stack, i.e. so
            // that profile resolutions are shared even as the type of request
            // may vary.
            let http_logical_profile_cache = http_concrete
                .clone()
                .push_per_service(http::box_request::Layer::new())
                .check_service::<Concrete<HttpEndpoint>>()
                // Provides route configuration. The profile service operates
                // over `Concret` services. When overrides are in play, the
                // Concrete destination may be overridden.
                .push(profiles::Layer::with_overrides(
                    profiles_client,
                    http_profile_route_proxy.into_inner(),
                ))
                .check_make_service::<Profile, Concrete<HttpEndpoint>>()
                // Use the `Logical` target as a `Concrete` target. It may be
                // overridden by the profile layer.
                .push_per_service(svc::map_target::Layer::new(
                    |inner: Logical<HttpEndpoint>| Concrete {
                        addr: inner.addr.clone(),
                        inner,
                    },
                ))
                .push_pending()
                .push_per_service(svc::lock::Layer::<DiscoveryError>::new())
                .spawn_cache(cache_capacity, cache_max_idle_age)
                .push_trace(|_: &Profile| info_span!("profile"))
                .check_service::<Profile>()
                .push(router::Layer::new(|()| ProfilePerTarget))
                .check_new_service_routes::<(), Logical<HttpEndpoint>>()
                .new_service(());

            // Caches DNS refinements from relative names to canonical names.
            //
            // For example, a client may send requests to `foo` or `foo.ns`; and
            // the canonical form of these names is `foo.ns.svc.cluster.local
            let dns_refine_cache = svc::stack(dns_resolver.into_make_refine())
                .push_per_service(svc::lock::Layer::default())
                .spawn_cache(cache_capacity, cache_max_idle_age)
                .push_trace(|name: &dns::Name| info_span!("canonicalize", %name))
                // Obtains the lock, advances the state of the resolution
                .push(svc::make_response::Layer)
                // Ensures that the cache isn't locked when polling readiness.
                .push_oneshot()
                .check_service_response::<dns::Name, dns::Name>()
                .into_inner();

            // Routes requests to their logical target.
            let http_logical_router = svc::stack(http_logical_profile_cache)
                .check_service::<Logical<HttpEndpoint>>()
                .push_per_service(svc::layers().boxed_http_response())
                .push_make_ready()
                .push(
                    fallback::Layer::new(
                        http_concrete
                            .push_map_target(|inner: Logical<HttpEndpoint>| Concrete {
                                addr: inner.addr.clone(),
                                inner,
                            })
                            .push_per_service(
                                svc::layers().boxed_http_response().boxed_http_request(),
                            )
                            .check_service::<Logical<HttpEndpoint>>()
                            .into_inner(),
                    )
                    .with_predicate(DiscoveryError::is_discovery_rejected),
                )
                .check_service::<Logical<HttpEndpoint>>()
                // Sets the canonical-dst header on all outbound requests.
                .push(http::header_from_target::layer(CANONICAL_DST_HEADER))
                // Strips headers that may be set by this proxy.
                .push(http::canonicalize::Layer::new(
                    dns_refine_cache,
                    canonicalize_timeout,
                ))
                .push_per_service(
                    // Strips headers that may be set by this proxy.
                    svc::layers()
                        .push(http::strip_header::request::layer(L5D_CLIENT_ID))
                        .push(http::strip_header::request::layer(DST_OVERRIDE_HEADER)),
                )
                .check_service::<Logical<HttpEndpoint>>()
                .push_trace(|logical: &Logical<_>| info_span!("logical", addr = %logical.addr));

            let http_admit_request = svc::layers()
                // Ensures that load is not shed if the inner service is in-use.
                .push_oneshot()
                // Limits the number of in-flight requests.
                .push_concurrency_limit(max_in_flight_requests)
                // Sheds load if too many requests are in flight.
                //
                // XXX Can this be removed? Is it okay to just backpressure onto
                // the client? Should we instead limit the number of active
                // connections?
                .push_load_shed()
                // Synthesizes responses for proxy errors.
                .push(errors::Layer)
                // Initiates OpenCensus tracing.
                .push(trace_context::layer(span_sink.map(|span_sink| {
                    SpanConverter::server(span_sink, trace_labels())
                })))
                // Tracks proxy handletime.
                .push(metrics.http_handle_time.layer());

            let http_server = http_logical_router
                .check_service::<Logical<HttpEndpoint>>()
                .push_make_ready()
                .push_timeout(service_acquisition_timeout)
                .push(router::Layer::new(LogicalPerRequest::from))
                // Used by tap.
                .push_http_insert_target()
                .push_per_service(http_admit_request)
                .push_trace(
                    |src: &tls::accept::Meta| {
                        info_span!("source", target.addr = %src.addrs.target_addr())
                    },
                )
                .check_new_service::<tls::accept::Meta>();

            let tcp_server = Server::new(
                TransportLabels,
                metrics.transport,
                tcp_forward.into_inner(),
                http_server.into_inner(),
                h2_settings,
                drain.clone(),
                disable_protocol_detection_for_ports.clone(),
            );

            // The local application does not establish mTLS with the proxy.
            let no_tls: tls::Conditional<identity::Local> =
                Conditional::None(tls::ReasonForNoPeerName::Loopback.into());
            let accept = tls::AcceptTls::new(no_tls, tcp_server)
                .with_skip_ports(disable_protocol_detection_for_ports);

            serve::serve(listen, accept, drain)
        }));

        Ok(Outbound { listen_addr, serve })
    }
}

#[derive(Copy, Clone, Debug)]
struct TransportLabels;

impl transport::metrics::TransportLabels<Target<HttpEndpoint>> for TransportLabels {
    type Labels = transport::labels::Key;

    fn transport_labels(&self, endpoint: &Target<HttpEndpoint>) -> Self::Labels {
        transport::labels::Key::connect("outbound", endpoint.inner.identity.as_ref())
    }
}

impl transport::metrics::TransportLabels<TcpEndpoint> for TransportLabels {
    type Labels = transport::labels::Key;

    fn transport_labels(&self, endpoint: &TcpEndpoint) -> Self::Labels {
        transport::labels::Key::connect("outbound", endpoint.identity.as_ref())
    }
}

impl transport::metrics::TransportLabels<proxy::server::Protocol> for TransportLabels {
    type Labels = transport::labels::Key;

    fn transport_labels(&self, proto: &proxy::server::Protocol) -> Self::Labels {
        transport::labels::Key::accept("outbound", proto.tls.peer_identity.as_ref())
    }
}

pub fn trace_labels() -> HashMap<String, String> {
    let mut l = HashMap::new();
    l.insert("direction".to_string(), "outbound".to_string());
    l
}

#[derive(Clone, Debug)]
enum DiscoveryError {
    DiscoveryRejected,
    Inner(String),
}

impl std::fmt::Display for DiscoveryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DiscoveryError::DiscoveryRejected => write!(f, "discovery rejected"),
            DiscoveryError::Inner(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for DiscoveryError {}

impl From<Error> for DiscoveryError {
    fn from(orig: Error) -> Self {
        if let Some(inner) = orig.downcast_ref::<DiscoveryError>() {
            return inner.clone();
        }

        if orig.is::<DiscoveryRejected>() || orig.is::<profiles::InvalidProfileAddr>() {
            return DiscoveryError::DiscoveryRejected;
        }

        DiscoveryError::Inner(orig.to_string())
    }
}

impl DiscoveryError {
    fn is_discovery_rejected(err: &Error) -> bool {
        tracing::trace!(?err, "is_discovery_rejected");
        if let Some(DiscoveryError::DiscoveryRejected) = err.downcast_ref::<DiscoveryError>() {
            return true;
        }

        false
    }
}
