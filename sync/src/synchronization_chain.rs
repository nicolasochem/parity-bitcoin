use std::fmt;
use std::sync::Arc;
use std::collections::VecDeque;
use parking_lot::RwLock;
use chain::{Block, BlockHeader};
use db;
use best_headers_chain::{BestHeadersChain, Information as BestHeadersInformation};
use primitives::hash::H256;
use hash_queue::{HashQueueChain, HashPosition};
use miner::MemoryPool;

/// Thread-safe reference to `Chain`
pub type ChainRef = Arc<RwLock<Chain>>;

/// Index of 'verifying' queue
const VERIFYING_QUEUE: usize = 0;
/// Index of 'requested' queue
const REQUESTED_QUEUE: usize = 1;
/// Index of 'scheduled' queue
const SCHEDULED_QUEUE: usize = 2;
/// Number of hash queues
const NUMBER_OF_QUEUES: usize = 3;

/// Block synchronization state
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum BlockState {
	/// Block is unknown
	Unknown,
	/// Scheduled for requesting
	Scheduled,
	/// Requested from peers
	Requested,
	/// Currently verifying
	Verifying,
	/// In storage
	Stored,
}

/// Synchronization chain information
pub struct Information {
	/// Number of blocks hashes currently scheduled for requesting
	pub scheduled: u32,
	/// Number of blocks hashes currently requested from peers
	pub requested: u32,
	/// Number of blocks currently verifying
	pub verifying: u32,
	/// Number of blocks in the storage
	pub stored: u32,
	/// Information on headers chain
	pub headers: BestHeadersInformation,
}

/// Result of intersecting chain && inventory
#[derive(Debug, PartialEq)]
pub enum HeadersIntersection {
	/// 3.2: No intersection with in-memory queue && no intersection with db
	NoKnownBlocks(usize),
	/// 2.3: Inventory has no new blocks && some of blocks in inventory are in in-memory queue
	InMemoryNoNewBlocks,
	/// 2.4.2: Inventory has new blocks && these blocks are right after chain' best block
	InMemoryMainNewBlocks(usize),
	/// 2.4.3: Inventory has new blocks && these blocks are forked from our chain' best block
	InMemoryForkNewBlocks(usize),
	/// 3.3: No intersection with in-memory queue && has intersection with db && all blocks are already stored in db
	DbAllBlocksKnown,
	/// 3.4: No intersection with in-memory queue && has intersection with db && some blocks are not yet stored in db
	DbForkNewBlocks(usize),
}

/// Blockchain from synchroniation point of view, consisting of:
/// 1) all blocks from the `storage` [oldest blocks]
/// 2) all blocks currently verifying by `verification_queue`
/// 3) all blocks currently requested from peers
/// 4) all blocks currently scheduled for requesting [newest blocks]
pub struct Chain {
	/// Genesis block hash (stored for optimizations)
	genesis_block_hash: H256,
	/// Best storage block (stored for optimizations)
	best_storage_block: db::BestBlock,
	/// Local blocks storage
	storage: Arc<db::Store>,
	/// In-memory queue of blocks hashes
	hash_chain: HashQueueChain,
	/// In-memory queue of blocks headers
	headers_chain: BestHeadersChain,
	/// Transactions memory pool
	memory_pool: MemoryPool,
}

impl BlockState {
	pub fn from_queue_index(queue_index: usize) -> BlockState {
		match queue_index {
			SCHEDULED_QUEUE => BlockState::Scheduled,
			REQUESTED_QUEUE => BlockState::Requested,
			VERIFYING_QUEUE => BlockState::Verifying,
			_ => panic!("Unsupported queue_index: {}", queue_index),
		}
	}

	pub fn to_queue_index(&self) -> usize {
		match *self {
			BlockState::Scheduled => SCHEDULED_QUEUE,
			BlockState::Requested => REQUESTED_QUEUE,
			BlockState::Verifying => VERIFYING_QUEUE,
			_ => panic!("Unsupported queue: {:?}", self),
		}
	}
}

impl Chain {
	/// Create new `Chain` with given storage
	pub fn new(storage: Arc<db::Store>) -> Self {
		// we only work with storages with genesis block
		let genesis_block_hash = storage.block_hash(0)
			.expect("storage with genesis block is required");
		let best_storage_block = storage.best_block()
			.expect("non-empty storage is required");

		Chain {
			genesis_block_hash: genesis_block_hash.clone(),
			best_storage_block: best_storage_block,
			storage: storage,
			hash_chain: HashQueueChain::with_number_of_queues(NUMBER_OF_QUEUES),
			headers_chain: BestHeadersChain::new(genesis_block_hash),
			memory_pool: MemoryPool::new(),
		}
	}

	/// Get information on current blockchain state
	pub fn information(&self) -> Information {
		Information {
			scheduled: self.hash_chain.len_of(SCHEDULED_QUEUE),
			requested: self.hash_chain.len_of(REQUESTED_QUEUE),
			verifying: self.hash_chain.len_of(VERIFYING_QUEUE),
			stored: self.best_storage_block.number + 1,
			headers: self.headers_chain.information(),
		}
	}

	/// Get storage
	pub fn storage(&self) -> Arc<db::Store> {
		self.storage.clone()
	}

	/// Get memory pool reference
	pub fn memory_pool(&self) -> &MemoryPool {
		&self.memory_pool
	}

	/// Get mutable memory pool reference
	#[cfg(test)]
	pub fn memory_pool_mut<'a>(&'a mut self) -> &'a mut MemoryPool {
		&mut self.memory_pool
	}

	/// Get number of blocks in given state
	pub fn length_of_state(&self, state: BlockState) -> u32 {
		match state {
			BlockState::Stored => self.best_storage_block.number + 1,
			_ => self.hash_chain.len_of(state.to_queue_index()),
		}
	}

	/// Get best block
	pub fn best_block(&self) -> db::BestBlock {
		match self.hash_chain.back() {
			Some(hash) => db::BestBlock {
				number: self.best_storage_block.number + self.hash_chain.len(),
				hash: hash.clone(),
			},
			None => self.best_storage_block.clone(),
		}
	}

	/// Get best storage block
	pub fn best_storage_block(&self) -> db::BestBlock {
		self.best_storage_block.clone()
	}

	/// Get block header by hash
	pub fn block_hash(&self, number: u32) -> Option<H256> {
		if number <= self.best_storage_block.number {
			self.storage.block_hash(number)
		} else {
			// we try to keep these in order, but they are probably not
			self.hash_chain.at(number - self.best_storage_block.number)
		}
	}

	/// Get block number by hash
	pub fn block_number(&self, hash: &H256) -> Option<u32> {
		if let Some(number) = self.storage.block_number(hash) {
			return Some(number);
		}
		self.headers_chain.height(hash).map(|p| self.best_storage_block.number + p + 1)
	}

	/// Get block header by number
	pub fn block_header_by_number(&self, number: u32) -> Option<BlockHeader> {
		if number <= self.best_storage_block.number {
			// TODO: read block header only
			self.storage.block(db::BlockRef::Number(number)).map(|b| b.block_header)
		} else {
			self.headers_chain.at(number - self.best_storage_block.number)
		}
	}

	/// Get block header by hash
	pub fn block_header_by_hash(&self, hash: &H256) -> Option<BlockHeader> {
		if let Some(block) = self.storage.block(db::BlockRef::Hash(hash.clone())) {
			return Some(block.block_header);
		}
		self.headers_chain.by_hash(hash)
	}

	/// Get block state
	pub fn block_state(&self, hash: &H256) -> BlockState {
		match self.hash_chain.contains_in(hash) {
			Some(queue_index) => BlockState::from_queue_index(queue_index),
			None => if self.storage.contains_block(db::BlockRef::Hash(hash.clone())) {
				BlockState::Stored
			} else {
				BlockState::Unknown
			},
		}
	}

	/// Prepare block locator hashes, as described in protocol documentation:
	/// https://en.bitcoin.it/wiki/Protocol_documentation#getblocks
	/// When there are forked blocks in the queue, this method can result in
	/// mixed block locator hashes ([0 - from fork1, 1 - from fork2, 2 - from fork1]).
	/// Peer will respond with blocks of fork1 || fork2 => we could end up in some side fork
	/// To resolve this, after switching to saturated state, we will also ask all peers for inventory.
	pub fn block_locator_hashes(&self) -> Vec<H256> {
		let mut block_locator_hashes: Vec<H256> = Vec::new();

		// calculate for hash_queue
		let (local_index, step) = self.block_locator_hashes_for_queue(&mut block_locator_hashes);

		// calculate for storage
		let storage_index = if self.best_storage_block.number < local_index { 0 } else { self.best_storage_block.number - local_index };
		self.block_locator_hashes_for_storage(storage_index, step, &mut block_locator_hashes);
		block_locator_hashes
	}

	/// Schedule blocks hashes for requesting
	pub fn schedule_blocks_headers(&mut self, hashes: Vec<H256>, headers: Vec<BlockHeader>) {
		self.hash_chain.push_back_n_at(SCHEDULED_QUEUE, hashes);
		self.headers_chain.insert_n(headers);
	}

	/// Moves n blocks from scheduled queue to requested queue
	pub fn request_blocks_hashes(&mut self, n: u32) -> Vec<H256> {
		let scheduled = self.hash_chain.pop_front_n_at(SCHEDULED_QUEUE, n);
		self.hash_chain.push_back_n_at(REQUESTED_QUEUE, scheduled.clone());
		scheduled
	}

	/// Add block to verifying queue
	pub fn verify_block(&mut self, hash: H256, header: BlockHeader) {
		// insert header to the in-memory chain in case when it is not already there (non-headers-first sync)
		self.headers_chain.insert(header);
		self.hash_chain.push_back_at(VERIFYING_QUEUE, hash);
	}

	/// Moves n blocks from requested queue to verifying queue
	#[cfg(test)]
	pub fn verify_blocks_hashes(&mut self, n: u32) -> Vec<H256> {
		let requested = self.hash_chain.pop_front_n_at(REQUESTED_QUEUE, n);
		self.hash_chain.push_back_n_at(VERIFYING_QUEUE, requested.clone());
		requested
	}

	/// Insert new best block to storage
	pub fn insert_best_block(&mut self, hash: H256, block: Block) -> Result<(), db::Error> {
		// insert to storage
		try!(self.storage.insert_block(&block));

		// remember new best block hash
		self.best_storage_block = self.storage.best_block().expect("Inserted block above");

		// remove inserted block + handle possible reorganization in headers chain
		self.headers_chain.block_inserted_to_storage(&hash, &self.best_storage_block.hash);

		Ok(())
	}

	/// Forget in-memory block
	pub fn forget(&mut self, hash: &H256) -> HashPosition {
		let position = self.forget_leave_header(hash);
		if position != HashPosition::Missing {
			self.headers_chain.remove(hash);
		}
		position
	}

	/// Forget in-memory block, but leave its header in the headers_chain (orphan queue)
	pub fn forget_leave_header(&mut self, hash: &H256) -> HashPosition {
		match self.hash_chain.remove_at(VERIFYING_QUEUE, hash) {
			HashPosition::Missing => match self.hash_chain.remove_at(REQUESTED_QUEUE, hash) {
				HashPosition::Missing => self.hash_chain.remove_at(SCHEDULED_QUEUE, hash),
				position @ _ => position,
			},
			position @ _ => position,
		}
	}

	/// Forget in-memory block by hash if it is currently in given state
	#[cfg(test)]
	pub fn forget_with_state(&mut self, hash: &H256, state: BlockState) -> HashPosition {
		let position = self.forget_with_state_leave_header(hash, state);
		if position != HashPosition::Missing {
			self.headers_chain.remove(hash);
		}
		position
	}

	/// Forget in-memory block by hash if it is currently in given state
	pub fn forget_with_state_leave_header(&mut self, hash: &H256, state: BlockState) -> HashPosition {
		self.hash_chain.remove_at(state.to_queue_index(), hash)
	}

	/// Forget in-memory block by hash.
	/// Also forget all its known children.
	pub fn forget_with_children(&mut self, hash: &H256) {
		let mut removal_stack: VecDeque<H256> = VecDeque::new();
		let mut removal_queue: VecDeque<H256> = VecDeque::new();
		removal_queue.push_back(hash.clone());

		// remove in reverse order to minimize headers operations
		while let Some(hash) = removal_queue.pop_front() {
			removal_queue.extend(self.headers_chain.children(&hash));
			removal_stack.push_back(hash);
		}
		while let Some(hash) = removal_stack.pop_back() {
			self.forget(&hash);
		}
	}

	/// Forget all blocks with given state
	pub fn forget_all_with_state(&mut self, state: BlockState) {
		let hashes = self.hash_chain.remove_all_at(state.to_queue_index());
		self.headers_chain.remove_n(hashes);
	}

	/// Intersect chain with inventory
	pub fn intersect_with_headers(&self, hashes: &Vec<H256>, headers: &Vec<BlockHeader>) -> HeadersIntersection {
		let hashes_len = hashes.len();
		assert!(hashes_len != 0 && hashes.len() == headers.len());

		// giving that headers are ordered
		let (is_first_known, first_state) = match self.block_state(&hashes[0]) {
			BlockState::Unknown => (false, self.block_state(&headers[0].previous_header_hash)),
			state @ _ => (true, state),
		};
		match first_state {
			// if first block of inventory is unknown && its parent is unknonw => all other blocks are also unknown
			BlockState::Unknown => {
				HeadersIntersection::NoKnownBlocks(0)
			},
			// else if first block is known
			first_block_state @ _ => match self.block_state(&hashes[hashes_len - 1]) {
				// if last block is known to be in db => all inventory blocks are also in db
				BlockState::Stored => {
					HeadersIntersection::DbAllBlocksKnown 
				},
				// if first block is known && last block is unknown but we know block before first one => intersection with queue or with db
				BlockState::Unknown if !is_first_known => {
					// previous block is stored => fork from stored block
					if first_state == BlockState::Stored {
						return HeadersIntersection::DbForkNewBlocks(0);
					}
					// previous block is best block => no fork
					else if &self.best_block().hash == &headers[0].previous_header_hash {
						return HeadersIntersection::InMemoryMainNewBlocks(0);
					}
					// previous block is not a best block => fork
					else {
						return HeadersIntersection::InMemoryForkNewBlocks(0);
					}
				},
				// if first block is known && last block is unknown => intersection with queue or with db
				BlockState::Unknown if is_first_known => {
					// find last known block
					let mut previous_state = first_block_state;
					for index in 1..hashes_len {
						let state = self.block_state(&hashes[index]);
						if state == BlockState::Unknown {
							// previous block is stored => fork from stored block
							if previous_state == BlockState::Stored {
								return HeadersIntersection::DbForkNewBlocks(index);
							}
							// previous block is best block => no fork
							else if &self.best_block().hash == &hashes[index - 1] {
								return HeadersIntersection::InMemoryMainNewBlocks(index);
							}
							// previous block is not a best block => fork
							else {
								return HeadersIntersection::InMemoryForkNewBlocks(index);
							}
						}
						previous_state = state;
					}

					// unreachable because last block is unknown && in above loop we search for unknown blocks
					unreachable!();
				},
				// if first block is known && last block is also known && is in queue => queue intersection with no new block
				_ => {
					HeadersIntersection::InMemoryNoNewBlocks
				}
			}
		}
	}

	/// Calculate block locator hashes for hash queue
	fn block_locator_hashes_for_queue(&self, hashes: &mut Vec<H256>) -> (u32, u32) {
		let queue_len = self.hash_chain.len();
		if queue_len == 0 {
			return (0, 1);
		}

		let mut index = queue_len - 1;
		let mut step = 1u32;
		loop {
			let block_hash = self.hash_chain[index].clone();
			hashes.push(block_hash);

			if hashes.len() >= 10 {
				step <<= 1;
			}
			if index < step {
				return (step - index - 1, step);
			}
			index -= step;
		}
	}

	/// Calculate block locator hashes for storage
	fn block_locator_hashes_for_storage(&self, mut index: u32, mut step: u32, hashes: &mut Vec<H256>) {
		loop {
			let block_hash = self.storage.block_hash(index)
				.expect("private function; index calculated in `block_locator_hashes`; qed");
			hashes.push(block_hash);

			if hashes.len() >= 10 {
				step <<= 1;
			}
			if index < step {
				// always include genesis hash
				if index != 0 {
					hashes.push(self.genesis_block_hash.clone())
				}

				break;
			}
			index -= step;
		}
	}
}

impl fmt::Debug for Information {
	fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
		write!(f, "[sch:{} / bh:{} -> req:{} -> vfy:{} -> stored: {}]", self.scheduled, self.headers.best, self.requested, self.verifying, self.stored)
	}
}

impl fmt::Debug for Chain {
	fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
		try!(writeln!(f, "chain: ["));
		{
			let mut num = self.best_storage_block.number;
			try!(writeln!(f, "\tworse(stored): {} {:?}", 0, self.storage.block_hash(0)));
			try!(writeln!(f, "\tbest(stored): {} {:?}", num, self.storage.block_hash(num)));

			let queues = vec![
				("verifying", VERIFYING_QUEUE),
				("requested", REQUESTED_QUEUE),
				("scheduled", SCHEDULED_QUEUE),
			];
			for (state, queue) in queues {
				let queue_len = self.hash_chain.len_of(queue);
				if queue_len != 0 {
					try!(writeln!(f, "\tworse({}): {} {:?}", state, num + 1, self.hash_chain.front_at(queue)));
					num += queue_len;
					if let Some(pre_best) = self.hash_chain.pre_back_at(queue) {
						try!(writeln!(f, "\tpre-best({}): {} {:?}", state, num - 1, pre_best));
					}
					try!(writeln!(f, "\tbest({}): {} {:?}", state, num, self.hash_chain.back_at(queue)));
				}
			}
		}
		writeln!(f, "]")
	}
}

#[cfg(test)]
mod tests {
	use std::sync::Arc;
	use chain::RepresentH256;
	use hash_queue::HashPosition;
	use super::{Chain, BlockState, HeadersIntersection};
	use db::{self, Store, BestBlock};
	use primitives::hash::H256;
	use test_data;

	#[test]
	fn chain_empty() {
		let db = Arc::new(db::TestStorage::with_genesis_block());
		let db_best_block = BestBlock { number: 0, hash: db.best_block().expect("storage with genesis block is required").hash };
		let chain = Chain::new(db.clone());
		assert_eq!(chain.information().scheduled, 0);
		assert_eq!(chain.information().requested, 0);
		assert_eq!(chain.information().verifying, 0);
		assert_eq!(chain.information().stored, 1);
		assert_eq!(chain.length_of_state(BlockState::Scheduled), 0);
		assert_eq!(chain.length_of_state(BlockState::Requested), 0);
		assert_eq!(chain.length_of_state(BlockState::Verifying), 0);
		assert_eq!(chain.length_of_state(BlockState::Stored), 1);
		assert_eq!(&chain.best_block(), &db_best_block);
		assert_eq!(chain.block_state(&db_best_block.hash), BlockState::Stored);
		assert_eq!(chain.block_state(&H256::from(0)), BlockState::Unknown);
	}

	#[test]
	fn chain_block_path() {
		let db = Arc::new(db::TestStorage::with_genesis_block());
		let mut chain = Chain::new(db.clone());

		// add 6 blocks to scheduled queue
		let blocks = test_data::build_n_empty_blocks_from_genesis(6, 0);
		let headers: Vec<_> = blocks.into_iter().map(|b| b.block_header).collect();
		let hashes: Vec<_> = headers.iter().map(|h| h.hash()).collect();
		chain.schedule_blocks_headers(hashes.clone(), headers);
		assert!(chain.information().scheduled == 6 && chain.information().requested == 0
			&& chain.information().verifying == 0 && chain.information().stored == 1);

		// move 2 best blocks to requested queue
		chain.request_blocks_hashes(2);
		assert!(chain.information().scheduled == 4 && chain.information().requested == 2
			&& chain.information().verifying == 0 && chain.information().stored == 1);
		// move 0 best blocks to requested queue
		chain.request_blocks_hashes(0);
		assert!(chain.information().scheduled == 4 && chain.information().requested == 2
			&& chain.information().verifying == 0 && chain.information().stored == 1);
		// move 1 best blocks to requested queue
		chain.request_blocks_hashes(1);
		assert!(chain.information().scheduled == 3 && chain.information().requested == 3
			&& chain.information().verifying == 0 && chain.information().stored == 1);

		// try to remove block 0 from scheduled queue => missing
		assert_eq!(chain.forget_with_state(&hashes[0], BlockState::Scheduled), HashPosition::Missing);
		assert!(chain.information().scheduled == 3 && chain.information().requested == 3
			&& chain.information().verifying == 0 && chain.information().stored == 1);
		// remove blocks 0 & 1 from requested queue
		assert_eq!(chain.forget_with_state(&hashes[1], BlockState::Requested), HashPosition::Inside(1));
		assert_eq!(chain.forget_with_state(&hashes[0], BlockState::Requested), HashPosition::Front);
		assert!(chain.information().scheduled == 3 && chain.information().requested == 1
			&& chain.information().verifying == 0 && chain.information().stored == 1);
		// mark 0 & 1 as verifying
		chain.verify_block(hashes[1].clone(), test_data::genesis().block_header);
		chain.verify_block(hashes[2].clone(), test_data::genesis().block_header);
		assert!(chain.information().scheduled == 3 && chain.information().requested == 1
			&& chain.information().verifying == 2 && chain.information().stored == 1);

		// mark block 0 as verified
		assert_eq!(chain.forget_with_state(&hashes[1], BlockState::Verifying), HashPosition::Front);
		assert!(chain.information().scheduled == 3 && chain.information().requested == 1
			&& chain.information().verifying == 1 && chain.information().stored == 1);
		// insert new best block to the chain
		chain.insert_best_block(test_data::block_h1().hash(), test_data::block_h1()).expect("Db error");
		assert!(chain.information().scheduled == 3 && chain.information().requested == 1
			&& chain.information().verifying == 1 && chain.information().stored == 2);
		assert_eq!(db.best_block().expect("storage with genesis block is required").number, 1);
	}

	#[test]
	fn chain_block_locator_hashes() {
		let mut chain = Chain::new(Arc::new(db::TestStorage::with_genesis_block()));
		let genesis_hash = chain.best_block().hash;
		assert_eq!(chain.block_locator_hashes(), vec![genesis_hash.clone()]);

		let block1 = test_data::block_h1();
		let block1_hash = block1.hash();

		chain.insert_best_block(block1_hash.clone(), block1).expect("Error inserting new block");
		assert_eq!(chain.block_locator_hashes(), vec![block1_hash.clone(), genesis_hash.clone()]);

		let block2 = test_data::block_h2();
		let block2_hash = block2.hash();

		chain.insert_best_block(block2_hash.clone(), block2).expect("Error inserting new block");
		assert_eq!(chain.block_locator_hashes(), vec![block2_hash.clone(), block1_hash.clone(), genesis_hash.clone()]);

		let blocks0 = test_data::build_n_empty_blocks_from_genesis(11, 0);
		let headers0: Vec<_> = blocks0.into_iter().map(|b| b.block_header).collect();
		let hashes0: Vec<_> = headers0.iter().map(|h| h.hash()).collect();
		chain.schedule_blocks_headers(hashes0.clone(), headers0.clone());
		chain.request_blocks_hashes(10);
		chain.verify_blocks_hashes(10);

		assert_eq!(chain.block_locator_hashes(), vec![
			hashes0[10].clone(),
			hashes0[9].clone(),
			hashes0[8].clone(),
			hashes0[7].clone(),
			hashes0[6].clone(),
			hashes0[5].clone(),
			hashes0[4].clone(),
			hashes0[3].clone(),
			hashes0[2].clone(),
			hashes0[1].clone(),
			block2_hash.clone(),
			genesis_hash.clone(),
		]);

		let blocks1 = test_data::build_n_empty_blocks_from(6, 0, &headers0[10]);
		let headers1: Vec<_> = blocks1.into_iter().map(|b| b.block_header).collect();
		let hashes1: Vec<_> = headers1.iter().map(|h| h.hash()).collect();
		chain.schedule_blocks_headers(hashes1.clone(), headers1.clone());
		chain.request_blocks_hashes(10);

		assert_eq!(chain.block_locator_hashes(), vec![
			hashes1[5].clone(),
			hashes1[4].clone(),
			hashes1[3].clone(),
			hashes1[2].clone(),
			hashes1[1].clone(),
			hashes1[0].clone(),
			hashes0[10].clone(),
			hashes0[9].clone(),
			hashes0[8].clone(),
			hashes0[7].clone(),
			hashes0[5].clone(),
			hashes0[1].clone(),
			genesis_hash.clone(),
		]);

		let blocks2 = test_data::build_n_empty_blocks_from(3, 0, &headers1[5]);
		let headers2: Vec<_> = blocks2.into_iter().map(|b| b.block_header).collect();
		let hashes2: Vec<_> = headers2.iter().map(|h| h.hash()).collect();
		chain.schedule_blocks_headers(hashes2.clone(), headers2);

		assert_eq!(chain.block_locator_hashes(), vec![
			hashes2[2].clone(),
			hashes2[1].clone(),
			hashes2[0].clone(),
			hashes1[5].clone(),
			hashes1[4].clone(),
			hashes1[3].clone(),
			hashes1[2].clone(),
			hashes1[1].clone(),
			hashes1[0].clone(),
			hashes0[10].clone(),
			hashes0[8].clone(),
			hashes0[4].clone(),
			genesis_hash.clone(),
		]);
	}

	#[test]
	fn chain_intersect_with_inventory() {
		let mut chain = Chain::new(Arc::new(db::TestStorage::with_genesis_block()));
		// append 2 db blocks
		chain.insert_best_block(test_data::block_h1().hash(), test_data::block_h1()).expect("Error inserting new block");
		chain.insert_best_block(test_data::block_h2().hash(), test_data::block_h2()).expect("Error inserting new block");

		// prepare blocks
		let blocks0 = test_data::build_n_empty_blocks_from(9, 0, &test_data::block_h2().block_header);
		let headers0: Vec<_> = blocks0.into_iter().map(|b| b.block_header).collect();
		let hashes0: Vec<_> = headers0.iter().map(|h| h.hash()).collect();
		// append 3 verifying blocks, 3 requested blocks && 3 scheduled blocks
		chain.schedule_blocks_headers(hashes0.clone(), headers0.clone());
		chain.request_blocks_hashes(6);
		chain.verify_blocks_hashes(3);

		let blocks1 = test_data::build_n_empty_blocks(2, 0);
		let headers1: Vec<_> = blocks1.into_iter().map(|b| b.block_header).collect();
		let hashes1: Vec<_> = headers1.iter().map(|h| h.hash()).collect();
		assert_eq!(chain.intersect_with_headers(&hashes1, &headers1), HeadersIntersection::NoKnownBlocks(0));

		assert_eq!(chain.intersect_with_headers(&vec![
			hashes0[2].clone(),
			hashes0[3].clone(),
			hashes0[4].clone(),
			hashes0[5].clone(),
			hashes0[6].clone(),
		], &vec![
			headers0[2].clone(),
			headers0[3].clone(),
			headers0[4].clone(),
			headers0[5].clone(),
			headers0[6].clone(),
		]), HeadersIntersection::InMemoryNoNewBlocks);

		assert_eq!(chain.intersect_with_headers(&vec![
			hashes0[7].clone(),
			hashes0[8].clone(),
			hashes1[0].clone(),
			hashes1[1].clone(),
		], &vec![
			headers0[7].clone(),
			headers0[8].clone(),
			headers1[0].clone(),
			headers1[1].clone(),
		]), HeadersIntersection::InMemoryMainNewBlocks(2));

		assert_eq!(chain.intersect_with_headers(&vec![
			hashes0[6].clone(),
			hashes0[7].clone(),
			hashes1[0].clone(),
			hashes1[1].clone(),
		], &vec![
			headers0[6].clone(),
			headers0[7].clone(),
			headers1[0].clone(),
			headers1[1].clone(),
		]), HeadersIntersection::InMemoryForkNewBlocks(2));

		assert_eq!(chain.intersect_with_headers(&vec![
			test_data::block_h1().hash(),
			test_data::block_h2().hash(),
		], &vec![
			test_data::block_h1().block_header,
			test_data::block_h2().block_header,
		]), HeadersIntersection::DbAllBlocksKnown);

		assert_eq!(chain.intersect_with_headers(&vec![
			test_data::block_h2().hash(),
			hashes1[0].clone(),
		], &vec![
			test_data::block_h2().block_header,
			headers1[0].clone(),
		]), HeadersIntersection::DbForkNewBlocks(1));
	}
}