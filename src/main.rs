pub mod signer {
    tonic::include_proto!("signer");
}
use clap::Parser;
use ethers::{
    contract::abigen,
    core::types::Address,
    providers::{Http, Provider},
    types::U256,
};
use eyre::Result;
use log::{error, info, warn};
use signer::{signer_client::SignerClient, Empty};
use tokio::time::sleep;
use tonic::transport::Endpoint;

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration,
};

mod metrics;

abigen!(DaSigners, "./abis/IDASigners.json");

const DA_SIGNERS_ADDRESS: &str = "0x0000000000000000000000000000000000001000";

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// Block chain rpc
    #[arg(short, long, default_value_t=String::from("https://evmrpc-test-us.0g.ai"))]
    chain_rpc: String,

    /// Timeout for rpc
    #[arg(short, long, default_value_t = 30)]
    rpc_timeout_secs: u64,

    /// Monitor interval
    #[arg(short, long, default_value_t = 900)]
    interval_secs: u64,

    /// Prometheus exporter address
    #[arg(short, long, default_value_t=String::from("0.0.0.0:9185"))]
    prometheus_exporter_address: String,

    /// Optional log configuration file
    #[arg(short, long)]
    log: Option<String>,
}

fn ensure_scheme(url: &str) -> String {
    if url.starts_with("http://") || url.starts_with("https://") {
        url.to_string()
    } else {
        format!("http://{}", url) // Default to http if scheme is missing
    }
}

async fn run(da_signers_contract: &DaSigners<Provider<Http>>, timeout: Duration) -> Result<()> {
    let epoch = da_signers_contract.epoch_number().call().await?;
    info!("current epoch {:?}", epoch);
    metrics::EPOCH.set(epoch.as_u64() as i64);

    let quorum_cnt = da_signers_contract.quorum_count(epoch).call().await?;
    info!("current quorum counter {:?}", quorum_cnt);

    let mut signers = HashMap::new();
    let mut quorums = vec![];
    for quorum_id in 0..quorum_cnt.as_u64() {
        let quorum = da_signers_contract
            .get_quorum(epoch, U256::from(quorum_id))
            .call()
            .await?
            .into_iter()
            .fold(HashMap::new(), |mut acc, c| {
                *acc.entry(c).or_insert(0) += 1;
                acc
            });

        let unique_quorum: Vec<_> = quorum
            .keys()
            .copied()
            .filter(|addr| !signers.contains_key(addr))
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();

        info!(
            "quorums size: {}, unique quorums size: {}",
            quorum.len(),
            unique_quorum.len()
        );
        quorums.push(quorum);

        if !unique_quorum.is_empty() {
            let signer: HashMap<Address, String> = da_signers_contract
                .get_signer(unique_quorum)
                .call()
                .await?
                .into_iter()
                .map(|s| (s.signer, s.socket))
                .collect();
            signers.extend(signer);
        }
    }

    let mut valid_signers = HashSet::new();
    for (addr, socket) in signers.iter() {
        match Endpoint::new(ensure_scheme(socket)) {
            Ok(endpoint) => match endpoint
                .connect_timeout(timeout)
                .timeout(timeout)
                .connect()
                .await
            {
                Ok(channel) => {
                    let mut client = SignerClient::new(channel);
                    match client.get_status(tonic::Request::new(Empty {})).await {
                        Ok(resp) => {
                            let status_code = resp.into_inner().status_code;
                            if status_code != 200 {
                                warn!(
                                    "status code from {:?} {:?} is {:?}",
                                    addr, socket, status_code
                                );
                            } else {
                                valid_signers.insert(*addr);
                            }
                        }
                        Err(e) => {
                            warn!("failed to get status from {:?} {:?}: {:?}", addr, socket, e);
                        }
                    }
                }
                Err(e) => {
                    warn!("failed to connect {:?} {:?}: {:?}", addr, socket, e);
                }
            },
            Err(e) => {
                warn!(
                    "failed to create endpoint for {:?} {:?}: {:?}",
                    addr, socket, e
                );
            }
        }
    }

    info!(
        "unique signers {:?}, validate signers {:?}",
        signers.len(),
        valid_signers.len()
    );
    metrics::SIGNER_COUNTER
        .with_label_values(&["total"])
        .set(signers.len() as f64);
    metrics::SIGNER_COUNTER
        .with_label_values(&["valid"])
        .set(valid_signers.len() as f64);
    metrics::SIGNER_COUNTER
        .with_label_values(&["pct."])
        .set(valid_signers.len() as f64 / signers.len() as f64);

    quorums.into_iter().enumerate().for_each(|(idx, quorum)| {
        let mut slice = 0;
        let mut validate_slice = 0;

        for (addr, val) in quorum {
            slice += val;

            if valid_signers.contains(&addr) {
                validate_slice += val;
            }
        }

        info!(
            "quorum {}: total slice {}, validate slice {}",
            idx, slice, validate_slice
        );

        let id = idx.to_string();
        metrics::SLICE_COUNTER
            .with_label_values(&[id.as_str(), "total"])
            .set(slice as f64);
        metrics::SLICE_COUNTER
            .with_label_values(&[id.as_str(), "valid"])
            .set(validate_slice as f64);
        metrics::SLICE_COUNTER
            .with_label_values(&[id.as_str(), "pct."])
            .set(validate_slice as f64 / slice as f64);
    });

    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Args::parse();

    // init log
    let log_conf = cli.log.as_deref().unwrap_or("log4rs.yaml");
    log4rs::init_file(log_conf, Default::default())
        .map_err(|e| format!("failed to initialize log with config file: {:?}", e))
        .unwrap();

    prometheus_exporter::start(
        cli.prometheus_exporter_address
            .parse()
            .expect("failed to parse binding"),
    )?;

    let provider = Provider::new(Http::new_with_client(
        url::Url::parse(&cli.chain_rpc)?,
        reqwest::Client::builder()
            .timeout(Duration::from_secs(cli.rpc_timeout_secs))
            .connect_timeout(Duration::from_secs(cli.rpc_timeout_secs))
            .build()?,
    ));
    let client = Arc::new(provider);

    let contract_address: Address = DA_SIGNERS_ADDRESS.parse()?;
    let da_signers_contract = DaSigners::new(contract_address, client.clone());

    loop {
        if let Err(e) = run(
            &da_signers_contract,
            Duration::from_secs(cli.rpc_timeout_secs),
        )
        .await
        {
            sleep(Duration::from_secs(1)).await;
            error!("failed to run monitor: {:?}", e);
            continue;
        }

        sleep(Duration::from_secs(cli.interval_secs)).await;
    }
}
