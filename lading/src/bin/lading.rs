use std::{
    env,
    fmt::{self, Display},
    io::Read,
    num::NonZeroU32,
    path::PathBuf,
    str::FromStr,
};

use clap::{ArgGroup, Parser, Subcommand};
use lading::{
    blackhole,
    captures::CaptureManager,
    config::{Config, Telemetry},
    generator::{self, process_tree},
    inspector, observer,
    signals::Shutdown,
    target::{self, Behavior, Output},
    target_metrics,
};
use metrics_exporter_prometheus::PrometheusBuilder;
use rand::{rngs::StdRng, SeedableRng};
use rustc_hash::FxHashMap;
use tokio::{
    runtime::Builder,
    signal,
    sync::broadcast,
    time::{sleep, Duration},
};
use tracing::{debug, error, info, warn};

fn default_config_path() -> String {
    "/etc/lading/lading.yaml".to_string()
}

fn default_target_behavior() -> Behavior {
    Behavior::Quiet
}

#[derive(Default, Clone)]
struct CliKeyValues {
    inner: FxHashMap<String, String>,
}

impl Display for CliKeyValues {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> Result<(), fmt::Error> {
        for (k, v) in self.inner.iter() {
            write!(f, "{}={},", k, v)?;
        }
        Ok(())
    }
}

impl FromStr for CliKeyValues {
    type Err = String;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let pair_err = String::from("pairs must be separated by '='");
        let mut labels = FxHashMap::default();
        for kv in input.split(',') {
            if kv.is_empty() {
                continue;
            }
            let mut pair = kv.split('=');
            let key = pair.next().ok_or_else(|| pair_err.clone())?;
            let value = pair.next().ok_or_else(|| pair_err.clone())?;
            labels.insert(key.into(), value.into());
        }
        Ok(Self { inner: labels })
    }
}

#[derive(Parser)]
#[clap(version, about, long_about = None)]
#[clap(group(
    ArgGroup::new("target")
        .required(true)
        .args(&["target-path", "target-pid", "no-target"]),
))]
struct Opts {
    /// path on disk to the configuration file
    #[clap(long, default_value_t = default_config_path())]
    config_path: String,
    /// additional labels to apply to all captures, format KEY=VAL,KEY2=VAL
    #[clap(long)]
    global_labels: Option<CliKeyValues>,
    /// measure an externally-launched process by PID
    #[clap(long)]
    target_pid: Option<NonZeroU32>,
    /// disable target measurement
    #[clap(long)]
    no_target: bool,
    /// the path of the target executable
    #[clap(long, group = "binary-target")]
    target_path: Option<PathBuf>,
    /// inherit the target environment variables from lading's environment
    #[clap(long, requires = "binary-target", action)]
    target_inherit_environment: bool,
    /// additional environment variables to apply to the target, format
    /// KEY=VAL,KEY2=VAL
    #[clap(long, requires = "binary-target")]
    target_environment_variables: Option<CliKeyValues>,
    /// arguments for the target executable
    #[clap(requires = "binary-target")]
    target_arguments: Vec<String>,
    /// the path to write target's stdout
    #[clap(long, default_value_t = default_target_behavior(), requires = "binary-target")]
    target_stdout_path: Behavior,
    /// the path to write target's stderr
    #[clap(long, default_value_t = default_target_behavior(), requires = "binary-target")]
    target_stderr_path: Behavior,
    /// the maximum amount of RSS bytes the target may consume before lading backs off load
    #[clap(long)]
    target_rss_bytes_limit: Option<byte_unit::Byte>,
    /// path on disk to write captures, will override prometheus-addr if both
    /// are set
    #[clap(long)]
    capture_path: Option<String>,
    /// address to bind prometheus exporter to, will be overridden by
    /// capture-path if both are set
    #[clap(long)]
    prometheus_addr: Option<String>,
    /// the maximum time to wait, in seconds, for controlled shutdown
    #[clap(long, default_value_t = 30)]
    max_shutdown_delay: u16,
    /// the time, in seconds, to run the target and collect samples about it
    #[clap(long, default_value_t = 120)]
    experiment_duration_seconds: u32,
    /// the time, in seconds, to allow the target to run without collecting
    /// samples
    #[clap(long, default_value_t = 30)]
    warmup_duration_seconds: u32,
    /// whether to ignore inspector configuration, if present, and not run the inspector
    #[clap(long)]
    disable_inspector: bool,
    /// Extra sub commands
    #[clap(subcommand)]
    extracmds: Option<ExtraCommands>,
}

#[derive(Subcommand, Debug)]
#[clap(hide = true)]
enum ExtraCommands {
    ProcessTreeGen(ProcessTreeGen),
}

#[derive(Parser, Debug)]
#[clap(group(
    ArgGroup::new("config")
        .required(true)
        .args(&["config-path", "config-content"]),
))]
struct ProcessTreeGen {
    /// path on disk to the configuration file
    #[clap(long)]
    config_path: Option<PathBuf>,
    /// string repesanting the configuration
    #[clap(long)]
    config_content: Option<String>,
}

fn get_config(ops: &Opts) -> Config {
    let contents = if let Ok(env_var_value) = env::var("LADING_CONFIG") {
        debug!("Using config from env var 'LADING_CONFIG'");
        env_var_value
    } else {
        debug!(
            "Attempting to open configuration file at: {}",
            ops.config_path
        );
        let mut file: std::fs::File = std::fs::OpenOptions::new()
            .read(true)
            .open(&ops.config_path)
            .unwrap_or_else(|_| {
                panic!("Could not open configuration file at: {}", &ops.config_path)
            });
        let mut contents = String::new();
        file.read_to_string(&mut contents).unwrap();

        contents
    };

    let mut config: Config = serde_yaml::from_str(&contents).unwrap();

    if let Some(rss_bytes_limit) = ops.target_rss_bytes_limit {
        target::Meta::set_rss_bytes_limit(rss_bytes_limit).unwrap();
    }
    let target = if ops.no_target {
        None
    } else if let Some(pid) = ops.target_pid {
        Some(target::Config::Pid(target::PidConfig { pid }))
    } else if let Some(path) = &ops.target_path {
        Some(target::Config::Binary(target::BinaryConfig {
            command: path.clone(),
            arguments: ops.target_arguments.clone(),
            inherit_environment: ops.target_inherit_environment,
            environment_variables: ops
                .target_environment_variables
                .clone()
                .unwrap_or_default()
                .inner,
            output: Output {
                stderr: ops.target_stderr_path.clone(),
                stdout: ops.target_stdout_path.clone(),
            },
        }))
    } else {
        unreachable!("clap ensures that exactly one target option is selected");
    };
    config.target = target;

    let options_global_labels = ops.global_labels.clone().unwrap_or_default();
    if let Some(ref prom_addr) = ops.prometheus_addr {
        config.telemetry = Telemetry::Prometheus {
            prometheus_addr: prom_addr.parse().unwrap(),
            global_labels: options_global_labels.inner,
        };
    } else if let Some(ref capture_path) = ops.capture_path {
        config.telemetry = Telemetry::Log {
            path: capture_path.parse().unwrap(),
            global_labels: options_global_labels.inner,
        };
    } else {
        match config.telemetry {
            Telemetry::Prometheus {
                ref mut global_labels,
                ..
            } => {
                for (k, v) in options_global_labels.inner {
                    global_labels.insert(k, v);
                }
            }
            Telemetry::Log {
                ref mut global_labels,
                ..
            } => {
                for (k, v) in options_global_labels.inner {
                    global_labels.insert(k, v);
                }
            }
        }
    }
    config
}

async fn inner_main(
    experiment_duration: Duration,
    warmup_duration: Duration,
    max_shutdown_delay: Duration,
    disable_inspector: bool,
    config: Config,
) {
    let shutdown = Shutdown::new();

    // Set up the telemetry sub-system.
    //
    // We support two methods to exflitrate telemetry about the target from rig:
    // a passive prometheus export and an active log file. Only one can be
    // active at a time.
    match config.telemetry {
        Telemetry::Prometheus {
            prometheus_addr,
            global_labels,
        } => {
            let mut builder = PrometheusBuilder::new().with_http_listener(prometheus_addr);
            for (k, v) in global_labels {
                builder = builder.add_global_label(k, v);
            }
            builder.install().unwrap();
        }
        Telemetry::Log {
            path,
            global_labels,
        } => {
            let mut capture_manager = CaptureManager::new(path, shutdown.clone()).await;
            capture_manager.install();
            for (k, v) in global_labels {
                capture_manager.add_global_label(k, v);
            }
            let _capmgr = tokio::spawn(capture_manager.run());
        }
    }

    // Set up the application servers. These are, depending on configuration:
    //
    // * the "generator" which pushes load into
    // * the "target" which is the measured system and might push load into
    // * the "blackhole" which may or may not exist.
    //
    // There is also, maybe:
    //
    // * the "inspector" which is a sub-process that users can rig to inspect
    //   the target.
    // * the "observer" which reads procfs on Linux and reports relevant process
    //   detail to the capture log

    let (tgt_snd, _tgt_rcv) = broadcast::channel(1);

    //
    // GENERATOR
    //
    for cfg in config.generator {
        let tgt_rcv = tgt_snd.subscribe();
        let generator_server = generator::Server::new(cfg, shutdown.clone()).unwrap();
        let _gsrv = tokio::spawn(generator_server.run(tgt_rcv));
    }

    //
    // INSPECTOR
    //
    if let Some(inspector_conf) = config.inspector {
        if !disable_inspector {
            let tgt_rcv = tgt_snd.subscribe();
            let inspector_server =
                inspector::Server::new(inspector_conf, shutdown.clone()).unwrap();
            let _isrv = tokio::spawn(inspector_server.run(tgt_rcv));
        }
    }

    //
    // BLACKHOLE
    //
    if let Some(cfgs) = config.blackhole {
        for cfg in cfgs {
            let blackhole_server = blackhole::Server::new(cfg, shutdown.clone()).unwrap();
            let _bsrv = tokio::spawn(async {
                match blackhole_server.run().await {
                    Ok(()) => debug!("blackhole shut down successfully"),
                    Err(err) => warn!("blackhole failed with {:?}", err),
                }
            });
        }
    }

    //
    // TARGET METRICS
    //
    if let Some(cfgs) = config.target_metrics {
        for cfg in cfgs {
            let metrics_server = target_metrics::Server::new(cfg, shutdown.clone());
            tokio::spawn(async {
                match metrics_server.run().await {
                    Ok(()) => debug!("target_metrics shut down successfully"),
                    Err(err) => warn!("target_metrics failed with {:?}", err),
                }
            });
        }
    }

    //
    // OBSERVER
    //
    // Observer is not used when there is no target.
    let tsrv = if let Some(target) = config.target {
        let obs_rcv = tgt_snd.subscribe();
        let observer_server = observer::Server::new(config.observer, shutdown.clone()).unwrap();
        let _osrv = tokio::spawn(observer_server.run(obs_rcv));

        //
        // TARGET
        //
        let target_server = target::Server::new(target, shutdown.clone());
        let tsrv = tokio::spawn(target_server.run(tgt_snd));
        futures::future::Either::Left(tsrv)
    } else {
        // Many lading servers synchronize on target startup.
        tgt_snd
            .send(None)
            .expect("unable to transmit startup sync signal, catastrophic failure");
        futures::future::Either::Right(futures::future::pending())
    };

    let experiment_sleep = async move {
        info!("target is running, now sleeping for warmup");
        sleep(warmup_duration).await;
        info!("warmup completed, collecting samples");
        sleep(experiment_duration).await;
    };

    tokio::select! {
        _ = signal::ctrl_c() => {
            info!("received ctrl-c");
            shutdown.signal().unwrap();
        },
        _ = experiment_sleep => {
            info!("experiment duration exceeded");
            shutdown.signal().unwrap();
        }
        res = tsrv => {
            match res {
                Ok(Err(e)) => {
                    error!("target shut down unexpectedly: {e}");
                    std::process::exit(1);
                }
                Ok(Ok(())) | Err(_) => {
                    // JoinError or a shutdown signal arrived
                    shutdown.signal().unwrap();
                }
            }
        }
    }
    info!(
        "Waiting for {} seconds for tasks to shutdown.",
        max_shutdown_delay.as_secs(),
    );
    shutdown.wait(max_shutdown_delay).await;
}

fn run_process_tree(opts: ProcessTreeGen) {
    let mut contents = String::new();

    if let Some(path) = opts.config_path {
        debug!(
            "Attempting to open configuration file at: {}",
            path.display()
        );
        let mut file: std::fs::File = std::fs::OpenOptions::new()
            .read(true)
            .open(&path)
            .unwrap_or_else(|_| panic!("Could not open configuration file at: {}", path.display()));

        file.read_to_string(&mut contents).unwrap();
    } else if let Some(str) = &opts.config_content {
        contents = str.to_string()
    } else {
        unreachable!("clap ensures that exactly one target option is selected");
    };

    match process_tree::get_config(&contents) {
        Ok(config) => {
            info!("Generating a process tree.");

            let mut rng = StdRng::from_seed(config.seed);
            let nodes = process_tree::generate_tree(&mut rng, &config);

            process_tree::spawn_tree(&nodes, config.process_sleep_ns.get());

            info!("Bye. :)");
        }
        Err(e) => panic!("invalide configuration: {}", e),
    }
}

fn run_extra_cmds(cmds: ExtraCommands) {
    match cmds {
        // This command will call fork and the process must be kept fork-safe up to this point.
        ExtraCommands::ProcessTreeGen(opts) => run_process_tree(opts),
    }
}

fn main() {
    tracing_subscriber::fmt::init();

    info!("Starting lading run.");
    let opts: Opts = Opts::parse();

    // handle extra commands
    if let Some(cmds) = opts.extracmds {
        run_extra_cmds(cmds);
        return;
    }

    let config = get_config(&opts);

    let experiment_duration = Duration::from_secs(opts.experiment_duration_seconds.into());
    let warmup_duration = Duration::from_secs(opts.warmup_duration_seconds.into());
    // The maximum shutdown delay is shared between `inner_main` and this
    // function, hence the divide by two.
    let max_shutdown_delay = Duration::from_secs(opts.max_shutdown_delay.into()) / 2;
    let disable_inspector = opts.disable_inspector;

    let runtime = Builder::new_multi_thread()
        .enable_io()
        .enable_time()
        .build()
        .unwrap();
    runtime.block_on(inner_main(
        experiment_duration,
        warmup_duration,
        max_shutdown_delay,
        disable_inspector,
        config,
    ));
    // The splunk_hec generator spawns long running tasks that are not plugged
    // into the shutdown mechanism we have here. This is a bug and needs to be
    // addressed. However as a workaround we explicitly shutdown the
    // runtime. Even when the splunk_hec issue is addressed we'll continue this
    // practice as it's a reasonable safeguard.
    info!(
        "Shutting down runtime with a {} second delay. May leave orphaned tasks.",
        max_shutdown_delay.as_secs(),
    );
    runtime.shutdown_timeout(max_shutdown_delay);
    info!("Bye. :)");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_key_values_deserializes_empty_string_to_empty_set() {
        let val = "";
        let deser = CliKeyValues::from_str(val);
        let deser = deser.unwrap().to_string();
        assert_eq!(deser, "");
    }

    #[test]
    fn cli_key_values_deserializes_kv_list() {
        let val = "first=one,second=two";
        let deser = CliKeyValues::from_str(val);
        let deser = deser.unwrap().to_string();
        // CliKeyValues does not preserve order. That's okay! It's just less
        // convenient to assert against.
        assert!(deser == "first=one,second=two," || deser == "second=two,first=one,");
    }

    #[test]
    fn cli_key_values_deserializes_kv_list_trailing_comma() {
        let val = "first=one,";
        let deser = CliKeyValues::from_str(val);
        let deser = deser.unwrap().to_string();
        assert_eq!(deser, "first=one,");
    }
}
