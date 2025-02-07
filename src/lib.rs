use bdk::bitcoin::{Address, BlockHeader, Script, Transaction, Txid};
use bdk::blockchain::{noop_progress, Blockchain, IndexedChain, TxStatus};
use bdk::database::BatchDatabase;
use bdk::wallet::{AddressIndex, Wallet};
use bdk::SignOptions;

use lightning::chain::chaininterface::BroadcasterInterface;
use lightning::chain::chaininterface::{ConfirmationTarget, FeeEstimator};
use lightning::chain::WatchedOutput;
use lightning::chain::{Confirm, Filter};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;

pub type TransactionWithHeight = (u32, Transaction);
pub type TransactionWithPosition = (usize, Transaction);
pub type TransactionWithHeightAndPosition = (u32, Transaction, usize);

#[derive(Debug)]
pub enum Error {
    Bdk(bdk::Error),
}

impl From<bdk::Error> for Error {
    fn from(e: bdk::Error) -> Self {
        Self::Bdk(e)
    }
}

struct TxFilter {
    watched_transactions: Vec<(Txid, Script)>,
    watched_outputs: Vec<WatchedOutput>,
}

impl TxFilter {
    fn new() -> Self {
        Self {
            watched_transactions: vec![],
            watched_outputs: vec![],
        }
    }

    fn register_tx(&mut self, txid: Txid, script: Script) {
        self.watched_transactions.push((txid, script));
    }

    fn register_output(&mut self, output: WatchedOutput) {
        self.watched_outputs.push(output);
    }
}

impl Default for TxFilter {
    fn default() -> Self {
        Self::new()
    }
}

/// Lightning Wallet
///
/// A wrapper around a bdk::Wallet to fulfill many of the requirements
/// needed to use lightning with LDK.  Note: The bdk::Blockchain you use
/// must implement the IndexedChain trait.
pub struct LightningWallet<B, D> {
    inner: Mutex<Wallet<B, D>>,
    filter: Mutex<TxFilter>,
}

impl<B, D> LightningWallet<B, D>
where
    B: Blockchain + IndexedChain,
    D: BatchDatabase,
{
    /// create a new lightning wallet from your bdk wallet
    pub fn new(wallet: Wallet<B, D>) -> Self {
        LightningWallet {
            inner: Mutex::new(wallet),
            filter: Mutex::new(TxFilter::new()),
        }
    }

    /// syncs both your onchain and lightning wallet to current tip
    /// utilizes ldk's Confirm trait to provide chain data
    pub fn sync(
        &self,
        channel_manager: Arc<dyn Confirm>,
        chain_monitor: Arc<dyn Confirm>,
    ) -> Result<(), Error> {
        self.sync_onchain_wallet()?;

        let mut relevant_txids = channel_manager.get_relevant_txids();
        relevant_txids.append(&mut chain_monitor.get_relevant_txids());
        relevant_txids.sort_unstable();
        relevant_txids.dedup();

        let unconfirmed_txids = self.get_unconfirmed(relevant_txids)?;
        for unconfirmed_txid in unconfirmed_txids {
            channel_manager.transaction_unconfirmed(&unconfirmed_txid);
            chain_monitor.transaction_unconfirmed(&unconfirmed_txid);
        }

        let confirmed_txs = self.get_confirmed_txs_by_block()?;
        for (height, header, tx_list) in confirmed_txs {
            let tx_list_ref = tx_list
                .iter()
                .map(|(height, tx)| (height.to_owned(), tx))
                .collect::<Vec<(usize, &Transaction)>>();

            channel_manager.transactions_confirmed(&header, tx_list_ref.as_slice(), height);
            chain_monitor.transactions_confirmed(&header, tx_list_ref.as_slice(), height);
        }

        let (tip_height, tip_header) = self.get_tip()?;

        channel_manager.best_block_updated(&tip_header, tip_height);
        chain_monitor.best_block_updated(&tip_header, tip_height);
        Ok(())
    }

    /// returns the AddressIndex::LastUnused address for your wallet
    /// this is useful when you need to sweep funds from a channel
    /// back into your onchain wallet.
    pub fn get_unused_address(&self) -> Result<Address, Error> {
        let wallet = self.inner.lock().unwrap();
        let address_info = wallet.get_address(AddressIndex::LastUnused)?;
        Ok(address_info.address)
    }

    /// when opening a channel you can use this to fund the channel
    /// with the utxos in your bdk wallet
    pub fn construct_funding_transaction(
        &self,
        output_script: &Script,
        value: u64,
        target_blocks: usize,
    ) -> Result<Transaction, Error> {
        let wallet = self.inner.lock().unwrap();

        let mut tx_builder = wallet.build_tx();
        let fee_rate = wallet.client().estimate_fee(target_blocks)?;

        tx_builder
            .add_recipient(output_script.clone(), value)
            .fee_rate(fee_rate)
            .do_not_spend_change()
            .enable_rbf();

        let (mut psbt, _tx_details) = tx_builder.finish()?;

        let _finalized = wallet.sign(&mut psbt, SignOptions::default())?;

        Ok(psbt.extract_tx())
    }

    fn sync_onchain_wallet(&self) -> Result<(), Error> {
        let wallet = self.inner.lock().unwrap();
        wallet.sync(noop_progress(), None)?;
        Ok(())
    }

    fn get_unconfirmed(&self, txids: Vec<Txid>) -> Result<Vec<Txid>, Error> {
        Ok(txids
            .into_iter()
            .map(|txid| self.augment_txid_with_confirmation_status(txid))
            .collect::<Result<Vec<(Txid, bool)>, Error>>()?
            .into_iter()
            .filter(|(_txid, confirmed)| !confirmed)
            .map(|(txid, _)| txid)
            .collect())
    }

    fn get_confirmed_txs_by_block(
        &self,
    ) -> Result<Vec<(u32, BlockHeader, Vec<TransactionWithPosition>)>, Error> {
        let mut txs_by_block: HashMap<u32, Vec<TransactionWithPosition>> = HashMap::new();

        let filter = self.filter.lock().unwrap();

        let mut confirmed_txs = filter
            .watched_transactions
            .iter()
            .map(|(txid, script)| self.get_confirmed_tx(txid, script))
            .collect::<Result<Vec<Option<TransactionWithHeight>>, Error>>()?
            .into_iter()
            .flatten()
            .collect::<Vec<TransactionWithHeight>>();

        let mut confirmed_spent = filter
            .watched_outputs
            .iter()
            .map(|output| self.get_confirmed_txs(output))
            .collect::<Result<Vec<Vec<TransactionWithHeight>>, Error>>()?
            .into_iter()
            .flatten()
            .collect::<Vec<TransactionWithHeight>>();

        confirmed_txs.append(&mut confirmed_spent);

        let confirmed_txs_with_position = confirmed_txs
            .into_iter()
            .map(|(height, tx)| self.augment_with_position(height, tx))
            .collect::<Result<Vec<Option<TransactionWithHeightAndPosition>>, Error>>()?
            .into_iter()
            .flatten()
            .collect::<Vec<TransactionWithHeightAndPosition>>();

        for (height, tx, pos) in confirmed_txs_with_position {
            txs_by_block.entry(height).or_default().push((pos, tx))
        }

        txs_by_block
            .into_iter()
            .map(|(height, tx_list)| self.augment_with_header(height, tx_list))
            .collect()
    }

    fn get_tip(&self) -> Result<(u32, BlockHeader), Error> {
        let wallet = self.inner.lock().unwrap();
        let tip_height = wallet.client().get_height()?;
        let tip_header = wallet.client().get_header(tip_height)?;
        Ok((tip_height, tip_header))
    }

    fn augment_txid_with_confirmation_status(&self, txid: Txid) -> Result<(Txid, bool), Error> {
        let wallet = self.inner.lock().unwrap();
        wallet
            .client()
            .get_tx_status(&txid)
            .map(|status| match status {
                Some(status) => (txid, status.confirmed),
                None => (txid, false),
            })
            .map_err(Error::Bdk)
    }

    fn get_confirmed_tx(
        &self,
        txid: &Txid,
        script: &Script,
    ) -> Result<Option<TransactionWithHeight>, Error> {
        let wallet = self.inner.lock().unwrap();
        wallet
            .client()
            .get_script_tx_history(script)
            .map(|history| {
                history
                    .into_iter()
                    .find(|(status, tx)| status.confirmed && tx.txid().eq(txid))
                    .map(|(status, tx)| (status.block_height.unwrap(), tx))
            })
            .map_err(Error::Bdk)
    }

    fn get_confirmed_txs_from_script_history(
        &self,
        history: Vec<(TxStatus, Transaction)>,
    ) -> Vec<TransactionWithHeight> {
        history
            .into_iter()
            .filter(|(status, _tx)| status.confirmed)
            .map(|(status, tx)| (status.block_height.unwrap(), tx))
            .collect::<Vec<TransactionWithHeight>>()
    }

    fn get_confirmed_txs(
        &self,
        output: &WatchedOutput,
    ) -> Result<Vec<TransactionWithHeight>, Error> {
        let wallet = self.inner.lock().unwrap();

        wallet
            .client()
            .get_script_tx_history(&output.script_pubkey)
            .map(|history| self.get_confirmed_txs_from_script_history(history))
            .map_err(Error::Bdk)
    }

    fn augment_with_position(
        &self,
        height: u32,
        tx: Transaction,
    ) -> Result<Option<TransactionWithHeightAndPosition>, Error> {
        let wallet = self.inner.lock().unwrap();

        wallet
            .client()
            .get_position_in_block(&tx.txid(), height as usize)
            .map(|position| position.map(|pos| (height, tx, pos)))
            .map_err(Error::Bdk)
    }

    fn augment_with_header(
        &self,
        height: u32,
        tx_list: Vec<TransactionWithPosition>,
    ) -> Result<(u32, BlockHeader, Vec<TransactionWithPosition>), Error> {
        let wallet = self.inner.lock().unwrap();
        wallet
            .client()
            .get_header(height)
            .map(|header| (height, header, tx_list))
            .map_err(Error::Bdk)
    }
}

impl<B, D> From<Wallet<B, D>> for LightningWallet<B, D>
where
    B: Blockchain + IndexedChain,
    D: BatchDatabase,
{
    fn from(wallet: Wallet<B, D>) -> Self {
        Self::new(wallet)
    }
}

impl<B, D> FeeEstimator for LightningWallet<B, D>
where
    B: Blockchain + IndexedChain,
    D: BatchDatabase,
{
    fn get_est_sat_per_1000_weight(&self, confirmation_target: ConfirmationTarget) -> u32 {
        let wallet = self.inner.lock().unwrap();

        let target_blocks = match confirmation_target {
            ConfirmationTarget::Background => 6,
            ConfirmationTarget::Normal => 3,
            ConfirmationTarget::HighPriority => 1,
        };

        let estimate = wallet
            .client()
            .estimate_fee(target_blocks)
            .unwrap_or_default();
        let sats_per_vbyte = estimate.as_sat_vb() as u32;
        sats_per_vbyte * 250
    }
}

impl<B, D> BroadcasterInterface for LightningWallet<B, D>
where
    B: Blockchain + IndexedChain,
    D: BatchDatabase,
{
    fn broadcast_transaction(&self, tx: &Transaction) {
        let wallet = self.inner.lock().unwrap();
        let _result = wallet.client().broadcast(tx);
    }
}

impl<B, D> Filter for LightningWallet<B, D>
where
    B: Blockchain + IndexedChain,
    D: BatchDatabase,
{
    fn register_tx(&self, txid: &Txid, script_pubkey: &Script) {
        let mut filter = self.filter.lock().unwrap();
        filter.register_tx(*txid, script_pubkey.clone());
    }

    fn register_output(&self, output: WatchedOutput) -> Option<TransactionWithPosition> {
        let mut filter = self.filter.lock().unwrap();
        filter.register_output(output);
        // TODO: do we need to check for tx here or wait for next sync?
        None
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn it_works() {
        let result = 2 + 2;
        assert_eq!(result, 4);
    }
}
