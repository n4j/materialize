// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

//! A SQL stream processor built on top of [timely dataflow] and
//! [differential dataflow].
//!
//! [differential dataflow]: ../differential_dataflow/index.html
//! [timely dataflow]: ../timely/index.html

use std::collections::HashMap;
use std::env;
use std::fs;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ::http::header::HeaderValue;
use anyhow::{anyhow, Context};
use compile_time_run::run_command_str;
use futures::StreamExt;
use mz_coord::PersistConfig;
use mz_dataflow_types::client::RemoteClient;
use mz_dataflow_types::sources::AwsExternalId;
use mz_frontegg_auth::FronteggAuthentication;
use mz_orchestrator::{Orchestrator, ServiceConfig, ServicePort};
use mz_orchestrator_kubernetes::{KubernetesOrchestrator, KubernetesOrchestratorConfig};
use openssl::ssl::{SslAcceptor, SslFiletype, SslMethod, SslVerifyMode};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio_stream::wrappers::TcpListenerStream;

use mz_build_info::BuildInfo;
use mz_coord::LoggingConfig;
use mz_ore::collections::CollectionExt;
use mz_ore::metrics::MetricsRegistry;
use mz_ore::now::NowFn;
use mz_ore::option::OptionExt;
use mz_ore::task;
use mz_pid_file::PidFile;
use mz_secrets::SecretsController;
use mz_secrets_filesystem::FilesystemSecretsController;
use mz_secrets_kubernetes::KubernetesSecretsController;

use crate::mux::Mux;
use crate::server_metrics::Metrics;

pub mod http;
pub mod mux;
pub mod server_metrics;
pub mod telemetry;

pub const BUILD_INFO: BuildInfo = BuildInfo {
    version: env!("CARGO_PKG_VERSION"),
    sha: run_command_str!(
        "sh",
        "-c",
        r#"if [ -n "$MZ_DEV_BUILD_SHA" ]; then
            echo "$MZ_DEV_BUILD_SHA"
        else
            # Unfortunately we need to suppress error messages from `git`, as
            # run_command_str will display no error message at all if we print
            # more than one line of output to stderr.
            git rev-parse --verify HEAD 2>/dev/null || {
                printf "error: unable to determine Git SHA; " >&2
                printf "either build from working Git clone " >&2
                printf "(see https://materialize.com/docs/install/#build-from-source), " >&2
                printf "or specify SHA manually by setting MZ_DEV_BUILD_SHA environment variable" >&2
                exit 1
            }
        fi"#
    ),
    time: run_command_str!("date", "-u", "+%Y-%m-%dT%H:%M:%SZ"),
    target_triple: env!("TARGET_TRIPLE"),
};

/// Configuration for a `materialized` server.
#[derive(Debug, Clone)]
pub struct Config {
    // === Timely and Differential worker options. ===
    /// The number of Timely worker threads that this process should host.
    pub workers: usize,
    /// The Timely worker configuration.
    pub timely_worker: timely::WorkerConfig,

    // === Performance tuning options. ===
    pub logging: Option<LoggingConfig>,
    /// The frequency at which to update introspection.
    pub introspection_frequency: Duration,
    /// The historical window in which distinctions are maintained for
    /// arrangements.
    ///
    /// As arrangements accept new timestamps they may optionally collapse prior
    /// timestamps to the same value, retaining their effect but removing their
    /// distinction. A large value or `None` results in a large amount of
    /// historical detail for arrangements; this increases the logical times at
    /// which they can be accurately queried, but consumes more memory. A low
    /// value reduces the amount of memory required but also risks not being
    /// able to use the arrangement in a query that has other constraints on the
    /// timestamps used (e.g. when joined with other arrangements).
    pub logical_compaction_window: Option<Duration>,
    /// The interval at which sources should be timestamped.
    pub timestamp_frequency: Duration,

    // === Connection options. ===
    /// The IP address and port to listen on.
    pub listen_addr: SocketAddr,
    /// The IP address and port to serve the "third party" metrics registry from.
    pub third_party_metrics_listen_addr: Option<SocketAddr>,
    /// TLS encryption configuration.
    pub tls: Option<TlsConfig>,
    /// Materialize Cloud configuration to enable Frontegg JWT user authentication.
    pub frontegg: Option<FronteggAuthentication>,
    /// Origins for which cross-origin resource sharing (CORS) for HTTP requests
    /// is permitted.
    pub cors_allowed_origins: Vec<HeaderValue>,

    // === Storage options. ===
    /// The directory in which `materialized` should store its own metadata.
    pub data_directory: PathBuf,
    /// The configuration of the storage layer.
    pub storage: StorageConfig,

    // === Platform options. ===
    /// Optional configuration for a service orchestrator.
    pub orchestrator: Option<OrchestratorConfig>,

    // === Secrets Storage options. ===
    /// Optional configuration for a secrets controller.
    pub secrets_controller: Option<SecretsControllerConfig>,

    // === AWS options. ===
    /// An [external ID] to be supplied to all AWS AssumeRole operations.
    ///
    /// [external id]: https://docs.aws.amazon.com/IAM/latest/UserGuide/id_roles_create_for-user_externalid.html
    pub aws_external_id: AwsExternalId,

    // === Mode switches. ===
    /// Whether to permit usage of experimental features.
    pub experimental_mode: bool,
    /// Whether to enable catalog-only mode.
    pub disable_user_indexes: bool,
    /// Whether to run in safe mode.
    pub safe_mode: bool,
    /// Telemetry configuration.
    pub telemetry: Option<TelemetryConfig>,
    /// The place where the server's metrics will be reported from.
    pub metrics_registry: MetricsRegistry,
    /// Configuration of the persistence runtime and features.
    pub persist: PersistConfig,
    /// Now generation function.
    pub now: NowFn,
}

/// Configures TLS encryption for connections.
#[derive(Debug, Clone)]
pub struct TlsConfig {
    /// The TLS mode to use.
    pub mode: TlsMode,
    /// The path to the TLS certificate.
    pub cert: PathBuf,
    /// The path to the TLS key.
    pub key: PathBuf,
}

/// Configures how strictly to enforce TLS encryption and authentication.
#[derive(Debug, Clone)]
pub enum TlsMode {
    /// Require that all clients connect with TLS, but do not require that they
    /// present a client certificate.
    Require,
    /// Require that clients connect with TLS and present a certificate that
    /// is signed by the specified CA.
    VerifyCa {
        /// The path to a TLS certificate authority.
        ca: PathBuf,
    },
    /// Like [`TlsMode::VerifyCa`], but the `cn` (Common Name) field of the
    /// certificate must additionally match the user named in the connection
    /// request.
    VerifyFull {
        /// The path to a TLS certificate authority.
        ca: PathBuf,
    },
}

/// Telemetry configuration.
#[derive(Debug, Clone)]
pub struct TelemetryConfig {
    /// The domain hosting the telemetry server.
    pub domain: String,
    /// The interval at which to report telemetry data.
    pub interval: Duration,
}

/// Configuration for the service orchestrator.
#[derive(Debug, Clone)]
pub enum OrchestratorConfig {
    /// Create a Kubernetes orchestrator.
    Kubernetes {
        /// The configuration for the orchestrator itself.
        config: KubernetesOrchestratorConfig,
        /// The dataflowd image reference to use.
        dataflowd_image: String,
    },
}

/// Configuration for the service orchestrator.
#[derive(Debug, Clone)]
pub enum SecretsControllerConfig {
    LocalFileSystem,
    // Create a Kubernetes Controller.
    Kubernetes {
        /// The name of a Kubernetes context to use, if the Kubernetes configuration
        /// is loaded from the local kubeconfig.
        context: String,
    },
}

/// Configuration of the storage layer.
#[derive(Debug, Clone)]
pub enum StorageConfig {
    /// Run a local storage instance.
    Local,
    /// Use a remote storage instance.
    Remote(RemoteStorageConfig),
}

/// Configuration of a remote storage instance.
#[derive(Debug, Clone)]
pub struct RemoteStorageConfig {
    /// The address that compute instances should connect to.
    pub compute_addr: String,
    /// The address that the controller should connect to.
    pub controller_addr: String,
}

/// Start a `materialized` server.
pub async fn serve(mut config: Config) -> Result<Server, anyhow::Error> {
    let workers = config.workers;

    // Validate TLS configuration, if present.
    let (pgwire_tls, http_tls) = match &config.tls {
        None => (None, None),
        Some(tls_config) => {
            let context = {
                // Mozilla publishes three presets: old, intermediate, and modern. They
                // recommend the intermediate preset for general purpose servers, which
                // is what we use, as it is compatible with nearly every client released
                // in the last five years but does not include any known-problematic
                // ciphers. We once tried to use the modern preset, but it was
                // incompatible with Fivetran, and presumably other JDBC-based tools.
                let mut builder = SslAcceptor::mozilla_intermediate_v5(SslMethod::tls())?;
                if let TlsMode::VerifyCa { ca } | TlsMode::VerifyFull { ca } = &tls_config.mode {
                    builder.set_ca_file(ca)?;
                    builder.set_verify(SslVerifyMode::PEER | SslVerifyMode::FAIL_IF_NO_PEER_CERT);
                }
                builder.set_certificate_chain_file(&tls_config.cert)?;
                builder.set_private_key_file(&tls_config.key, SslFiletype::PEM)?;
                builder.build().into_context()
            };
            let pgwire_tls = mz_pgwire::TlsConfig {
                context: context.clone(),
                mode: match tls_config.mode {
                    TlsMode::Require | TlsMode::VerifyCa { .. } => mz_pgwire::TlsMode::Require,
                    TlsMode::VerifyFull { .. } => mz_pgwire::TlsMode::VerifyUser,
                },
            };
            let http_tls = http::TlsConfig {
                context,
                mode: match tls_config.mode {
                    TlsMode::Require | TlsMode::VerifyCa { .. } => http::TlsMode::Require,
                    TlsMode::VerifyFull { .. } => http::TlsMode::AssumeUser,
                },
            };
            (Some(pgwire_tls), Some(http_tls))
        }
    };

    // Attempt to acquire PID file lock.
    let pid_file =
        PidFile::open(config.data_directory.join("materialized.pid")).map_err(|e| match e {
            // Enhance error with some materialized-specific details.
            mz_pid_file::Error::AlreadyRunning { pid } => anyhow!(
                "another materialized process (PID {}) is running with the same data directory\n\
                data directory: {}\n",
                pid.display_or("<unknown>"),
                fs::canonicalize(&config.data_directory)
                    .unwrap_or_else(|_| config.data_directory.clone())
                    .display(),
            ),
            e => e.into(),
        })?;

    // Initialize network listener.
    let listener = TcpListener::bind(&config.listen_addr).await?;
    let local_addr = listener.local_addr()?;

    // Load the coordinator catalog from disk.
    let coord_storage = mz_coord::catalog::storage::Connection::open(
        &config.data_directory,
        Some(config.experimental_mode),
    )?;

    // Initialize persistence runtime.
    let persister = config
        .persist
        .init(
            // Safe to use the cluster ID as the reentrance ID because
            // `materialized` can only run as a single node.
            coord_storage.cluster_id(),
            BUILD_INFO,
            &config.metrics_registry,
        )
        .await?;

    // Initialize orchestrator.
    let orchestrator = match config.orchestrator {
        None => None,
        Some(OrchestratorConfig::Kubernetes {
            config: kubernetes_config,
            dataflowd_image,
        }) => {
            let orchestrator = KubernetesOrchestrator::new(kubernetes_config)
                .await
                .context("connecting to kubernetes")?;

            if let StorageConfig::Local = &config.storage {
                let storage_workers = 1;
                let service = orchestrator
                    .namespace("storage")
                    .ensure_service(
                        "runtime",
                        ServiceConfig {
                            image: dataflowd_image.clone(),
                            args: vec![
                                format!("--workers={storage_workers}"),
                                "--runtime=storage".into(),
                                format!("--storage-addr=0.0.0.0:2101"),
                            ],
                            ports: vec![
                                ServicePort {
                                    name: "controller".into(),
                                    port: 2100,
                                },
                                ServicePort {
                                    name: "storage".into(),
                                    port: 2101,
                                },
                            ],
                            // TODO: limits?
                            cpu_limit: None,
                            memory_limit: None,
                            processes: 1,
                            labels: HashMap::new(),
                        },
                    )
                    .await?;
                let storage_host = service.hosts().into_element();
                config.storage = StorageConfig::Remote(RemoteStorageConfig {
                    compute_addr: format!("{storage_host}:2101"),
                    controller_addr: format!("{storage_host}:2100"),
                });
            }

            let remote_storage_config = match &config.storage {
                StorageConfig::Local => unreachable!("storage config forced to be remote above"),
                StorageConfig::Remote(c) => c,
            };

            Some(mz_dataflow_types::client::controller::OrchestratorConfig {
                orchestrator: Box::new(orchestrator),
                dataflowd_image,
                storage_addr: remote_storage_config.compute_addr.clone(),
            })
        }
    };

    // Initialize secrets controller.
    let secrets_controller: Box<dyn SecretsController> = match config.secrets_controller {
        None | Some(SecretsControllerConfig::LocalFileSystem) => {
            let secrets_storage = config.data_directory.join("secrets");
            fs::create_dir_all(&secrets_storage).with_context(|| {
                format!("creating secrets directory: {}", secrets_storage.display())
            })?;
            Box::new(FilesystemSecretsController::new(secrets_storage))
        }
        Some(SecretsControllerConfig::Kubernetes { context }) => Box::new(
            KubernetesSecretsController::new(context)
                .await
                .context("connecting to kubernetes")?,
        ),
    };

    // Initialize dataflow server.
    let dataflow_config = mz_dataflow::Config {
        workers,
        timely_config: timely::Config {
            communication: timely::CommunicationConfig::Process(workers),
            worker: config.timely_worker,
        },
        experimental_mode: config.experimental_mode,
        now: config.now.clone(),
        metrics_registry: config.metrics_registry.clone(),
        persister: persister.runtime.clone(),
        aws_external_id: config.aws_external_id.clone(),
    };
    let (dataflow_server, dataflow_controller) = match &config.storage {
        StorageConfig::Local => {
            let (dataflow_server, storage_client, local_compute_client) =
                mz_dataflow::serve(dataflow_config)?;
            let storage_controller =
                mz_dataflow_types::client::controller::storage::Controller::new(
                    Box::new(storage_client),
                    config.data_directory,
                );
            let dataflow_controller = mz_dataflow_types::client::Controller::new(
                orchestrator,
                storage_controller,
                Box::new(local_compute_client),
            );
            (dataflow_server, dataflow_controller)
        }
        StorageConfig::Remote(RemoteStorageConfig {
            compute_addr,
            controller_addr,
        }) => {
            let (storage_compute_client, _thread) =
                mz_dataflow::tcp_boundary::client::connect(compute_addr, config.workers).await?;
            let boundary = (0..config.workers)
                .into_iter()
                .map(|_| Some((mz_dataflow::DummyBoundary, storage_compute_client.clone())))
                .collect::<Vec<_>>();
            let boundary = Arc::new(Mutex::new(boundary));
            let workers = config.workers;
            let (compute_server, _inactive_storage_client, local_compute_client) =
                mz_dataflow::serve_boundary(dataflow_config, move |index| {
                    boundary.lock().unwrap()[index % workers].take().unwrap()
                })?;
            let storage_client = Box::new({
                let mut client = RemoteClient::new(&[controller_addr]);
                client.connect().await;
                client
            });
            let storage_controller =
                mz_dataflow_types::client::controller::storage::Controller::new(
                    storage_client,
                    config.data_directory,
                );
            let dataflow_controller = mz_dataflow_types::client::Controller::new(
                orchestrator,
                storage_controller,
                Box::new(local_compute_client),
            );
            (compute_server, dataflow_controller)
        }
    };

    // Initialize coordinator.
    let (coord_handle, coord_client) = mz_coord::serve(mz_coord::Config {
        dataflow_client: dataflow_controller,
        logging: config.logging,
        storage: coord_storage,
        timestamp_frequency: config.timestamp_frequency,
        logical_compaction_window: config.logical_compaction_window,
        experimental_mode: config.experimental_mode,
        disable_user_indexes: config.disable_user_indexes,
        safe_mode: config.safe_mode,
        build_info: &BUILD_INFO,
        aws_external_id: config.aws_external_id.clone(),
        metrics_registry: config.metrics_registry.clone(),
        persister,
        now: config.now,
        secrets_controller,
    })
    .await?;

    // Register metrics.
    let mut metrics_registry = config.metrics_registry;
    let metrics =
        Metrics::register_with(&mut metrics_registry, workers, coord_handle.start_instant());

    // Listen on the third-party metrics port if we are configured for it.
    if let Some(third_party_addr) = config.third_party_metrics_listen_addr {
        task::spawn(|| "metrics_server", {
            let server = http::ThirdPartyServer::new(metrics_registry.clone());
            async move {
                server.serve(third_party_addr).await;
            }
        });
    }

    // Launch task to serve connections.
    //
    // The lifetime of this task is controlled by a trigger that activates on
    // drop. Draining marks the beginning of the server shutdown process and
    // indicates that new user connections (i.e., pgwire and HTTP connections)
    // should be rejected. Once all existing user connections have gracefully
    // terminated, this task exits.
    let (drain_trigger, drain_tripwire) = oneshot::channel();
    task::spawn(|| "pgwire_server", {
        let pgwire_server = mz_pgwire::Server::new(mz_pgwire::Config {
            tls: pgwire_tls,
            coord_client: coord_client.clone(),
            metrics_registry: &metrics_registry,
            frontegg: config.frontegg.clone(),
        });
        let http_server = http::Server::new(http::Config {
            tls: http_tls,
            frontegg: config.frontegg,
            coord_client: coord_client.clone(),
            metrics_registry,
            global_metrics: metrics,
            pgwire_metrics: pgwire_server.metrics(),
            allowed_origins: config.cors_allowed_origins,
        });
        let mut mux = Mux::new();
        mux.add_handler(pgwire_server);
        mux.add_handler(http_server);
        async move {
            // TODO(benesch): replace with `listener.incoming()` if that is
            // restored when the `Stream` trait stabilizes.
            let mut incoming = TcpListenerStream::new(listener);
            mux.serve(incoming.by_ref().take_until(drain_tripwire))
                .await;
        }
    });

    // Start telemetry reporting loop.
    if let Some(telemetry) = config.telemetry {
        let config = telemetry::Config {
            domain: telemetry.domain,
            interval: telemetry.interval,
            cluster_id: coord_handle.cluster_id(),
            workers,
            coord_client,
        };
        task::spawn(|| "telemetry_loop", async move {
            telemetry::report_loop(config).await
        });
    }

    Ok(Server {
        local_addr,
        _pid_file: pid_file,
        _drain_trigger: drain_trigger,
        _coord_handle: coord_handle,
        _dataflow_server: dataflow_server,
    })
}

/// A running `materialized` server.
pub struct Server {
    local_addr: SocketAddr,
    _pid_file: PidFile,
    // Drop order matters for these fields.
    _drain_trigger: oneshot::Sender<()>,
    _coord_handle: mz_coord::Handle,
    _dataflow_server: mz_dataflow::Server,
}

impl Server {
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }
}
