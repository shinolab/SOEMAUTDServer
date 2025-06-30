#![allow(non_snake_case)]

mod log_formatter;

use std::net::SocketAddr;
use std::num::{NonZeroU64, NonZeroUsize};

use log_formatter::LogFormatter;

use autd3_core::link::{Ack, Link, TxMessage};
use autd3_core::sleep::{Sleep, SpinSleeper, SpinWaitSleeper, StdSleeper};
use autd3_link_soem::{SOEM, SOEMOption};
use autd3_protobuf::*;

use clap::{Args, Parser, Subcommand, ValueEnum};

use tokio::{
    runtime::Runtime,
    sync::{RwLock, mpsc},
};
use tonic::{Request, Response, Status, transport::Server};

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, ValueEnum)]
enum TimerStrategyArg {
    /// use std::time::sleep
    StdSleep,
    /// use spin_sleep wait
    SpinSleep,
    /// use spin loop
    SpinWait,
}

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
#[command(
    help_template = "Author: {author-with-newline} {about-section}Version: {version} \n\n {usage-heading} {usage} \n\n {all-args} {tab}"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Args)]
struct Arg {
    /// Interface name
    #[clap(short = 'i', long = "ifname", default_value = "")]
    ifname: String,
    /// Client port
    #[clap(short = 'p', long = "port")]
    port: u16,
    /// Sync0 cycle time in us
    #[clap(short = 's', long = "sync0", default_value = "1000")]
    sync0: NonZeroU64,
    /// Send cycle time in us
    #[clap(short = 'c', long = "send", default_value = "1000")]
    send: NonZeroU64,
    /// Buffer size
    #[clap(short = 'b', long = "buffer_size", default_value = "32")]
    buf_size: NonZeroUsize,
    /// Timer strategy
    #[clap(short = 't', long = "sleeper", default_value = "spin-sleep")]
    timer_strategy: TimerStrategyArg,
    /// State check interval in ms
    #[clap(short = 'e', long = "state_check_interval", default_value = "100")]
    state_check_interval: NonZeroU64,
    /// Sync tolerance in us
    #[clap(long = "sync_tolerance", default_value = "1")]
    sync_tolerance: u64,
    /// Sync timeout in s
    #[clap(short = 'o', long = "sync_timeout", default_value = "10")]
    sync_timeout: u64,
    /// CPU affinity
    #[clap(short = 'a', long = "affinity")]
    affinity: Option<usize>,
}

#[derive(Subcommand)]
enum Commands {
    Run(Arg),
    /// List available interfaces
    List,
}

struct SOEMServer<
    F: Fn(usize, autd3_link_soem::Status) + Send + Sync + 'static,
    S: Sleep + Send + Sync + 'static,
> {
    num_dev: usize,
    soem: RwLock<SOEM<F, S>>,
}

#[tonic::async_trait]
impl<
    F: Fn(usize, autd3_link_soem::Status) + Send + Sync + 'static,
    S: Sleep + Send + Sync + 'static,
> ecat_server::Ecat for SOEMServer<F, S>
{
    async fn send_data(
        &self,
        request: Request<TxRawData>,
    ) -> Result<Response<SendResponse>, Status> {
        let tx = Vec::<TxMessage>::from_msg(request.into_inner())?;
        match Link::send(&mut *self.soem.write().await, tx) {
            Ok(_) => Ok(Response::new(SendResponse {})),
            Err(_) => Err(Status::internal("Failed to send data")),
        }
    }

    async fn read_data(&self, _: Request<ReadRequest>) -> Result<Response<RxMessage>, Status> {
        let mut rx = vec![autd3_core::link::RxMessage::new(0, Ack::new()); self.num_dev];
        match Link::receive(&mut *self.soem.write().await, &mut rx) {
            Ok(_) => Ok(Response::new(rx.into())),
            Err(_) => return Err(Status::internal("Failed to read data")),
        }
    }

    async fn close(&self, _: Request<CloseRequest>) -> Result<Response<CloseResponse>, Status> {
        self.soem
            .write()
            .await
            .clear_iomap()
            .map_err(|_| Status::internal("Failed to clear data"))?;
        Ok(Response::new(CloseResponse {}))
    }
}

async fn run<S: Sleep + Send + Sync + 'static>(
    addr: SocketAddr,
    mut rx: mpsc::Receiver<()>,
    option: SOEMOption,
    sleeper: S,
) -> anyhow::Result<()> {
    let mut soem = autd3_link_soem::SOEM::with_sleeper(
        |slave, status| {
            tracing::error!("slave [{}]: {}", slave, status);
            if status == autd3_link_soem::Status::Lost {
                std::process::exit(-1);
            }
        },
        option,
        sleeper,
    );
    soem.open(&autd3_core::geometry::Geometry::new(vec![]))?;
    let num_dev = soem.num_devices();

    tracing::info!("{} AUTDs found", num_dev);

    Server::builder()
        .add_service(ecat_server::EcatServer::new(SOEMServer {
            num_dev,
            soem: RwLock::new(soem),
        }))
        .serve_with_shutdown(addr, async {
            let _ = rx.recv().await;
        })
        .await?;

    Ok(())
}

async fn main_() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match &cli.command {
        Commands::List => {
            println!("Available interfaces:");
            let adapters = autd3_link_soem::EthernetAdapters::new();
            let name_len = adapters
                .iter()
                .map(|adapter| adapter.name().len())
                .max()
                .unwrap_or(0);
            adapters.into_iter().for_each(|adapter| {
                println!("\t{:name_len$}\t{}", adapter.name(), adapter.desc());
            });
        }
        Commands::Run(args) => {
            let port = args.port;
            let option = {
                let ifname = args.ifname.to_string();
                let sync0_cycle = args.sync0;
                let send_cycle = args.send;
                let state_check_interval = args.state_check_interval;
                let sync_tolerance = std::time::Duration::from_micros(args.sync_tolerance);
                let sync_timeout = std::time::Duration::from_secs(args.sync_timeout);
                let buf_size = args.buf_size;
                let affinity = args.affinity.map(|id| core_affinity::CoreId { id });
                SOEMOption {
                    buf_size,
                    ifname: ifname.clone(),
                    sync0_cycle: std::time::Duration::from_micros(sync0_cycle.get()),
                    send_cycle: std::time::Duration::from_micros(send_cycle.get()),
                    state_check_interval: std::time::Duration::from_millis(
                        state_check_interval.get(),
                    ),
                    sync_tolerance,
                    sync_timeout,
                    affinity,
                    ..Default::default()
                }
            };

            let (tx, rx) = mpsc::channel(1);
            ctrlc::set_handler(move || {
                let rt = Runtime::new().expect("failed to obtain a new Runtime object");
                rt.block_on(tx.send(())).unwrap();
            })
            .expect("Error setting Ctrl-C handler");

            let addr = format!("0.0.0.0:{port}").parse()?;
            tracing::info!("Waiting for client connection on {}", addr);

            tracing::info!("Starting SOEM server...");

            match args.timer_strategy {
                TimerStrategyArg::StdSleep => run(addr, rx, option, StdSleeper).await?,
                TimerStrategyArg::SpinSleep => {
                    run(addr, rx, option, SpinSleeper::default()).await?
                }
                TimerStrategyArg::SpinWait => run(addr, rx, option, SpinWaitSleeper).await?,
            }
        }
    }

    Ok(())
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt().event_format(LogFormatter).init();

    match main_().await {
        Ok(_) => {}
        Err(e) => {
            tracing::error!("{}", e);
            std::process::exit(-1);
        }
    }
}
