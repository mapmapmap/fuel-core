use crate::fuel_core_graphql_api::{
    database::arc_wrapper::ArcWrapper,
    ports::{
        DatabaseBlocks,
        DatabaseChain,
        DatabaseContracts,
        DatabaseMessageProof,
        DatabaseMessages,
        DatabaseRelayedTransactions,
        OffChainDatabase,
        OnChainDatabase,
    },
};
use fuel_core_storage::{
    iter::{
        BoxedIter,
        IntoBoxedIter,
        IterDirection,
    },
    transactional::AtomicView,
    Error as StorageError,
    Mappable,
    Result as StorageResult,
    StorageInspect,
};
use fuel_core_txpool::types::{
    ContractId,
    TxId,
};
use fuel_core_types::{
    blockchain::{
        block::CompressedBlock,
        consensus::Consensus,
        primitives::{
            BlockId,
            DaBlockHeight,
        },
    },
    entities::relayer::{
        message::{
            MerkleProof,
            Message,
        },
        transaction::RelayedTransactionStatus,
    },
    fuel_tx::{
        Address,
        AssetId,
        Bytes32,
        Salt,
        TxPointer,
        UtxoId,
    },
    fuel_types::{
        BlockHeight,
        Nonce,
    },
    services::{
        graphql_api::ContractBalance,
        txpool::TransactionStatus,
    },
};
use std::{
    borrow::Cow,
    sync::Arc,
};

mod arc_wrapper;

/// The on-chain view of the database used by the [`ReadView`] to fetch on-chain data.
pub type OnChainView = Arc<dyn OnChainDatabase>;
/// The off-chain view of the database used by the [`ReadView`] to fetch off-chain data.
pub type OffChainView = Arc<dyn OffChainDatabase>;

/// The container of the on-chain and off-chain database view provides.
/// It is used only by `ViewExtension` to create a [`ReadView`].
pub struct ReadDatabase {
    /// The on-chain database view provider.
    on_chain: Box<dyn AtomicView<View = OnChainView, Height = BlockHeight>>,
    /// The off-chain database view provider.
    off_chain: Box<dyn AtomicView<View = OffChainView, Height = BlockHeight>>,
}

impl ReadDatabase {
    /// Creates a new [`ReadDatabase`] with the given on-chain and off-chain database view providers.
    pub fn new<OnChain, OffChain>(on_chain: OnChain, off_chain: OffChain) -> Self
    where
        OnChain: AtomicView<Height = BlockHeight> + 'static,
        OffChain: AtomicView<Height = BlockHeight> + 'static,
        OnChain::View: OnChainDatabase,
        OffChain::View: OffChainDatabase,
    {
        Self {
            on_chain: Box::new(ArcWrapper::new(on_chain)),
            off_chain: Box::new(ArcWrapper::new(off_chain)),
        }
    }

    /// Creates a consistent view of the database.
    pub fn view(&self) -> ReadView {
        // TODO: Use the same height for both views to guarantee consistency.
        //  It is not possible to implement until `view_at` is implemented for the `AtomicView`.
        //  https://github.com/FuelLabs/fuel-core/issues/1582
        ReadView {
            on_chain: self.on_chain.latest_view(),
            off_chain: self.off_chain.latest_view(),
        }
    }
}

pub struct ReadView {
    on_chain: OnChainView,
    off_chain: OffChainView,
}

impl DatabaseBlocks for ReadView {
    fn blocks(
        &self,
        height: Option<BlockHeight>,
        direction: IterDirection,
    ) -> BoxedIter<'_, StorageResult<CompressedBlock>> {
        // Chain together blocks from the off-chain db and the on-chain db
        // The blocks in off-chain db, if any, are from time before regenesis

        if let Some(height) = height {
            match self.on_chain.latest_genesis_height() {
                Ok(onchain_start_height) => {
                    match (height >= onchain_start_height, direction) {
                        (true, IterDirection::Forward) => {
                            self.on_chain.blocks(Some(height), direction)
                        }
                        (true, IterDirection::Reverse) => self
                            .on_chain
                            .blocks(Some(height), direction)
                            .chain(self.off_chain.old_blocks(None, direction))
                            .into_boxed(),
                        (false, IterDirection::Forward) => self
                            .off_chain
                            .old_blocks(Some(height), direction)
                            .chain(self.on_chain.blocks(None, direction))
                            .into_boxed(),
                        (false, IterDirection::Reverse) => {
                            self.off_chain.old_blocks(Some(height), direction)
                        }
                    }
                }
                Err(err) => core::iter::once(Err(err)).into_boxed(),
            }
        } else {
            match direction {
                IterDirection::Forward => self
                    .off_chain
                    .old_blocks(None, direction)
                    .chain(self.on_chain.blocks(None, direction))
                    .into_boxed(),
                IterDirection::Reverse => self
                    .on_chain
                    .blocks(None, direction)
                    .chain(self.off_chain.old_blocks(None, direction))
                    .into_boxed(),
            }
        }
    }

    fn latest_height(&self) -> StorageResult<BlockHeight> {
        self.on_chain.latest_height()
    }

    fn latest_genesis_height(&self) -> StorageResult<BlockHeight> {
        self.on_chain.latest_genesis_height()
    }
}

impl<M> StorageInspect<M> for ReadView
where
    M: Mappable,
    dyn OnChainDatabase: StorageInspect<M, Error = StorageError>,
{
    type Error = StorageError;

    fn get(&self, key: &M::Key) -> StorageResult<Option<Cow<M::OwnedValue>>> {
        self.on_chain.get(key)
    }

    fn contains_key(&self, key: &M::Key) -> StorageResult<bool> {
        self.on_chain.contains_key(key)
    }
}

impl DatabaseMessages for ReadView {
    fn all_messages(
        &self,
        start_message_id: Option<Nonce>,
        direction: IterDirection,
    ) -> BoxedIter<'_, StorageResult<Message>> {
        self.on_chain.all_messages(start_message_id, direction)
    }

    fn message_is_spent(&self, nonce: &Nonce) -> StorageResult<bool> {
        self.on_chain.message_is_spent(nonce)
    }

    fn message_exists(&self, nonce: &Nonce) -> StorageResult<bool> {
        self.on_chain.message_exists(nonce)
    }
}

impl DatabaseRelayedTransactions for ReadView {
    fn transaction_status(
        &self,
        id: Bytes32,
    ) -> StorageResult<Option<RelayedTransactionStatus>> {
        let maybe_status = self.off_chain.relayed_tx_status(id)?;
        Ok(maybe_status)
    }
}

impl DatabaseContracts for ReadView {
    fn contract_balances(
        &self,
        contract: ContractId,
        start_asset: Option<AssetId>,
        direction: IterDirection,
    ) -> BoxedIter<StorageResult<ContractBalance>> {
        self.on_chain
            .contract_balances(contract, start_asset, direction)
    }
}

impl DatabaseChain for ReadView {
    fn da_height(&self) -> StorageResult<DaBlockHeight> {
        self.on_chain.da_height()
    }
}

impl DatabaseMessageProof for ReadView {
    fn block_history_proof(
        &self,
        message_block_height: &BlockHeight,
        commit_block_height: &BlockHeight,
    ) -> StorageResult<MerkleProof> {
        self.on_chain
            .block_history_proof(message_block_height, commit_block_height)
    }
}

impl OnChainDatabase for ReadView {}

impl OffChainDatabase for ReadView {
    fn block_height(&self, block_id: &BlockId) -> StorageResult<BlockHeight> {
        self.off_chain.block_height(block_id)
    }

    fn tx_status(&self, tx_id: &TxId) -> StorageResult<TransactionStatus> {
        self.off_chain.tx_status(tx_id)
    }

    fn owned_coins_ids(
        &self,
        owner: &Address,
        start_coin: Option<UtxoId>,
        direction: IterDirection,
    ) -> BoxedIter<'_, StorageResult<UtxoId>> {
        self.off_chain.owned_coins_ids(owner, start_coin, direction)
    }

    fn owned_message_ids(
        &self,
        owner: &Address,
        start_message_id: Option<Nonce>,
        direction: IterDirection,
    ) -> BoxedIter<'_, StorageResult<Nonce>> {
        self.off_chain
            .owned_message_ids(owner, start_message_id, direction)
    }

    fn owned_transactions_ids(
        &self,
        owner: Address,
        start: Option<TxPointer>,
        direction: IterDirection,
    ) -> BoxedIter<StorageResult<(TxPointer, TxId)>> {
        self.off_chain
            .owned_transactions_ids(owner, start, direction)
    }

    fn contract_salt(&self, contract_id: &ContractId) -> StorageResult<Salt> {
        self.off_chain.contract_salt(contract_id)
    }

    fn old_blocks(
        &self,
        height: Option<BlockHeight>,
        direction: IterDirection,
    ) -> BoxedIter<'_, StorageResult<CompressedBlock>> {
        self.off_chain.old_blocks(height, direction)
    }

    fn old_block_consensus(&self, height: BlockHeight) -> StorageResult<Consensus> {
        self.off_chain.old_block_consensus(height)
    }

    fn old_transaction(
        &self,
        id: &TxId,
    ) -> StorageResult<Option<fuel_core_types::fuel_tx::Transaction>> {
        self.off_chain.old_transaction(id)
    }

    fn relayed_tx_status(
        &self,
        id: Bytes32,
    ) -> StorageResult<Option<RelayedTransactionStatus>> {
        self.off_chain.relayed_tx_status(id)
    }
}
