use std::fmt::Formatter;
use std::sync::Arc;
use std::time::{Duration, Instant};
use std::vec;
use log::{error, info, warn};
use colored::Colorize;
use futures_util::StreamExt;
use rand::Rng;
use serde::{de, Deserialize};
use serde_json::{json, Value};
use solana_client::client_error::{ClientError, ClientErrorKind};
use solana_program::{
    instruction::Instruction,
    native_token::{lamports_to_sol, sol_to_lamports},
};
use solana_program::pubkey::Pubkey;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_rpc_client::spinner;
use solana_sdk::{compute_budget::ComputeBudgetInstruction, pubkey, signature::{Signature, Signer}, transaction::Transaction};
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::transaction::VersionedTransaction;
use thiserror::Error;
use tonic::{
    codegen::{Body, Bytes, StdError},
    transport,
    Response, Status, Streaming,
};
use jito_protos::{
    bundle::{
        bundle_result::Result as BundleResultType, rejected::Reason, Accepted, Bundle,
        BundleResult, InternalError, SimulationFailure, StateAuctionBidRejected,
        WinningBatchBidRejected,
    },
    convert::proto_packet_from_versioned_tx,
    searcher::{
        searcher_service_client::SearcherServiceClient, SendBundleRequest, SendBundleResponse,
    },
};
use tokio::time::{sleep, timeout};
use tokio::{sync::RwLock, task::JoinHandle};
use jito_protos::searcher::{NextScheduledLeaderRequest, SubscribeBundleResultsRequest};
use crate::Miner;
use crate::send_and_confirm::ComputeBudget;


pub const JITO_RECIPIENTS: [Pubkey; 8] = [
    // mainnet
    pubkey!("96gYZGLnJYVFmbjzopPSU6QiEV5fGqZNyN9nmNhvrZU5"),
    pubkey!("HFqU5x63VTqvQss8hp11i4wVV8bD44PvwucfZ2bU7gRe"),
    pubkey!("Cw8CFyM9FkoMi7K7Crf6HNQqf4uEMzpKw6QNghXLvLkY"),
    pubkey!("ADaUMid9yfUytqMBgopwjb2DTLSokTSzL1zt6iGPaS49"),
    pubkey!("DfXygSm4jCyNCybVYYK6DwvWqjKee8pbDmJGcLWNDXjh"),
    pubkey!("ADuUkR4vqLUMWXxW9gh6D6L8pMSawimctcNZ5pGwDcEt"),
    pubkey!("DttWaMuVvTiduZRnguLF7jNxTgiMBZ1hyAumKUiL2KRL"),
    pubkey!("3AVi9Tg9Uo68tJfuvoKvqKNWKkC5wPdSSdeBnizKZ6jT"),

    // testnet
    // pubkey!("4xgEmT58RwTNsF5xm2RMYCnR1EVukdK8a1i2qFjnJFu3"),
    // pubkey!("EoW3SUQap7ZeynXQ2QJ847aerhxbPVr843uMeTfc9dxM"),
    // pubkey!("9n3d1K5YD2vECAbRFhFFGYNNjiXtHXJWn9F31t89vsAV"),
    // pubkey!("B1mrQSpdeMU9gCvkJ6VsXVVoYjRGkNA7TtjMyqxrhecH"),
    // pubkey!("ARTtviJkLLt6cHGQDydfo1Wyk6M4VGZdKZ2ZhdnJL336"),
    // pubkey!("9ttgPBBhRYFuQccdR1DSnb7hydsWANoDsV3P9kaGMCEh"),
    // pubkey!("E2eSqe33tuhAHKTrwky5uEjaVqnb2T9ns6nHHUrN8588"),
    // pubkey!("aTtUk2DHgLhKZRDjePq6eiHRKC1XXFMBiSUfQ2JNDbN"),

];

#[derive(Debug, Error)]
pub enum BlockEngineConnectionError {
    #[error("transport error {0}")]
    TransportError(#[from] transport::Error),
    #[error("client error {0}")]
    ClientError(#[from] Status),
}

#[derive(Debug, Error)]
pub enum BundleRejectionError {
    #[error("bundle lost state auction, auction: {0}, tip {1} lamports")]
    StateAuctionBidRejected(String, u64),
    #[error("bundle won state auction but failed global auction, auction {0}, tip {1} lamports")]
    WinningBatchBidRejected(String, u64),
    #[error("bundle simulation failure on tx {0}, message: {1:?}")]
    SimulationFailure(String, Option<String>),
    #[error("internal error {0}")]
    InternalError(String),
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
pub struct JitoTips {
    #[serde(rename = "landed_tips_25th_percentile")]
    pub p25_landed: f64,

    #[serde(rename = "landed_tips_50th_percentile")]
    pub p50_landed: f64,

    #[serde(rename = "landed_tips_75th_percentile")]
    pub p75_landed: f64,

    #[serde(rename = "landed_tips_95th_percentile")]
    pub p95_landed: f64,

    #[serde(rename = "landed_tips_99th_percentile")]
    pub p99_landed: f64,
}

impl JitoTips {
    pub fn _p50(&self) -> u64 {
        (self.p50_landed * 1e9f64) as u64
    }

    pub fn p25(&self) -> u64 {
        (self.p25_landed * 1e9f64) as u64
    }
}

impl std::fmt::Display for JitoTips {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "tips(p25={},p50={},p75={},p95={},p99={})",
            (self.p25_landed * 1e9f64) as u64,
            (self.p50_landed * 1e9f64) as u64,
            (self.p75_landed * 1e9f64) as u64,
            (self.p95_landed * 1e9f64) as u64,
            (self.p99_landed * 1e9f64) as u64
        )
    }
}

pub type BlockEngineConnectionResult<T> = Result<T, BlockEngineConnectionError>;

#[derive(Debug, Deserialize)]
pub struct JitoResponse<T> {
    pub result: T,
}

impl Miner {
    pub async fn jito_send_and_confirm(
        &self,
        ixs: &[Instruction],
        compute_budget: ComputeBudget,
        _skip_confirm: bool,
        tips: Arc<RwLock<JitoTips>>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let progress_bar = spinner::new_progress_bar();
        let signer = self.signer();
        let client = self.rpc_client.clone();
        let mut jito_client = self.jito_client.clone();

        // Return error, if balance is zero
        if let Ok(balance) = client.get_balance(&signer.pubkey()).await {
            if balance <= sol_to_lamports(crate::send_and_confirm::MIN_SOL_BALANCE) {
                panic!(
                    "{} Insufficient balance: {} SOL\nPlease top up with at least {} SOL",
                    "ERROR".bold().red(),
                    lamports_to_sol(balance),
                    crate::send_and_confirm::MIN_SOL_BALANCE
                );
            }
        }

        // Set compute units
        let mut final_ixs = vec![];
        match compute_budget {
            ComputeBudget::Dynamic => {
                // TODO simulate
                final_ixs.push(ComputeBudgetInstruction::set_compute_unit_limit(1_400_000))
            }
            ComputeBudget::Fixed(cus) => {
                final_ixs.push(ComputeBudgetInstruction::set_compute_unit_limit(cus))
            }
        }
        final_ixs.push(ComputeBudgetInstruction::set_compute_unit_price(
            self.priority_fee,
        ));
        final_ixs.extend_from_slice(ixs);

        let mut tip = self.priority_fee.clone();
        if self.max_adaptive_tip > 0 {
            let tips = *tips.read().await;

            if tips.p25() > 0 {
                tip = self.max_adaptive_tip.min(tips.p25() + self.priority_fee);
            }
        }
        final_ixs.push(build_bribe_ix(&signer.pubkey(), tip));

        let mut bundle_results_subscription = jito_client
            .subscribe_bundle_results(SubscribeBundleResultsRequest {})
            .await
            .expect("subscribe to bundle results")
            .into_inner();

        // wait for jito-solana leader slot
        let mut is_leader_slot = false;
        while !is_leader_slot {
            let next_leader = jito_client
                .get_next_scheduled_leader(NextScheduledLeaderRequest {
                    regions: "tokyo,amsterdam,frankfurt,ny,slc".split(",").map(|s| s.to_string()).collect(),
                })
                .await
                .expect("gets next scheduled leader")
                .into_inner();
            let num_slots = next_leader.next_leader_slot - next_leader.current_slot;
            is_leader_slot = num_slots <= 2;
            info!(
                    "next jito leader slot in {num_slots} slots in {}",
                    next_leader.next_leader_region
                );
            sleep(Duration::from_millis(500)).await;
        }
        // Build tx
        let mut tx = Transaction::new_with_payer(&final_ixs, Some(&signer.pubkey()));

        // Sign tx
        let (hash, _slot) = client
            .get_latest_blockhash_with_commitment(self.rpc_client.commitment())
            .await
            .unwrap();

        tx.sign(&[&signer], hash);
        let txs: Vec<_> = vec![VersionedTransaction::from(tx)];


        let mut attempts = 1;
        loop {
            progress_bar.set_message(format!("Submitting transaction... (attempt {})", attempts));
            match send_bundle_with_confirmation(
                &txs,
                &client,
                &mut jito_client,
                &mut bundle_results_subscription,
            )
                .await {
                Ok(()) => {
                    return Ok(());
                }

                Err(e) => {
                    if let Some(bundle_error) = e.downcast_ref::<BundleRejectionError>() {
                        match bundle_error {
                            BundleRejectionError::StateAuctionBidRejected(auction_id, tip) => {
                                warn!(
                                    "bundle auction rejected, auction: {}, tip {} lamports",
                                    auction_id, tip
                                );
                            }

                            BundleRejectionError::InternalError(msg) => {
                                warn!(
                                    "{}",
                                    msg
                                );
                            }

                            BundleRejectionError::SimulationFailure(tx_signature, msg) => {
                                warn!(
                                    "bundle simulation failure on tx {}, message: {:?}",
                                    tx_signature, msg
                                );
                            }

                            _ => {
                                panic!("Sending bundle failed: {e:?}");
                            }
                        }
                    } else if let Some(status) = e.downcast_ref::<tonic::Status>(){
                        if status.message() != "bundle contains an already processed transaction" {
                            panic!("Sending bundle failed: {e:?}");
                        }
                    } else {
                        panic!("Sending bundle failed: {e:?}");
                    }
                }
            }

            // Retry
            sleep(Duration::from_millis(400 * ( attempts + 1 ))).await;

            attempts += 1;
            if attempts > 5 {
                progress_bar.finish_with_message(format!("{}: Max retries", "ERROR".bold().red()));
                return Err(Box::new(ClientError {
                    request: None,
                    kind: ClientErrorKind::Custom("Max retries".to_string()),
                }));
            }
        }


    }


}

async fn _make_jito_request<T>(method: &'static str, params: Value) -> eyre::Result<T>
where
    T: de::DeserializeOwned,
{
    let response = reqwest::Client::new()
        .post("https://ny.testnet.block-engine.jito.wtf/api/v1/bundles")
        .header("Content-Type", "application/json")
        .json(&json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}))
        .send()
        .await;

    let response = match response {
        Ok(response) => response,
        Err(err) => eyre::bail!("fail to send request: {err}"),
    };

    let status = response.status();
    let text = match response.text().await {
        Ok(text) => text,
        Err(err) => eyre::bail!("fail to read response content: {err:#}"),
    };

    if !status.is_success() {
        eyre::bail!("status code: {status}, response: {text}");
    }

    let response: T = match serde_json::from_str(&text) {
        Ok(response) => response,
        Err(err) => eyre::bail!("fail to deserialize response: {err:#}, response: {text}, status: {status}"),
    };

    Ok(response)
}

pub fn build_bribe_ix(pubkey: &Pubkey, value: u64) -> solana_sdk::instruction::Instruction {
    solana_sdk::system_instruction::transfer(pubkey, &JITO_RECIPIENTS[rand::thread_rng().gen_range(0..JITO_RECIPIENTS.len())], value)
}

pub async fn send_bundle_with_confirmation<T>(
    transactions: &[VersionedTransaction],
    rpc_client: &RpcClient,
    searcher_client: &mut SearcherServiceClient<T>,
    bundle_results_subscription: &mut Streaming<BundleResult>,
) -> Result<(), Box<dyn std::error::Error>>
where
    T: tonic::client::GrpcService<tonic::body::BoxBody> + Send + 'static + Clone,
    T::Error: Into<StdError>,
    T::ResponseBody: Body<Data = Bytes> + Send + 'static,
    <T::ResponseBody as Body>::Error: Into<StdError> + Send,
    <T as tonic::client::GrpcService<tonic::body::BoxBody>>::Future: std::marker::Send,
{
    let bundle_signatures: Vec<Signature> =
        transactions.iter().map(|tx| tx.signatures[0]).collect();

    let result = send_bundle_no_wait(transactions, searcher_client).await?;

    // grab uuid from block engine + wait for results
    let uuid = result.into_inner().uuid;
    info!("Bundle sent. UUID: {:?}", uuid);

    info!("Waiting for 5 seconds to hear results...");
    let mut time_left = 5000;
    while let Ok(Some(Ok(results))) = timeout(
        Duration::from_millis(time_left),
        bundle_results_subscription.next(),
    )
        .await
    {
        let instant = Instant::now();
        info!("bundle results: {:?}", results);
        match results.result {
            Some(BundleResultType::Accepted(Accepted {
                                                slot: _s,
                                                validator_identity: _v,
                                            })) => {}
            Some(BundleResultType::Rejected(rejected)) => {
                match rejected.reason {
                    Some(Reason::WinningBatchBidRejected(WinningBatchBidRejected {
                                                             auction_id,
                                                             simulated_bid_lamports,
                                                             msg: _,
                                                         })) => {
                        return Err(Box::new(BundleRejectionError::WinningBatchBidRejected(
                            auction_id,
                            simulated_bid_lamports,
                        )))
                    }
                    Some(Reason::StateAuctionBidRejected(StateAuctionBidRejected {
                                                             auction_id,
                                                             simulated_bid_lamports,
                                                             msg: _,
                                                         })) => {
                        return Err(Box::new(BundleRejectionError::StateAuctionBidRejected(
                            auction_id,
                            simulated_bid_lamports,
                        )))
                    }
                    Some(Reason::SimulationFailure(SimulationFailure { tx_signature, msg })) => {
                        if let Some(message) = msg {
                            if message != "This transaction has already been processed" {
                                error!("{}" ,message);
                                return Err(Box::new(BundleRejectionError::SimulationFailure(
                                    tx_signature,
                                    Some(message),
                                )));
                            }
                        }
                    }
                    Some(Reason::InternalError(InternalError { msg })) => {
                        return Err(Box::new(BundleRejectionError::InternalError(msg)))
                    }
                    _ => {}
                };
            }
            _ => {}
        }
        time_left -= instant.elapsed().as_millis() as u64;
    }

    let futs: Vec<_> = bundle_signatures
        .iter()
        .map(|sig| {
            rpc_client.get_signature_status_with_commitment(sig, CommitmentConfig::processed())
        })
        .collect();
    let results = futures_util::future::join_all(futs).await;
    if !results.iter().all(|r| matches!(r, Ok(Some(Ok(()))))) {
        warn!("Transactions in bundle did not land");
        return Err(Box::new(BundleRejectionError::InternalError(
            "Searcher service did not provide bundle status in time".into(),
        )));
    }
    info!("Bundle landed successfully");
    let url: String = rpc_client.url();
    let cluster = if url.contains("testnet") {
        "testnet"
    } else if url.contains("devnet") {
        "devnet"
    } else {
        "mainnet"
    };
    for sig in bundle_signatures.iter() {
        info!("https://solscan.io/tx/{}?cluster={}", sig, cluster);
    }
    Ok(())
}

pub async fn send_bundle_no_wait<T>(
    transactions: &[VersionedTransaction],
    searcher_client: &mut SearcherServiceClient<T>,
) -> Result<Response<SendBundleResponse>, Status>
where
    T: tonic::client::GrpcService<tonic::body::BoxBody> + Send + 'static + Clone,
    T::Error: Into<StdError>,
    T::ResponseBody: Body<Data = Bytes> + Send + 'static,
    <T::ResponseBody as Body>::Error: Into<StdError> + Send,
    <T as tonic::client::GrpcService<tonic::body::BoxBody>>::Future: std::marker::Send,
{
    // convert them to packets + send over
    let packets: Vec<_> = transactions
        .iter()
        .map(proto_packet_from_versioned_tx)
        .collect();

    searcher_client
        .send_bundle(SendBundleRequest {
            bundle: Some(Bundle {
                header: None,
                packets,
            }),
        })
        .await
}

pub async fn subscribe_jito_tips(tips: Arc<RwLock<JitoTips>>) -> JoinHandle<()> {
    tokio::spawn({
        let tips = tips.clone();
        async move {
            let url = "ws://bundles-api-rest.jito.wtf/api/v1/bundles/tip_stream";

            loop {
                let stream = match tokio_tungstenite::connect_async(url).await {
                    Ok((ws_stream, _)) => ws_stream,
                    Err(err) => {
                        tracing::error!("fail to connect to jito tip stream: {err:#}");
                        tokio::time::sleep(Duration::from_secs(5)).await;
                        continue;
                    }
                };

                let (_, read) = stream.split();

                read.for_each(|message| async {
                    let data = match message {
                        Ok(data) => data.into_data(),
                        Err(err) => {
                            tracing::error!("fail to read jito tips message: {err:#}");
                            return;
                        }
                    };

                    let data = match serde_json::from_slice::<Vec<JitoTips>>(&data) {
                        Ok(t) => t,
                        Err(err) => {
                            tracing::info!("fail to parse jito tips: {err:#}");
                            return;
                        }
                    };

                    if data.is_empty() {
                        return;
                    }

                    *tips.write().await = *data.first().unwrap();
                })
                    .await;

                tracing::info!("jito tip stream disconnected, retries in 5 seconds");
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    })
}

