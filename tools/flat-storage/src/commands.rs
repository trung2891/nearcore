/// Tools for modifying flat storage - should be used only for experimentation & debugging.
use borsh::BorshDeserialize;
use clap::Parser;
use near_chain::flat_storage_creator::FlatStorageShardCreator;
use near_chain::types::RuntimeAdapter;
use near_chain::{ChainStore, ChainStoreAccess};
use near_chain_configs::GenesisValidationMode;
use near_epoch_manager::{EpochManager, EpochManagerAdapter, EpochManagerHandle};
use near_primitives::shard_layout::ShardVersion;
use near_primitives::types::{BlockHeight, ShardId};
use near_store::flat::{
    inline_flat_state_values, store_helper, FlatStateDelta, FlatStateDeltaMetadata,
    FlatStorageManager, FlatStorageStatus,
};
use near_store::{DBCol, Mode, NodeStorage, ShardUId, Store, StoreOpener};
use nearcore::{load_config, NearConfig, NightshadeRuntime};
use std::sync::atomic::AtomicBool;
use std::{path::PathBuf, sync::Arc, time::Duration};
use tqdm::tqdm;

#[derive(Parser)]
pub struct FlatStorageCommand {
    #[clap(subcommand)]
    subcmd: SubCommand,
}

#[derive(Parser)]
#[clap(subcommand_required = true, arg_required_else_help = true)]
enum SubCommand {
    /// View the current state of flat storage
    View(ViewCmd),

    /// Reset the flat storage state (remove all the contents)
    Reset(ResetCmd),

    /// Init the flat storage state, by copying from trie
    Init(InitCmd),

    /// Verify flat storage state (it can take up to couple hours if flat storage is very large)
    Verify(VerifyCmd),

    /// Temporary command to set the store version (useful as long flat
    /// storage is enabled only during nightly with separate DB version).
    SetStoreVersion(SetStoreVersionCmd),

    /// Run FlatState value inininig migration
    MigrateValueInlining(MigrateValueInliningCmd),

    /// Construct and store trie in a separate directory from flat storage state for a given shard.
    /// The trie is constructed for the block height equal to flat_head
    ConstructTrieFromFlat(ConstructTriedFromFlatCmd),

    /// Move flat head forward.
    MoveFlatHead(MoveFlatHeadCmd),
}

#[derive(Parser)]
pub struct ViewCmd {
    #[clap(long)]
    shard_id: Option<ShardId>,
}

#[derive(Parser)]
pub struct ConstructTriedFromFlatCmd {
    #[clap(long)]
    shard_id: ShardId,
    /// Path to directory where the constructed trie would be stored. Note that there shouldn't be an
    /// existing DB in the path provided.
    #[clap(long)]
    write_store_path: PathBuf,
}

#[derive(Parser)]
pub struct SetStoreVersionCmd {
    version: u32,
}

#[derive(Parser)]
pub struct ResetCmd {
    shard_id: ShardId,
}

#[derive(Parser)]
pub struct InitCmd {
    shard_id: ShardId,

    #[clap(default_value = "3")]
    num_threads: usize,
}

#[derive(Parser)]
pub struct VerifyCmd {
    shard_id: ShardId,
}

#[derive(Parser)]
pub struct MigrateValueInliningCmd {
    #[clap(default_value = "16")]
    num_threads: usize,

    #[clap(default_value = "50000")]
    batch_size: usize,
}

#[derive(Parser)]
pub struct MoveFlatHeadCmd {
    #[clap(long)]
    shard_id: ShardId,
    #[clap(long)]
    version: ShardVersion,
    #[clap(long)]
    new_flat_head_height: BlockHeight,
}

fn print_delta(store: &Store, shard_uid: ShardUId, metadata: FlatStateDeltaMetadata) {
    let changes =
        store_helper::get_delta_changes(store, shard_uid, metadata.block.hash).unwrap().unwrap();
    println!("{:?}", FlatStateDelta { metadata, changes });
}

fn print_deltas(store: &Store, shard_uid: ShardUId) {
    let deltas_metadata = store_helper::get_all_deltas_metadata(store, shard_uid).unwrap();
    let num_deltas = deltas_metadata.len();
    println!("Deltas: {}", num_deltas);

    if num_deltas <= 10 {
        for delta_metadata in deltas_metadata {
            print_delta(store, shard_uid, delta_metadata);
        }
    } else {
        let (first_deltas, last_deltas) = deltas_metadata.split_at(5);

        for delta_metadata in first_deltas {
            print_delta(store, shard_uid, *delta_metadata);
        }
        println!("... skipped {} deltas ...", num_deltas - 10);
        let (_, last_deltas) = last_deltas.split_at(last_deltas.len() - 5);
        for delta_metadata in last_deltas {
            print_delta(store, shard_uid, *delta_metadata);
        }
    }
}

impl FlatStorageCommand {
    fn get_db(
        opener: &StoreOpener,
        home_dir: &PathBuf,
        near_config: &NearConfig,
        mode: Mode,
    ) -> (NodeStorage, Arc<EpochManagerHandle>, Arc<NightshadeRuntime>, ChainStore, Store) {
        let node_storage = opener.open_in_mode(mode).unwrap();
        let epoch_manager =
            EpochManager::new_arc_handle(node_storage.get_hot_store(), &near_config.genesis.config);
        let hot_runtime = NightshadeRuntime::from_config(
            home_dir,
            node_storage.get_hot_store(),
            &near_config,
            epoch_manager.clone(),
        );
        let chain_store = ChainStore::new(node_storage.get_hot_store(), 0, false);
        let hot_store = node_storage.get_hot_store();
        (node_storage, epoch_manager, hot_runtime, chain_store, hot_store)
    }

    fn view(
        &self,
        cmd: &ViewCmd,
        home_dir: &PathBuf,
        near_config: &NearConfig,
        opener: StoreOpener,
    ) -> anyhow::Result<()> {
        let (.., hot_store) =
            Self::get_db(&opener, home_dir, &near_config, near_store::Mode::ReadOnly);
        println!("DB version: {:?}", hot_store.get_db_version()?);
        for item in hot_store.iter(DBCol::FlatStorageStatus) {
            let (bytes_shard_uid, status) = item?;
            let shard_uid = ShardUId::try_from(bytes_shard_uid.as_ref()).unwrap();
            let status = FlatStorageStatus::try_from_slice(&status)?;
            if let Some(shard_id) = cmd.shard_id {
                if shard_id != shard_uid.shard_id as ShardId {
                    continue;
                }
            }

            match status {
                FlatStorageStatus::Ready(ready_status) => {
                    println!(
                        "Shard: {shard_uid:?} - flat storage @{:?} ({})",
                        ready_status.flat_head.height, ready_status.flat_head.hash,
                    );
                    print_deltas(&hot_store, shard_uid);
                }
                status => {
                    println!("Shard: {shard_uid:?} - no flat storage: {status:?}");
                }
            }
        }
        Ok(())
    }

    fn set_store_version(
        &self,
        cmd: &SetStoreVersionCmd,
        opener: StoreOpener,
    ) -> anyhow::Result<()> {
        let rw_storage = opener.open_in_mode(near_store::Mode::ReadWriteExisting)?;
        let rw_store = rw_storage.get_hot_store();
        println!("Setting storage DB version to: {:?}", cmd.version);
        rw_store.set_db_version(cmd.version)?;
        Ok(())
    }

    fn reset(
        &self,
        cmd: &ResetCmd,
        home_dir: &PathBuf,
        near_config: &NearConfig,
        opener: StoreOpener,
    ) -> anyhow::Result<()> {
        let (_, epoch_manager, rw_hot_runtime, rw_chain_store, store) =
            Self::get_db(&opener, home_dir, &near_config, near_store::Mode::ReadWriteExisting);
        let tip = rw_chain_store.final_head()?;

        // TODO: there should be a method that 'loads' the current flat storage state based on Storage.
        let shard_uid = epoch_manager.shard_id_to_uid(cmd.shard_id, &tip.epoch_id)?;
        let flat_storage_manager = rw_hot_runtime.get_flat_storage_manager();
        flat_storage_manager.create_flat_storage_for_shard(shard_uid)?;
        let mut store_update = store.store_update();
        flat_storage_manager.remove_flat_storage_for_shard(shard_uid, &mut store_update)?;
        store_update.commit()?;
        Ok(())
    }

    fn init(
        &self,
        cmd: &InitCmd,
        home_dir: &PathBuf,
        near_config: &NearConfig,
        opener: StoreOpener,
    ) -> anyhow::Result<()> {
        let (_, epoch_manager, rw_hot_runtime, rw_chain_store, rw_hot_store) =
            Self::get_db(&opener, home_dir, &near_config, near_store::Mode::ReadWriteExisting);

        let tip = rw_chain_store.final_head()?;
        let shard_uid = epoch_manager.shard_id_to_uid(cmd.shard_id, &tip.epoch_id)?;
        let mut creator =
            FlatStorageShardCreator::new(shard_uid, tip.height - 1, epoch_manager, rw_hot_runtime);
        let pool = rayon::ThreadPoolBuilder::new().num_threads(cmd.num_threads).build()?;

        loop {
            let status = creator.update_status(&rw_chain_store, &pool)?;
            if status {
                break;
            }
            let current_status = store_helper::get_flat_storage_status(&rw_hot_store, shard_uid);
            println!("Status: {:?}", current_status);

            std::thread::sleep(Duration::from_secs(1));
        }

        println!("Flat storage initialization finished.");
        Ok(())
    }

    fn verify(
        &self,
        cmd: &VerifyCmd,
        home_dir: &PathBuf,
        near_config: &NearConfig,
        opener: StoreOpener,
    ) -> anyhow::Result<()> {
        let (_, epoch_manager, hot_runtime, chain_store, hot_store) =
            Self::get_db(&opener, home_dir, &near_config, near_store::Mode::ReadOnly);
        let tip = chain_store.final_head()?;
        let shard_uid = epoch_manager.shard_id_to_uid(cmd.shard_id, &tip.epoch_id)?;

        let head_hash = match store_helper::get_flat_storage_status(&hot_store, shard_uid)
            .expect("falied to read flat storage status")
        {
            FlatStorageStatus::Ready(ready_status) => ready_status.flat_head.hash,
            status => {
                panic!("Flat storage is not ready for shard {:?}: {status:?}", cmd.shard_id);
            }
        };
        let block_header = chain_store.get_block_header(&head_hash)?;
        let shard_layout = epoch_manager.get_shard_layout(block_header.epoch_id())?;

        println!(
            "Verifying flat storage for shard {:?} - flat head @{:?} ({:?})",
            cmd.shard_id,
            block_header.height(),
            block_header.hash()
        );
        let chunk_extra = chain_store.get_chunk_extra(
            &head_hash,
            &ShardUId::from_shard_id_and_layout(cmd.shard_id, &shard_layout),
        )?;

        // The state root must be from AFTER applying the final block (that's why we're taking it from the chunk extra).
        let state_root = chunk_extra.state_root();

        println!("Verifying using the {:?} as state_root", state_root);
        let tip = chain_store.final_head()?;

        let shard_uid = epoch_manager.shard_id_to_uid(cmd.shard_id, &tip.epoch_id)?;
        hot_runtime.get_flat_storage_manager().create_flat_storage_for_shard(shard_uid)?;

        let trie = hot_runtime.get_view_trie_for_shard(cmd.shard_id, &head_hash, *state_root)?;

        let flat_state_entries_iter =
            store_helper::iter_flat_state_entries(shard_uid, &hot_store, None, None);

        let trie_iter = trie.iter()?;
        let mut verified = 0;
        let mut success = true;
        for (item_trie, item_flat) in tqdm(std::iter::zip(trie_iter, flat_state_entries_iter)) {
            let item_flat = item_flat?;
            let value_ref = item_flat.1.to_value_ref();
            verified += 1;

            let item_trie = item_trie?;
            if item_trie.0 != *item_flat.0 {
                println!(
                    "Different keys {:?} in trie, {:?} in flat storage. ",
                    item_trie.0, item_flat.0
                );
                success = false;
                break;
            }
            if item_trie.1.len() != value_ref.length as usize {
                println!(
                    "Different ValueRef::length for key: {:?}  in trie: {:?} vs flat storage: {:?}",
                    item_trie.0,
                    item_trie.1.len(),
                    value_ref.length
                );
                success = false;
                break;
            }

            if near_primitives::hash::hash(&item_trie.1) != value_ref.hash {
                println!(
                    "Different ValueRef::hash for key: {:?} in trie: {:?} vs flat storage: {:?}",
                    item_trie.0,
                    near_primitives::hash::hash(&item_trie.1),
                    value_ref.hash
                );
                success = false;
                break;
            }
        }
        if success {
            println!("Success - verified {:?} nodes", verified);
        } else {
            println!("FAILED - on node {:?}", verified);
        }
        Ok(())
    }

    fn migrate_value_inlining(
        &self,
        cmd: &MigrateValueInliningCmd,
        home_dir: &PathBuf,
        near_config: &NearConfig,
        opener: StoreOpener,
    ) -> anyhow::Result<()> {
        let store =
            Self::get_db(&opener, home_dir, &near_config, near_store::Mode::ReadWriteExisting).4;
        let flat_storage_manager = FlatStorageManager::new(store.clone());
        inline_flat_state_values(
            store,
            &flat_storage_manager,
            &AtomicBool::new(true),
            cmd.num_threads,
            cmd.batch_size,
        );
        Ok(())
    }

    fn construct_trie_from_flat(
        &self,
        cmd: &ConstructTriedFromFlatCmd,
        home_dir: &PathBuf,
        near_config: &NearConfig,
        opener: StoreOpener,
    ) -> anyhow::Result<()> {
        let (_, epoch_manager, _, chain_store, store) =
            Self::get_db(&opener, home_dir, &near_config, near_store::Mode::ReadWriteExisting);

        let write_opener =
            NodeStorage::opener(&cmd.write_store_path, false, &near_config.config.store, None);
        let write_node_storage = write_opener.open_in_mode(Mode::Create)?;
        let write_store = write_node_storage.get_hot_store();

        let tip = chain_store.final_head()?;
        let shard_uid = epoch_manager.shard_id_to_uid(cmd.shard_id, &tip.epoch_id)?;

        near_store::trie::construct_trie_from_flat(store, write_store, shard_uid);
        Ok(())
    }

    fn move_flat_head(
        &self,
        cmd: &MoveFlatHeadCmd,
        home_dir: &PathBuf,
        near_config: &NearConfig,
        opener: StoreOpener,
    ) -> anyhow::Result<()> {
        let (_, _, runtime, chain_store, _) =
            Self::get_db(&opener, home_dir, &near_config, near_store::Mode::ReadWriteExisting);

        let shard_uid = ShardUId { version: cmd.version, shard_id: cmd.shard_id as u32 };
        let flat_storage_manager = runtime.get_flat_storage_manager();
        flat_storage_manager.create_flat_storage_for_shard(shard_uid)?;
        let flat_storage = flat_storage_manager.get_flat_storage_for_shard(shard_uid).unwrap();
        let header = chain_store.get_block_header_by_height(cmd.new_flat_head_height)?;
        println!("Header: {header:?}");
        flat_storage.update_flat_head(header.hash(), true)?;
        Ok(())
    }

    pub fn run(
        &self,
        home_dir: &PathBuf,
        genesis_validation: GenesisValidationMode,
    ) -> anyhow::Result<()> {
        let near_config = load_config(home_dir, genesis_validation)?;
        let opener = NodeStorage::opener(
            home_dir,
            near_config.config.archive,
            &near_config.config.store,
            None,
        );

        match &self.subcmd {
            SubCommand::View(cmd) => self.view(cmd, home_dir, &near_config, opener),
            SubCommand::SetStoreVersion(cmd) => self.set_store_version(cmd, opener),
            SubCommand::Reset(cmd) => self.reset(cmd, home_dir, &near_config, opener),
            SubCommand::Init(cmd) => self.init(cmd, home_dir, &near_config, opener),
            SubCommand::Verify(cmd) => self.verify(cmd, home_dir, &near_config, opener),
            SubCommand::MigrateValueInlining(cmd) => {
                self.migrate_value_inlining(cmd, home_dir, &near_config, opener)
            }
            SubCommand::ConstructTrieFromFlat(cmd) => {
                self.construct_trie_from_flat(cmd, home_dir, &near_config, opener)
            }
            SubCommand::MoveFlatHead(cmd) => {
                self.move_flat_head(cmd, home_dir, &near_config, opener)
            }
        }
    }
}
