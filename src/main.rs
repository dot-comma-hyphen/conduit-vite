use std::{future::Future, io, net::SocketAddr, sync::atomic, time::Duration};

use axum::{
    body::Body,
    extract::{FromRequestParts, MatchedPath},
    middleware::map_response,
    response::{IntoResponse, Response},
    routing::{any, get, on, MethodFilter},
    Router,
};
use axum_server::{bind, bind_rustls, tls_rustls::RustlsConfig, Handle as ServerHandle};
use conduit::api::{client_server, server_server};
use figment::{
    providers::{Env, Format, Toml},
    value::Uncased,
    Figment,
};
use http::{
    header::{self, HeaderName, CONTENT_SECURITY_POLICY},
    Method, StatusCode, Uri,
};
use opentelemetry::trace::TracerProvider;
use ruma::api::{
    client::{
        error::{Error as RumaError, ErrorBody, ErrorKind},
        uiaa::UiaaResponse,
    },
    IncomingRequest,
};
use tokio::signal;
use tower::ServiceBuilder;
use tower_http::{
    cors::{self, CorsLayer},
    trace::TraceLayer,
    ServiceBuilderExt as _,
};
use tracing::{debug, error, info, warn};
use tracing_subscriber::{prelude::*, EnvFilter};

pub use conduit::*; // Re-export everything from the library crate

#[cfg(all(not(target_env = "msvc"), feature = "jemalloc"))]
use tikv_jemallocator::Jemalloc;

#[cfg(all(not(target_env = "msvc"), feature = "jemalloc"))]
#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc;

static SUB_TABLES: [&str; 3] = ["well_known", "tls", "media"]; // Not doing `proxy` cause setting that with env vars would be a pain

// Yeah, I know it's terrible, but since it seems the container users dont want syntax like A[B][C]="...",
// this is what we have to deal with. Also see: https://github.com/SergioBenitez/Figment/issues/12#issuecomment-801449465
static SUB_SUB_TABLES: [&str; 2] = ["directory_structure", "retention"];

#[tokio::main]
async fn main() {
    clap::parse();

    // Initialize config
    let raw_config = Figment::new()
        .merge(
            Toml::file(
                Env::var("CONDUIT_CONFIG").expect(
                    "The CONDUIT_CONFIG env var needs to be set. Example: /etc/conduit.toml",
                ),
            )
            .nested(),
        )
        .merge(Env::prefixed("CONDUIT_").global().map(|k| {
            let mut key: Uncased = k.into();

            'outer: for table in SUB_TABLES {
                if k.starts_with(&(table.to_owned() + "_")) {
                    for sub_table in SUB_SUB_TABLES {
                        if k.starts_with(&(table.to_owned() + "_" + sub_table + "_")) {
                            key = Uncased::from(
                                table.to_owned()
                                    + "."
                                    + sub_table
                                    + "."
                                    + k[table.len() + 1 + sub_table.len() + 1..k.len()].as_str(),
                            );

                            break 'outer;
                        }
                    }

                    key = Uncased::from(
                        table.to_owned() + "." + k[table.len() + 1..k.len()].as_str(),
                    );

                    break;
                }
            }

            key
        }));

    let config = match raw_config.extract::<Config>() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("It looks like your config is invalid. The following error occurred: {e}");
            std::process::exit(1);
        }
    };

    config.warn_deprecated();

    if config.ldap.enabled {
        info!("Attempting to connect to LDAP server");
        match ldap3::LdapConn::new(&config.ldap.uri) {
            Ok(mut ldap) => {
                ldap.simple_bind(&config.ldap.bind_dn, &config.ldap.bind_password)
                    .unwrap();
                match ldap.simple_bind(&config.ldap.bind_dn, &config.ldap.bind_password) {
                    Ok(_) => {
                        info!("Successfully connected and bound to LDAP server");
                        ldap.unbind().unwrap();
                    }
                    Err(e) => {
                        error!("Failed to bind to LDAP server: {}", e);
                        std::process::exit(1);
                    }
                }
            }
            Err(e) => {
                error!("Failed to connect to LDAP server: {}", e);
                std::process::exit(1);
            }
        }
    }

    let jaeger = if config.allow_jaeger {
        opentelemetry::global::set_text_map_propagator(
            opentelemetry_jaeger_propagator::Propagator::new(),
        );
        let exporter = opentelemetry_otlp::SpanExporter::builder()
            .with_tonic()
            .build()
            .unwrap();

        let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
            .with_simple_exporter(exporter)
            .build();

        let tracer = provider.tracer("");

        let telemetry = tracing_opentelemetry::layer().with_tracer(tracer);

        let filter_layer = match EnvFilter::try_new(&config.log) {
            Ok(s) => s,
            Err(e) => {
                eprintln!(
                    "It looks like your log config is invalid. The following error occurred: {e}"
                );
                EnvFilter::try_new("warn").unwrap()
            }
        };

        let subscriber = tracing_subscriber::Registry::default()
            .with(filter_layer)
            .with(telemetry);
        tracing::subscriber::set_global_default(subscriber).unwrap();

        Some(provider)
    } else if config.tracing_flame {
        let registry = tracing_subscriber::Registry::default();
        let (flame_layer, _guard) =
            tracing_flame::FlameLayer::with_file("./tracing.folded").unwrap();
        let flame_layer = flame_layer.with_empty_samples(false);

        let filter_layer = EnvFilter::new("trace,h2=off");

        let subscriber = registry.with(filter_layer).with(flame_layer);
        tracing::subscriber::set_global_default(subscriber).unwrap();

        None
    } else {
        let registry = tracing_subscriber::Registry::default();
        let fmt_layer = tracing_subscriber::fmt::Layer::new();
        let filter_layer = match EnvFilter::try_new(&config.log) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("It looks like your config is invalid. The following error occurred while parsing it: {e}");
                EnvFilter::try_new("warn").unwrap()
            }
        };

        let subscriber = registry.with(filter_layer).with(fmt_layer);
        tracing::subscriber::set_global_default(subscriber).unwrap();

        None
    };

    // This is needed for opening lots of file descriptors, which tends to
    // happen more often when using RocksDB and making lots of federation
    // connections at startup. The soft limit is usually 1024, and the hard
    // limit is usually 512000; I've personally seen it hit >2000.
    //
    // * https://www.freedesktop.org/software/systemd/man/systemd.exec.html#id-1.12.2.1.17.6
    // * https://github.com/systemd/systemd/commit/0abf94923b4a95a7d89bc526efc84e7ca2b71741
    #[cfg(unix)]
    maximize_fd_limit().expect("should be able to increase the soft limit to the hard limit");

    info!("Loading database");
    if let Err(error) = KeyValueDatabase::load_or_create(config).await {
        error!(?error, "The database couldn't be loaded or created");

        std::process::exit(1);
    };

    info!("Starting server");
    run_server().await.unwrap();

    if let Some(provider) = jaeger {
        let _ = provider.shutdown();
    }
}

/// Adds additional headers to prevent any potential XSS attacks via the media repo
async fn set_csp_header(response: Response) -> impl IntoResponse {
    (
        [(CONTENT_SECURITY_POLICY, "sandbox; default-src 'none'; script-src 'none'; plugin-types application/pdf; style-src 'unsafe-inline'; object-src 'self';")], response
    )
}

async fn run_server() -> io::Result<()> {
    let config = &services().globals.config;
    let addr = SocketAddr::from((config.address, config.port));

    let x_requested_with = HeaderName::from_static("x-requested-with");

    let middlewares = ServiceBuilder::new()
        .sensitive_headers([header::AUTHORIZATION])
        .layer(axum::middleware::from_fn(spawn_task))
        .layer(
            TraceLayer::new_for_http().make_span_with(|request: &http::Request<_>| {
                let path = if let Some(path) = request.extensions().get::<MatchedPath>() {
                    path.as_str()
                } else {
                    request.uri().path()
                };

                tracing::info_span!("http_request", %path)
            }),
        )
        .layer(axum::middleware::from_fn(unrecognized_method))
        .layer(
            CorsLayer::new()
                .allow_origin(cors::Any)
                .allow_methods([
                    Method::GET,
                    Method::POST,
                    Method::PUT,
                    Method::DELETE,
                    Method::OPTIONS,
                ])
                .allow_headers([
                    header::ORIGIN,
                    x_requested_with,
                    header::CONTENT_TYPE,
                    header::ACCEPT,
                    header::AUTHORIZATION,
                ])
                .max_age(Duration::from_secs(86400)),
        )
        .layer(map_response(set_csp_header));

    let app = routes(config).layer(middlewares).into_make_service();
    let handle = ServerHandle::new();

    tokio::spawn(shutdown_signal(handle.clone()));

    match &config.tls {
        Some(tls) => {
            let conf = RustlsConfig::from_pem_file(&tls.certs, &tls.key).await?;
            let server = bind_rustls(addr, conf).handle(handle).serve(app);

            #[cfg(feature = "systemd")]
            let _ = sd_notify::notify(true, &[sd_notify::NotifyState::Ready]);

            server.await?
        }
        None => {
            let server = bind(addr).handle(handle).serve(app);

            #[cfg(feature = "systemd")]
            let _ = sd_notify::notify(true, &[sd_notify::NotifyState::Ready]);

            server.await?
        }
    }

    Ok(())
}

async fn spawn_task(
    req: http::Request<Body>,
    next: axum::middleware::Next,
) -> std::result::Result<Response, StatusCode> {
    if services().globals.shutdown.load(atomic::Ordering::Relaxed) {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    }
    tokio::spawn(next.run(req))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

async fn unrecognized_method(
    req: http::Request<Body>,
    next: axum::middleware::Next,
) -> std::result::Result<Response, StatusCode> {
    let method = req.method().clone();
    let uri = req.uri().clone();
    let inner = next.run(req).await;
    if inner.status() == StatusCode::METHOD_NOT_ALLOWED {
        warn!("Method not allowed: {method} {uri}");
        return Ok(RumaResponse(UiaaResponse::MatrixError(RumaError {
            body: ErrorBody::Standard {
                kind: ErrorKind::Unrecognized,
                message: "M_UNRECOGNIZED: Unrecognized request".to_owned(),
            },
            status_code: StatusCode::METHOD_NOT_ALLOWED,
        }))
        .into_response());
    }
    Ok(inner)
}

async fn shutdown_signal(handle: ServerHandle) {
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to install signal handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    let sig: &str;

    tokio::select! {
        _ = ctrl_c => { sig = "Ctrl+C"; },
        _ = terminate => { sig = "SIGTERM"; },
    }

    warn!("Received {}, shutting down...", sig);
    handle.graceful_shutdown(Some(Duration::from_secs(30)));

    services().globals.shutdown().await;

    #[cfg(feature = "systemd")]
    let _ = sd_notify::notify(true, &[sd_notify::NotifyState::Stopping]);
}

async fn federation_disabled(_: Uri) -> impl IntoResponse {
    Error::bad_config("Federation is disabled.")
}

async fn not_found(uri: Uri) -> impl IntoResponse {
    warn!("Not found: {uri}");
    Error::BadRequest(ErrorKind::Unrecognized, "Unrecognized request")
}

async fn initial_sync(_uri: Uri) -> impl IntoResponse {
    Error::BadRequest(
        ErrorKind::GuestAccessForbidden,
        "Guest access not implemented",
    )
}

async fn it_works() -> &'static str {
    "Hello from Conduit!"
}

trait RouterExt {
    fn ruma_route<H, T>(self, handler: H) -> Self
    where
        H: RumaHandler<T>,
        T: 'static;
}

impl RouterExt for Router {
    fn ruma_route<H, T>(self, handler: H) -> Self
    where
        H: RumaHandler<T>,
        T: 'static,
    {
        handler.add_to_router(self)
    }
}

pub trait RumaHandler<T> {
    // Can't transform to a handler without boxing or relying on the nightly-only
    // impl-trait-in-traits feature. Moving a small amount of extra logic into the trait
    // allows bypassing both.
    fn add_to_router(self, router: Router) -> Router;
}

macro_rules! impl_ruma_handler {
    ( $($ty:ident),* $(,)? ) => {
        #[allow(non_snake_case)]
        impl<Req, E, F, Fut, $($ty,)*> RumaHandler<($($ty,)* Ruma<Req>,)> for F
        where
            Req: IncomingRequest + Send + 'static,
            F: FnOnce($($ty,)* Ruma<Req>) -> Fut + Clone + Send + Sync + 'static,
            Fut: Future<Output = Result<Req::OutgoingResponse, E>>
                + Send,
            E: IntoResponse,
            $( $ty: FromRequestParts<()> + Send + 'static, )*
        {
            fn add_to_router(self, mut router: Router) -> Router {
                let meta = Req::METADATA;
                let method_filter = method_to_filter(meta.method);

                for path in meta.history.all_paths() {
                    let handler = self.clone();

                    router = router.route(path, on(method_filter, |$( $ty: $ty, )* req| async move {
                        handler($($ty,)* req).await.map(RumaResponse)
                    }))
                }

                router
            }
        }
    };
}

impl_ruma_handler!();
impl_ruma_handler!(T1);
impl_ruma_handler!(T1, T2);
impl_ruma_handler!(T1, T2, T3);
impl_ruma_handler!(T1, T2, T3, T4);
impl_ruma_handler!(T1, T2, T3, T4, T5);
impl_ruma_handler!(T1, T2, T3, T4, T5, T6);
impl_ruma_handler!(T1, T2, T3, T4, T5, T6, T7);
impl_ruma_handler!(T1, T2, T3, T4, T5, T6, T7, T8);

fn method_to_filter(method: Method) -> MethodFilter {
    match method {
        Method::DELETE => MethodFilter::DELETE,
        Method::GET => MethodFilter::GET,
        Method::HEAD => MethodFilter::HEAD,
        Method::OPTIONS => MethodFilter::OPTIONS,
        Method::PATCH => MethodFilter::PATCH,
        Method::POST => MethodFilter::POST,
        Method::PUT => MethodFilter::PUT,
        Method::TRACE => MethodFilter::TRACE,
        m => panic!("Unsupported HTTP method: {m:?}"),
    }
}

#[cfg(unix)]
#[tracing::instrument(err)]
fn maximize_fd_limit() -> Result<(), nix::errno::Errno> {
    use nix::sys::resource::{getrlimit, setrlimit, Resource};

    let res = Resource::RLIMIT_NOFILE;

    let (soft_limit, hard_limit) = getrlimit(res)?;

    debug!("Current nofile soft limit: {soft_limit}");

    setrlimit(res, hard_limit, hard_limit)?;

    debug!("Increased nofile soft limit to {hard_limit}");

    Ok(())
}
