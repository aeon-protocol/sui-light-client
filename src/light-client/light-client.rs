// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: BSD-3-Clause-Clear

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use move_core_types::{account_address::AccountAddress, identifier::Identifier};
use object_store::path::Path;
use object_store::ObjectStore;
use sui_json_rpc_types::{
    SuiEvent, SuiObjectDataOptions, SuiTransactionBlockResponseOptions,
};

use sui_json_rpc_types::{EventFilter, ObjectChange};

use sui_rest_api::{CheckpointData, Client};
use sui_types::transaction::ObjectArg;
use sui_types::{
    base_types::{ObjectID, ObjectRef},
    committee::Committee,
    crypto::AuthorityQuorumSignInfo,
    message_envelope::Envelope,
    messages_checkpoint::{CertifiedCheckpointSummary, CheckpointSummary, EndOfEpochData},
    object::{Object, Owner},
};

use sui_config::genesis::Genesis;

use sui_package_resolver::Result as ResolverResult;
use sui_package_resolver::{Package, PackageStore, Resolver};
use sui_sdk::{SuiClientBuilder};

use clap::{Parser, Subcommand};
use std::thread::sleep;
use std::{fs, io::Write, path::PathBuf, str::FromStr};
use std::{io::Read, sync::Arc};

use sui_config::{sui_config_dir, SUI_KEYSTORE_FILENAME};
use move_core_types::language_storage::{StructTag, TypeTag};
use serde_json::{Value};
use shared_crypto::intent::Intent;
use sui_keys::keystore::{AccountKeystore, FileBasedKeystore};
use sui_sdk::{
    types::{
        programmable_transaction_builder::ProgrammableTransactionBuilder,
        quorum_driver_types::ExecuteTransactionRequestType,
        transaction::{Argument, Command, ProgrammableMoveCall, Transaction, TransactionData},
    },
};
use std::{collections::HashMap, sync::Mutex};

use log::info;
use object_store::parse_url;
use serde_json::json;
use url::Url;



/// A light client for the Sui blockchain
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Sets a custom config file
    #[arg(short, long, value_name = "FILE")]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<SCommands>,
}

struct RemotePackageStore {
    config: Config,
    cache: Mutex<HashMap<AccountAddress, Arc<Package>>>,
}
impl RemotePackageStore {
    pub fn new(config: Config) -> Self {
        Self {
            config,
            cache: Mutex::new(HashMap::new()),
        }
    }
}

#[async_trait]
impl PackageStore for RemotePackageStore {
    /// Read package contents. Fails if `id` is not an object, not a package, or is malformed in
    /// some way.
    async fn fetch(&self, id: AccountAddress) -> ResolverResult<Arc<Package>> {
        // Check if we have it in the cache
        if let Some(package) = self.cache.lock().unwrap().get(&id) {
            // info!("Fetch Package: {} cache hit", id);
            return Ok(package.clone());
        }

        info!("Fetch Package: {}", id);

        let object: Object = get_verified_object(&self.config, id.into()).await.unwrap();
        let package = Arc::new(Package::read_from_object(&object).unwrap());

        // Add to the cache
        self.cache.lock().unwrap().insert(id, package.clone());

        Ok(package)
    }
}

#[derive(Subcommand, Debug)]
enum SCommands {
    /// Sync all end-of-epoch checkpoints
    Init {
        #[arg(short, long, value_name = "TID")]
        ckp_id: u64,
    },

    Sync {},

    /// Checks a specific transaction using the light client
    Transaction {
        /// Transaction hash
        #[arg(short, long, value_name = "TID")]
        tid: String,
    },
}


// The config file for the light client including the root of trust genesis digest
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
struct Config {
    /// Full node url
    sui_full_node_url: String,

    dwallet_full_node_url: String,

    /// Checkpoint summary directory
    checkpoint_summary_dir: PathBuf,

    //  Genesis file name
    genesis_filename: PathBuf,

    /// Object store url
    object_store_url: String,

    /// GraphQL endpoint
    graphql_url: String,

    /// Sui deployed state proof package
    sui_deployed_state_proof_package: String,

    /// Dwallet registry object id
    dwltn_registry_object_id: String,

    /// Dwallet config object id
    dwltn_config_object_id: String,
}

impl Config {
    pub fn sui_rest_url(&self) -> String {
        format!("{}/rest", self.sui_full_node_url)
    }

    pub fn dwallet_full_node_url(&self) -> String {
        format!("{}", self.dwallet_full_node_url)
    }
}



// The list of checkpoints at the end of each epoch
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
struct CheckpointsList {
    // List of end of epoch checkpoints
    checkpoints: Vec<u64>,
}

fn read_checkpoint_list(config: &Config) -> anyhow::Result<CheckpointsList> {
    let mut checkpoints_path = config.checkpoint_summary_dir.clone();
    checkpoints_path.push("checkpoints.yaml");
    // Read the resulting file and parse the yaml checkpoint list
    let reader = fs::File::open(checkpoints_path.clone())?;
    Ok(serde_yaml::from_reader(reader)?)
}

fn read_checkpoint(
    config: &Config,
    seq: u64,
) -> anyhow::Result<Envelope<CheckpointSummary, AuthorityQuorumSignInfo<true>>> {
    read_checkpoint_general(config, seq, None)
}

fn read_checkpoint_general(
    config: &Config,
    seq: u64,
    path: Option<&str>,
) -> anyhow::Result<Envelope<CheckpointSummary, AuthorityQuorumSignInfo<true>>> {
    // Read the resulting file and parse the yaml checkpoint list
    let mut checkpoint_path = config.checkpoint_summary_dir.clone();
    if let Some(path) = path {
        checkpoint_path.push(path);
    }
    checkpoint_path.push(format!("{}.yaml", seq));
    let mut reader = fs::File::open(checkpoint_path.clone())?;
    let metadata = fs::metadata(&checkpoint_path)?;
    let mut buffer = vec![0; metadata.len() as usize];
    reader.read_exact(&mut buffer)?;
    bcs::from_bytes(&buffer).map_err(|_| anyhow!("Unable to parse checkpoint file"))
}

fn write_checkpoint(
    config: &Config,
    summary: &Envelope<CheckpointSummary, AuthorityQuorumSignInfo<true>>,
) -> anyhow::Result<()> {
    write_checkpoint_general(config, summary, None)
}

fn write_checkpoint_general(
    config: &Config,
    summary: &Envelope<CheckpointSummary, AuthorityQuorumSignInfo<true>>,
    path: Option<&str>,
) -> anyhow::Result<()> {
    // Write the checkpoint summary to a file
    let mut checkpoint_path = config.checkpoint_summary_dir.clone();
    if let Some(path) = path {
        checkpoint_path.push(path);
    }
    checkpoint_path.push(format!("{}.yaml", summary.sequence_number));
    let mut writer = fs::File::create(checkpoint_path.clone())?;
    let bytes =
        bcs::to_bytes(&summary).map_err(|_| anyhow!("Unable to serialize checkpoint summary"))?;
    writer.write_all(&bytes)?;
    Ok(())
}

fn write_checkpoint_list(
    config: &Config,
    checkpoints_list: &CheckpointsList,
) -> anyhow::Result<()> {
    // Write the checkpoint list to a file
    let mut checkpoints_path = config.checkpoint_summary_dir.clone();
    checkpoints_path.push("checkpoints.yaml");
    let mut writer = fs::File::create(checkpoints_path.clone())?;
    let bytes = serde_yaml::to_vec(&checkpoints_list)?;
    writer
        .write_all(&bytes)
        .map_err(|_| anyhow!("Unable to serialize checkpoint list"))
}

async fn download_checkpoint_summary(
    config: &Config,
    checkpoint_number: u64,
) -> anyhow::Result<CertifiedCheckpointSummary> {
    // Download the checkpoint from the server

    let url = Url::parse(&config.object_store_url)?;
    let (dyn_store, _store_path) = parse_url(&url).unwrap();
    let path = Path::from(format!("{}.chk", checkpoint_number));
    let response = dyn_store.get(&path).await?;
    let bytes = response.bytes().await?;
    let (_, blob) = bcs::from_bytes::<(u8, CheckpointData)>(&bytes)?;

    info!("Downloaded checkpoint summary: {}", checkpoint_number);
    Ok(blob.checkpoint_summary)
}

async fn query_last_checkpoint_of_epoch(config: &Config, epoch_id: u64) -> anyhow::Result<u64> {
    // GraphQL query to get the last checkpoint of an epoch
    let query = json!({
        "query": "query ($epochID: Int) { epoch(id: $epochID) { checkpoints(last: 1) { nodes { sequenceNumber } } } }",
        "variables": { "epochID": epoch_id }
    });

    // Submit the query by POSTing to the GraphQL endpoint
    let client = reqwest::Client::new();
    let resp = client
        .post(&config.graphql_url)
        .header("Content-Type", "application/json")
        .body(query.to_string())
        .send()
        .await
        .expect("Cannot connect to graphql")
        .text()
        .await
        .expect("Cannot parse response");

    // Parse the JSON response to get the last checkpoint of the epoch
    let v: Value = serde_json::from_str(resp.as_str()).expect("Incorrect JSON response");
    let checkpoint_number = v["data"]["epoch"]["checkpoints"]["nodes"][0]["sequenceNumber"]
        .as_u64()
        .unwrap();

    Ok(checkpoint_number)
}


/// Run binary search to for each end of epoch checkpoint that is missing
/// between the latest on the list and the latest checkpoint.
async fn sync_checkpoint_list_to_latest(config: &Config) -> anyhow::Result<()> {
    // Get the local checkpoint list
    let mut checkpoints_list: CheckpointsList = read_checkpoint_list(config)?;
    let latest_in_list = checkpoints_list
        .checkpoints
        .last()
        .ok_or(anyhow!("Empty checkpoint list"))?;

    println!("Latest in list: {}", latest_in_list);
    // Download the latest in list checkpoint
    let summary = download_checkpoint_summary(config, *latest_in_list)
        .await
        .context("Failed to download checkpoint")?;
    let mut last_epoch = summary.epoch();
    let mut last_checkpoint_seq = summary.sequence_number;

    // Download the very latest checkpoint
    let sui_client = SuiClientBuilder::default()
        .build(config.sui_full_node_url.as_str())
        .await
        .expect("Cannot connect to full node");

    let latest_seq = sui_client
        .read_api()
        .get_latest_checkpoint_sequence_number()
        .await
        .unwrap();
    let latest = download_checkpoint_summary(config, latest_seq).await?;
    println!("Latest: {}", latest.epoch());
    // Sequentially record all the missing end of epoch checkpoints numbers
    while last_epoch + 1 < latest.epoch() {
        let target_epoch = last_epoch + 1;
        let target_last_checkpoint_number =
            query_last_checkpoint_of_epoch(config, target_epoch).await?;

        // Add to the list
        checkpoints_list
            .checkpoints
            .push(target_last_checkpoint_number);
        write_checkpoint_list(config, &checkpoints_list)?;

        // Update
        last_epoch = target_epoch;

        println!(
            "Last Epoch: {} Last Checkpoint: {}",
            target_epoch, target_last_checkpoint_number
        );
    }

    Ok(())
}



async fn check_and_sync_checkpoints(config: &Config) -> anyhow::Result<()> {
    println!("Syncing checkpoints to latest");
    sync_checkpoint_list_to_latest(config)
        .await
        .context("Failed to sync checkpoints")?;
    println!("Synced checkpoints to latest");

    // Get the local checkpoint list
    let checkpoints_list: CheckpointsList = read_checkpoint_list(config)?;
    println!("Checkpoints: {:?}", checkpoints_list.checkpoints);

    // Load the genesis committee
    let mut genesis_path = config.checkpoint_summary_dir.clone();
    genesis_path.push(&config.genesis_filename);
    let mut genesis_committee = Genesis::load(&genesis_path)?.committee()?;
    genesis_committee.epoch = 1; // TOOD hack to make it work

    // Retrieve highest epoch committee id that was registered on dWallet newtwork
    let latest_registered_epoch_committee_id = retrieve_highest_epoch(config).await.unwrap_or(0);
    println!(
        "Latest registered checkpoint id: {}",
        latest_registered_epoch_committee_id
    );

    // Check the signatures of all checkpoints
    // And download any missing ones
    let mut prev_committee = genesis_committee;
    // let mut prev_committee_object_ref_dwltn = genesis_committee_object_ref_dwltn;
    for ckp_id in &checkpoints_list.checkpoints {
        // check if there is a file with this name ckp_id.yaml in the checkpoint_summary_dir
        let mut checkpoint_path = config.checkpoint_summary_dir.clone();
        checkpoint_path.push(format!("{}.yaml", ckp_id));

        // If file exists read the file otherwise download it from the server
        println!("Processing checkpoint: {}", ckp_id);
        let summary = if checkpoint_path.exists() {
            read_checkpoint(config, *ckp_id)?
        } else {
            // Download the checkpoint from the server
            println!("Downloading checkpoint: {}", ckp_id);
            download_checkpoint_summary(config, *ckp_id)
                .await
                .context("Failed to download checkpoint")?
        };
        println!("{}", summary.auth_sig().epoch);
        println!("{}", summary.data().epoch);

        summary.clone().try_into_verified(&prev_committee)?;
        println!("verified checkpoint");

        // Check if the checkpoint needs to be submitted to the dwallet network
        if (latest_registered_epoch_committee_id < summary.epoch()) {
            let mut ptb = ProgrammableTransactionBuilder::new();

            let prev_committee_object_id = retieve_epoch_committee_id_by_epoch(
                config,
                summary.epoch().checked_sub(1).unwrap(),
            )
            .await
            .unwrap();
            let prev_committee_object_ref_dwltn =
                get_object_ref_by_id(config, prev_committee_object_id)
                    .await
                    .unwrap();

            let registry_object_id =
                ObjectID::from_hex_literal(&config.dwltn_registry_object_id).unwrap();
            // retrieve highest shared version of the registry
            let dwallet_client = SuiClientBuilder::default()
                .build(config.dwallet_full_node_url())
                .await
                .unwrap();
            let res = dwallet_client
                .read_api()
                .get_object_with_options(
                    registry_object_id,
                    SuiObjectDataOptions::full_content().with_bcs(),
                )
                .await
                .unwrap();
            let registry_initial_shared_version = match res.owner().unwrap() {
                Owner::Shared {
                    initial_shared_version,
                } => initial_shared_version,
                _ => return Err(anyhow::anyhow!("Expected a Shared owner")),
            };

            let registry_arg = ptb
                .obj(ObjectArg::SharedObject {
                    id: registry_object_id,
                    initial_shared_version: registry_initial_shared_version,
                    mutable: true,
                })
                .unwrap();
            let prev_committee_arg = ptb
                .obj(ObjectArg::ImmOrOwnedObject(prev_committee_object_ref_dwltn))
                .unwrap();
            let new_checkpoint_summary_arg = ptb.pure(bcs::to_bytes(&summary).unwrap()).unwrap();

            let call = ProgrammableMoveCall {
                package: ObjectID::from_hex_literal(
                    "0x0000000000000000000000000000000000000000000000000000000000000003",
                )
                .unwrap(),
                module: Identifier::new("sui_state_proof").unwrap(),
                function: Identifier::new("submit_new_state_committee").unwrap(),
                type_arguments: vec![],
                arguments: vec![registry_arg, prev_committee_arg, new_checkpoint_summary_arg],
            };

            let dwallet_client = SuiClientBuilder::default()
                .build(config.dwallet_full_node_url())
                .await
                .unwrap();

            ptb.command(Command::MoveCall(Box::new(call)));

            let builder = ptb.finish();

            let gas_budget = 1000000000;
            let gas_price = dwallet_client
                .read_api()
                .get_reference_gas_price()
                .await
                .unwrap();

            let keystore =
                FileBasedKeystore::new(&sui_config_dir().unwrap().join(SUI_KEYSTORE_FILENAME))
                    .unwrap();

            let sender = *keystore.addresses_with_alias().first().unwrap().0;
            println!("sender: {}", sender);

            let coins = dwallet_client
                .coin_read_api()
                .get_coins(sender, None, None, None)
                .await
                .unwrap();
            let coin_gas = coins
                .data
                .into_iter()
                .max_by_key(|coin| coin.balance)
                .unwrap();

            let tx_data = TransactionData::new_programmable(
                sender,
                vec![coin_gas.object_ref()],
                builder,
                gas_budget,
                gas_price,
            );

            // 4) sign transaction
            let signature = keystore
                .sign_secure(&sender, &tx_data, Intent::sui_transaction())
                .unwrap();

            // 5) execute the transaction
            println!("Executing the transaction...");
            let transaction_response = dwallet_client
                .quorum_driver_api()
                .execute_transaction_block(
                    Transaction::from_data(tx_data, vec![signature]),
                    SuiTransactionBlockResponseOptions::full_content(),
                    Some(ExecuteTransactionRequestType::WaitForLocalExecution),
                )
                .await
                .unwrap();

            let object_changes = transaction_response.object_changes.unwrap();

            // println!("object changes: {}", object_changes);
            let committee_object_change = object_changes
                .iter()
                .filter(|object| match object {
                    ObjectChange::Created {
                        sender: _,
                        owner: _,
                        object_type: object_type,
                        object_id: _,
                        version: _,
                        digest: _,
                    } => object_type.to_string().contains("EpochCommittee"),
                    _ => false,
                })
                .next()
                .unwrap();

            // sleep 3 secs
            sleep(std::time::Duration::from_secs(5));
        }

        // Write the checkpoint summary to a file
        write_checkpoint(config, &summary)?;

        // Print the id of the checkpoint and the epoch number
        println!(
            "Epoch: {} Checkpoint ID: {}",
            summary.epoch(),
            summary.digest()
        );

        // Extract the new committee information
        if let Some(EndOfEpochData {
            next_epoch_committee,
            ..
        }) = &summary.end_of_epoch_data
        {
            let next_committee = next_epoch_committee.iter().cloned().collect();
            prev_committee = Committee::new(summary.epoch().saturating_add(1), next_committee);
        } else {
            return Err(anyhow!(
                "Expected all checkpoints to be end-of-epoch checkpoints"
            ));
        }
    }

    Ok(())
}



async fn get_verified_object(config: &Config, id: ObjectID) -> anyhow::Result<Object> {
    let sui_client: Arc<sui_sdk::SuiClient> = Arc::new(
        SuiClientBuilder::default()
            .build(config.sui_full_node_url.as_str())
            .await
            .unwrap(),
    );

    println!("Getting object: {}", id);

    let read_api = sui_client.read_api();
    let object_json = read_api
        .get_object_with_options(id, SuiObjectDataOptions::bcs_lossless())
        .await
        .expect("Cannot get object");
    let object = object_json
        .into_object()
        .expect("Cannot make into object data");
    let object: Object = object.try_into().expect("Cannot reconstruct object");

    Ok(object)
}


async fn retrieve_highest_epoch(config: &Config) -> anyhow::Result<u64> {
    let client = SuiClientBuilder::default()
        .build(config.dwallet_full_node_url.clone())
        .await
        .unwrap();

    let query = EventFilter::MoveModule {
        package: ObjectID::from_hex_literal(
            &"0x0000000000000000000000000000000000000000000000000000000000000003",
        )
        .unwrap(),
        module: Identifier::from_str(&"sui_state_proof").unwrap(),
    };

    let res = client
        .event_api()
        .query_events(query.clone(), Option::None, Option::None, true)
        .await
        .unwrap();
    let max = res
        .data
        .iter()
        .filter(|event| event.parsed_json.get("epoch").is_some())
        .filter(|event| event.parsed_json.get("registry_id").unwrap().as_str().unwrap() == config.dwltn_registry_object_id)
        .map(|event| {
            u64::from_str(event.parsed_json.get("epoch").unwrap().as_str().unwrap()).unwrap()
        })
        .max()
        .unwrap();
    return anyhow::Ok(max);
}

async fn retieve_epoch_committee_id_by_epoch(
    config: &Config,
    target_epoch: u64,
) -> anyhow::Result<ObjectID> {
    let client = SuiClientBuilder::default()
        .build(config.dwallet_full_node_url.clone())
        .await
        .unwrap();

    let query = EventFilter::MoveModule {
        package: ObjectID::from_hex_literal(
            &"0x0000000000000000000000000000000000000000000000000000000000000003",
        )
        .unwrap(),
        module: Identifier::from_str(&"sui_state_proof").unwrap(),
    };

    let mut has_next = true;
    let mut cursor = Option::None;
    while (has_next) {
        let res = client
            .event_api()
            .query_events(query.clone(), cursor, Option::None, true)
            .await
            .unwrap();

        let filtered: Option<&SuiEvent> = res
            .data
            .iter()
            .filter(|event| event.parsed_json.get("epoch").is_some())
            .filter(|event| {
                u64::from_str(event.parsed_json.get("epoch").unwrap().as_str().unwrap()).unwrap()
                    == target_epoch
            })
            .next();
        if filtered.is_some() {
            return Ok(ObjectID::from_hex_literal(
                filtered
                    .unwrap()
                    .parsed_json
                    .get("epoch_committee_id")
                    .unwrap()
                    .as_str()
                    .unwrap(),
            )
            .unwrap());
        }

        cursor = res.next_cursor;
        has_next = res.has_next_page;
    }

    return Err(anyhow::Error::msg("Epoch not found"));
}


async fn get_object_ref_by_id(config: &Config, object_id: ObjectID) -> anyhow::Result<ObjectRef> {
    let dwallet_client = SuiClientBuilder::default()
        .build(config.dwallet_full_node_url())
        .await
        .unwrap();
    let res = dwallet_client
        .read_api()
        .get_object_with_options(object_id, SuiObjectDataOptions::full_content().with_bcs())
        .await
        .unwrap();
    let object_ref = res.data.unwrap().object_ref();
    Ok(object_ref)
}


#[tokio::main]
pub async fn main() {
    // Command line arguments and config loading
    let args = Args::parse();

    let path = args
        .config
        .unwrap_or_else(|| panic!("Need a config file path"));
    let reader = fs::File::open(path.clone())
        .unwrap_or_else(|_| panic!("Unable to load config from {}", path.display()));
    let mut config: Config = serde_yaml::from_reader(reader).unwrap();

    println!("Config: {:?}", config);

    // Print config parameters
    println!(
        "Checkpoint Dir: {}",
        config.checkpoint_summary_dir.display()
    );

    let sui_client: Client = Client::new(config.sui_rest_url());
    let remote_package_store = RemotePackageStore::new(config.clone());
    let resolver = Resolver::new(remote_package_store);

    let dwallet_client = SuiClientBuilder::default()
        .build(config.dwallet_full_node_url())
        .await
        .unwrap();

    match args.command {
        Some(SCommands::Init { ckp_id }) => {
            // create a PTB with init module
            let mut ptb = ProgrammableTransactionBuilder::new();

            let mut genesis_committee: Committee;
            let mut genesis_epoch;

            if ckp_id == 0 {
                // Load the genesis committee
                let mut genesis_path = config.checkpoint_summary_dir.clone();
                genesis_path.push(&config.genesis_filename);
                genesis_committee = Genesis::load(&genesis_path).unwrap().committee().unwrap();
                genesis_committee.epoch = 1; // TOOD hack to make it work
                genesis_epoch = 0;
            } else {
                let summary = download_checkpoint_summary(&config, ckp_id).await.unwrap();
                genesis_committee = Committee::new(
                    summary.epoch() + 1,
                    summary
                        .end_of_epoch_data
                        .as_ref()
                        .unwrap()
                        .next_epoch_committee
                        .iter()
                        .cloned()
                        .collect(),
                );
                genesis_epoch = summary.epoch();
                println!("Epoch: {}", summary.epoch() + 1);
            }

            let init_committee_arg = ptb
                .pure(bcs::to_bytes(&genesis_committee).unwrap())
                .unwrap();
            let package_id_arg = ptb
                .pure(
                    bcs::to_bytes(
                        &ObjectID::from_hex_literal(&config.sui_deployed_state_proof_package)
                            .unwrap(),
                    )
                    .unwrap(),
                )
                .unwrap();

            let init_tag = StructTag {
                address: AccountAddress::from_hex_literal(&config.sui_deployed_state_proof_package)
                    .unwrap(),
                module: Identifier::new("dwallet_cap").expect("can't create identifier"),
                name: Identifier::new("DWalletNetworkInitCapRequest")
                    .expect("can't create identifier"),
                type_params: vec![],
            };
            println!("Init Tag: {}", init_tag);

            let init_type_layout = resolver
                .type_layout(TypeTag::Struct(Box::new(init_tag)))
                .await
                .unwrap();

            let init_event_type_layout_arg =
                ptb.pure(bcs::to_bytes(&init_type_layout).unwrap()).unwrap();

            let approve_tag = StructTag {
                address: AccountAddress::from_hex_literal(&config.sui_deployed_state_proof_package)
                    .unwrap(),
                module: Identifier::new("dwallet_cap").expect("can't create identifier"),
                name: Identifier::new("DWalletNetworkApproveRequest")
                    .expect("can't create identifier"),
                type_params: vec![],
            };

            let approve_type_layout = resolver
                .type_layout(TypeTag::Struct(Box::new(approve_tag)))
                .await
                .unwrap();
            let approve_event_type_layout_arg = ptb
                .pure(bcs::to_bytes(&approve_type_layout).unwrap())
                .unwrap();

            let epoch_id_committee_arg = ptb.pure(genesis_epoch).unwrap();

            let call = ProgrammableMoveCall {
                package: ObjectID::from_hex_literal(
                    "0x0000000000000000000000000000000000000000000000000000000000000003",
                )
                .unwrap(),
                module: Identifier::new("sui_state_proof").expect("can't create identifier"),
                function: Identifier::new("init_module").expect("can't create identifier"),
                type_arguments: vec![],
                arguments: vec![
                    init_committee_arg,
                    package_id_arg,
                    init_event_type_layout_arg,
                    approve_event_type_layout_arg,
                    epoch_id_committee_arg,
                ],
            };

            ptb.command(Command::MoveCall(Box::new(call)));

            let builder = ptb.finish();

            let gas_budget = 1000000000;
            let gas_price = dwallet_client
                .read_api()
                .get_reference_gas_price()
                .await
                .unwrap();

            let keystore =
                FileBasedKeystore::new(&sui_config_dir().unwrap().join(SUI_KEYSTORE_FILENAME))
                    .unwrap();

            let sender = *keystore.addresses_with_alias().first().unwrap().0;
            println!("Address: {}", sender);

            let coins = dwallet_client
                .coin_read_api()
                .get_coins(sender, None, None, None)
                .await
                .unwrap();
            let coin_gas = coins
                .data
                .into_iter()
                .max_by_key(|coin| coin.balance)
                .expect("no gas coins available");

            // create the transaction data that will be sent to the network
            let tx_data = TransactionData::new_programmable(
                sender,
                vec![coin_gas.object_ref()],
                builder,
                gas_budget,
                gas_price,
            );

            // 4) sign transaction
            let signature = keystore
                .sign_secure(&sender, &tx_data, Intent::sui_transaction())
                .unwrap();

            // 5) execute the transaction
            println!("Executing the transaction...");
            let transaction_response = dwallet_client
                .quorum_driver_api()
                .execute_transaction_block(
                    Transaction::from_data(tx_data, vec![signature]),
                    SuiTransactionBlockResponseOptions::full_content(),
                    Some(ExecuteTransactionRequestType::WaitForLocalExecution),
                )
                .await
                .unwrap();

            println!(
                "Transaction executed {}",
                transaction_response.clone().object_changes.unwrap().len()
            );

            let _ = transaction_response
                .clone()
                .object_changes
                .unwrap()
                .iter()
                .for_each(|object| println!("{}", object));

            let object_changes = transaction_response.object_changes.unwrap();
            let registry_object_change = object_changes
                .iter()
                .filter(|object| match object {
                    ObjectChange::Created {
                        sender: _,
                        owner: _,
                        object_type: object_type,
                        object_id: _,
                        version: _,
                        digest: _,
                    } => object_type.to_string().contains("Registry"),
                    _ => false,
                })
                .next()
                .unwrap();

            let committee_object_change = object_changes
                .iter()
                .filter(|object| match object {
                    ObjectChange::Created {
                        sender: _,
                        owner: _,
                        object_type: object_type,
                        object_id: _,
                        version: _,
                        digest: _,
                    } => object_type.to_string().contains("EpochCommittee"),
                    _ => false,
                })
                .next()
                .unwrap();

            let config_object_change = object_changes
                .iter()
                .filter(|object| match object {
                    ObjectChange::Created {
                        sender: _,
                        owner: _,
                        object_type: object_type,
                        object_id: _,
                        version: _,
                        digest: _,
                    } => object_type.to_string().contains("StateProofConfig"),
                    _ => false,
                })
                .next()
                .unwrap();

            let registry_object_ref = registry_object_change.object_ref();
            let committee_object_ref = committee_object_change.object_ref();
            let config_object_ref = config_object_change.object_ref();

            config.dwltn_config_object_id = config_object_ref.0.to_string();
            config.dwltn_registry_object_id = registry_object_ref.0.to_string();
        }
        Some(SCommands::Sync {}) => {
            let res = check_and_sync_checkpoints(&config)
                .await
                .context("check and sync error");

            if res.is_err() {
                println!("Error: {:?}", res);
            }
        }
        Some(SCommands::Transaction { tid }) => {
            // not implemented - use TS library to submit transaction
        }
        _ => {}
    }
    // writing config file back
    let file = fs::File::create(&path)
        .unwrap_or_else(|_| panic!("Unable to open config file for writing: {}", path.display()));
    serde_yaml::to_writer(file, &config)
        .unwrap_or_else(|_| panic!("Failed to write config to file: {}", path.display()));
}

