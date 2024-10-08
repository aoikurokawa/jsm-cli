use std::{fs::File, path::PathBuf, time::Duration};

use anyhow::Context;
use chrono_crank::{
    vault_handler::VaultHandler, vault_update_state_tracker_handler::VaultUpdateStateTrackerHandler,
};
use clap::Parser;
use jito_bytemuck::AccountDeserialize;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::{pubkey::Pubkey, signature::read_keypair_file};

#[derive(Parser)]
struct Args {
    /// RPC URL for the cluster
    #[arg(short, long, env, default_value = "https://api.devnet.solana.com")]
    rpc_url: String,

    /// Path to keypair used to pay
    #[arg(long, env, default_value = "~/.config/solana/id.json")]
    keypair: PathBuf,

    /// Vault program ID (Pubkey as base58 string)
    #[arg(
        long,
        env,
        default_value = "34X2uqBhEGiWHu43RDEMwrMqXF4CpCPEZNaKdAaUS9jx"
    )]
    vault_program_id: Pubkey,

    /// Restaking program ID (Pubkey as base58 string)
    #[arg(
        long,
        env,
        default_value = "78J8YzXGGNynLRpn85MH77PVLBZsWyLCHZAXRvKaB6Ng"
    )]
    restaking_program_id: Pubkey,

    /// NCN
    #[arg(long)]
    ncn: Pubkey,
}

#[tokio::main]
async fn main() -> anyhow::Result<(), anyhow::Error> {
    let log_file = File::create("app.log").expect("create log file");

    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .target(env_logger::Target::Pipe(Box::new(log_file)))
        .init();

    let args = Args::parse();
    let rpc_client = RpcClient::new_with_timeout(args.rpc_url.clone(), Duration::from_secs(60));
    let payer = read_keypair_file(args.keypair).expect("read keypair file");

    let config_address =
        jito_vault_core::config::Config::find_program_address(&args.vault_program_id).0;

    let account = rpc_client
        .get_account(&config_address)
        .await
        .expect("Failed to read Jito vault config address");
    let config = jito_vault_core::config::Config::try_from_slice_unchecked(&account.data)
        .expect("Failed to deserialize Jito vault config");

    let vault_handler =
        VaultHandler::new(&args.rpc_url, &payer, args.vault_program_id, config_address);
    let handler = VaultUpdateStateTrackerHandler::new(
        &args.rpc_url,
        &payer,
        args.restaking_program_id,
        args.vault_program_id,
        config_address,
        config.epoch_length(),
    );

    let ncn_vault_tickets: Vec<Pubkey> = handler.get_ncn_vault_tickets(args.ncn).await?;
    let vaults = vault_handler.get_vaults(&ncn_vault_tickets).await?;

    let slot = rpc_client.get_slot().await.context("get slot")?;
    let epoch = slot / config.epoch_length();

    let vaults: Vec<Pubkey> = vaults
        .iter()
        .filter_map(|(pubkey, vault)| {
            // Initialize new tracker
            if vault.last_full_state_update_slot() / config.epoch_length() != epoch {
                Some(*pubkey)
            } else {
                None
            }
        })
        .collect();

    handler.initialize(&vaults, epoch).await?;

    let mut last_epoch = epoch;
    let mut close_failed = false;
    let mut count = 0;
    loop {
        let slot = rpc_client.get_slot().await.context("get slot")?;
        let epoch = slot / config.epoch_length();

        log::info!("Slot: {slot}, Current Epoch: {epoch}, Last Epoch: {last_epoch}");

        if epoch != last_epoch || (close_failed && count < 10) {
            let ncn_vault_tickets: Vec<Pubkey> = match handler.get_ncn_vault_tickets(args.ncn).await
            {
                Ok(v) => v,
                Err(_) => vaults.clone(),
            };
            let vaults = vault_handler.get_vaults(&ncn_vault_tickets).await?;
            let vaults: Vec<Pubkey> = vaults
                .iter()
                .filter_map(|(pubkey, vault)| {
                    // Initialize new tracker
                    if vault.last_full_state_update_slot() / config.epoch_length() != epoch {
                        Some(*pubkey)
                    } else {
                        None
                    }
                })
                .collect();

            // Close previous epoch's tracker
            match handler.close(&vaults, last_epoch).await {
                Ok(()) => {
                    // Initialize new tracker
                    handler.initialize(&vaults, epoch).await?;

                    last_epoch = epoch;
                    close_failed = false;
                    count = 0;
                }
                Err(e) => {
                    close_failed = true;
                    count += 1;

                    if count == 9 {
                        log::error!("Error: Failed to close tracker");
                        return Err(e);
                    }
                }
            }
        }

        // ---------- SLEEP (1 hour)----------
        tokio::time::sleep(Duration::from_secs(60 * 60)).await;
    }
}
