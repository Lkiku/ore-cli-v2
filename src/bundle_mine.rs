use std::{fs, sync::Arc, time::Instant};
use std::collections::HashMap;
use drillx::{equix, Hash, Solution};
use ore::state::Proof;
use solana_sdk::compute_budget::ComputeBudgetInstruction;
use solana_sdk::signature::{Keypair, Signer};
use solana_sdk::signer::EncodableKey;
use solana_sdk::transaction::Transaction;
use tokio::sync::RwLock;
use crate::{args::BundleMineArgs, jito, Miner};
use crate::jito::{JitoTips, subscribe_jito_tips};
use crate::send_and_confirm::ComputeBudget;
use crate::utils::{get_proof, proof_pubkey};

impl Miner {
    pub async fn bundle_mine(&self, args: &BundleMineArgs) {
        let signers = Self::read_keys(&args.key_folder);
        let tips = Arc::new(RwLock::new(JitoTips::default()));

        subscribe_jito_tips(tips.clone());

        let results: Vec<_> = signers.iter().map(|keypair| {
            let keypair = keypair.clone();
            async move {
                let proof = get_proof(&self.rpc_client, keypair.pubkey()).await;
                let cutoff_time = self.get_cutoff(proof, args.buffer_time).await;

                let solution = Self::single_mine(proof, cutoff_time, args.min_difficulty).await;
                (keypair, solution)
            }
        }).collect::<Vec<_>>();

        let (hash, _slot) = self.rpc_client
            .get_latest_blockhash_with_commitment(self.rpc_client.commitment())
            .await
            .unwrap();

        let mut txs = Vec::with_capacity(results.len());
        for (keypair, solution) in results {
            let mut ixs = vec![];
            ixs.push(ore::instruction::mine(
                keypair.pubkey(),
                ore::BUS_ADDRESSES[0 as usize],
                solution,
            ));
            // Set compute units
            let mut final_ixs = vec![];

            final_ixs.push(ComputeBudgetInstruction::set_compute_unit_limit(500_000));

            final_ixs.push(ComputeBudgetInstruction::set_compute_unit_price(
                self.priority_fee,
            ));
            final_ixs.extend_from_slice(&ixs);
            let mut tx = Transaction::new_with_payer(&ixs, Some(&keypair.pubkey()));
            // Sign tx
            tx.sign(&[&keypair], hash);

            txs.push(tx);
        }

        jito::send_bundle(txs).await;
    }

    pub fn read_keys(key_folder: &str) -> Vec<Keypair> {
        fs::read_dir(key_folder)
            .expect("Failed to read key folder")
            .map(|entry| {
                let path = entry.expect("Failed to read entry").path();

                Keypair::read_from_file(&path).unwrap_or_else(|_| panic!("Failed to read keypair from {:?}", path))
            })
            .collect::<Vec<_>>()
    }

    fn single_mine(proof: Proof, cutoff_time: u64, min_difficulty:u32) -> Solution {
        if !min_difficulty.gt(&ore::MIN_DIFFICULTY) { panic!("min_difficulty is less than the minimum difficulty of the contract") }
        let proof = proof.clone();
        let mut memory = equix::SolverMemory::new();
        // move || {
        let timer = Instant::now();
        let mut nonce = 0u64;
        let mut best_nonce = nonce;
        let mut best_difficulty = 0;
        let mut best_hash = Hash::default();
        loop {
            // Create hash
            if let Ok(hx) = drillx::hash_with_memory(
                &mut memory,
                &proof.challenge,
                &nonce.to_le_bytes(),
            ) {
                let difficulty = hx.difficulty();
                if difficulty.gt(&min_difficulty) {
                    tracing::info!(
                        "Nonce: {} Difficulty: {}",
                        nonce,
                        difficulty
                    )
                }
                if difficulty.gt(&best_difficulty) {
                    best_nonce = nonce;
                    best_difficulty = difficulty;
                    best_hash = hx;
                }
            }

            // Exit if time has elapsed
            if nonce % 128 == 0 {

                    if best_difficulty.gt(&min_difficulty) {
                        // Mine until min difficulty has been met
                        break;
                    }
                if timer.elapsed().as_secs().ge(&cutoff_time) {
                    break;
                } 
                // else if i == 0 {
                //     tracing::info!(format!(
                //         "Mining... ({} sec remaining)",
                //         cutoff_time.saturating_sub(timer.elapsed().as_secs()),
                //     ));
                // }
            }

            // Increment nonce
            nonce += 1;
        }

        Solution::new(best_hash.d, best_nonce.to_le_bytes())
    }
}

