mod signer_snapshot;
mod params;
mod step_service;

use rlp::{encode_list, encode, Decodable, DecoderError, Encodable, RlpStream, Rlp};
use std::time::{Duration};
use itertools::Itertools;

use std::sync::{Weak, Arc};
use std::collections::{BTreeMap, HashMap};
use std::{fmt, error};
use std::str::FromStr;
use hash::{keccak};

use self::params::CliqueParams;
use self::step_service::StepService;

use super::epoch::{PendingTransition,EpochVerifier,NoOp};

use account_provider::AccountProvider;
use builtin::Builtin;
use vm::{EnvInfo, Schedule, CreateContractAddress, CallType, ActionValue};
use error::Error;
use header::{Header, BlockNumber, ExtendedHeader};
use snapshot::SnapshotComponents;
use spec::CommonParams;
use transaction::{self, UnverifiedTransaction, SignedTransaction};
use client::EngineClient;
use parking_lot::RwLock;
use block::*;
use io::IoService;

use ethkey::{Password, Signature, recover as ec_recover, public_to_address};
use parity_machine::{Machine, LocalizedMachine as Localized, TotalScoredHeader};
use ethereum_types::{H256, U256, Address, Public};
use unexpected::{Mismatch, OutOfBounds};
use bytes::Bytes;
use types::ancestry_action::AncestryAction;
use engines::{Engine, Seal, EngineError, ConstructedVerifier, Headers, PendingTransitionStore};
use super::validator_set::{ValidatorSet, SimpleList};
use super::signer::EngineSigner;
use machine::{Call, AuxiliaryData, EthereumMachine};
use self::signer_snapshot::{SignerSnapshot, SignerAuthorization, NONCE_AUTH_VOTE, NONCE_DROP_VOTE, NULL_AUTHOR};

pub const SIGNER_VANITY_LENGTH: u32 = 32;  // Fixed number of extra-data prefix bytes reserved for signer vanity
//const EXTRA_DATA_POST_LENGTH: u32 = 128;
pub const SIGNER_SIG_LENGTH: u32 = 65; // Fixed number of extra-data suffix bytes reserved for signer seal

pub struct Clique {
  client: RwLock<Option<Weak<EngineClient>>>,
  snapshot: RwLock<Option<SignerSnapshot>>,
  //signers: RwLock<Option<Vec<Address>>>,
  machine: EthereumMachine,
  step_service: IoService<Duration>,
  epoch_length: u64,
  period: u64,
}

/*
 * only sign over non-signature bytes (vanity data).  There shouldn't be a signature here to sign
 * yet.
 */
pub fn sig_hash(header: &Header) -> Result<H256, Error> {
  if header.extra_data().len() >= SIGNER_VANITY_LENGTH as usize {
	let extra_data = header.extra_data().clone();
    let mut reduced_header = header.clone();
	  reduced_header.set_extra_data(
		  extra_data[..extra_data.len() - SIGNER_SIG_LENGTH as usize].to_vec());

    trace!(target: "engine", "Header RLP {}", reduced_header.encoded().rlp());

    //Ok(keccak(::rlp::encode(&reduced_header)))
    Ok(reduced_header.hash())
  } else {
    Ok(header.hash())
    //Ok(keccak(::rlp::encode(header)))
  }

}

fn recover(header: &Header) -> Result<Public, Error> {
	let data = header.extra_data();
	let mut sig: [u8; 65] = [0; 65];
	sig.copy_from_slice(&data[(data.len() - SIGNER_SIG_LENGTH as usize)..]);

	let msg = sig_hash(header).unwrap();
	let pubkey = ec_recover(&Signature::from(sig), &msg).unwrap();

	Ok(pubkey)
}

const step_time: Duration = Duration::from_millis(100);

impl Clique {

  /// Check if current signer is the current proposer.
  fn is_signer_proposer(&self, bn: u64) -> bool {
    let mut authorized = false;
    if let Some(ref snapshot) = *self.snapshot.read() {
        let signers = &snapshot.final_state.signers;
        authorized = if let Some(pos) = signers.iter().position(|x| self.snapshot.read().unwrap().signer.read().is_address(x)) {
          bn % signers.len() as u64 == pos as u64
        } else {
          false
        };
        return authorized;
    };

    return false;
  }

  pub fn new(our_params: CliqueParams, machine: EthereumMachine) -> Result<Arc<Self>, Error> {
    // don't let there be any duplicate signers

    //length of signers must be greater than 1
    //

    trace!(target: "engine", "epoch length: {}, period: {}", our_params.epoch, our_params.period);
    /*
    let snapshot = SignerSnapshot {
      bn: 0,
      signers: vec![],
      epoch_length: our_params.epoch,
      votes: HashMap::<Address, (bool, Address)>::new(),
    };
    */

    let engine = Arc::new(
	  Clique {
		  client: RwLock::new(None),
          snapshot: RwLock::new(None),
		  machine: machine,
		  step_service: IoService::<Duration>::start()?,
          epoch_length: our_params.epoch,
          period:  our_params.period,
		});


	let handler = StepService::new(Arc::downgrade(&engine) as Weak<Engine<_>>, step_time);
	engine.step_service.register_handler(Arc::new(handler))?;

    return Ok(engine);
  }

  fn sign_header(&self, header: &Header) -> Result<(Signature, H256), Error> {
    let digest = sig_hash(header)?;
    if let Ok(sig) = self.snapshot.read().unwrap().signer.read().sign(digest) {
      Ok((sig, digest))
    } else {
      Err(Box::new("failed to sign header").into())
    }
  }

  //pub fn snapshot(self, bn: u64) -> AuthorizationSnapshot {
    // if we are on a checkpoint block, build a snapshot
  //}
}

impl Engine<EthereumMachine> for Clique {
  fn name(&self) -> &str { "Clique" }

  // nonce + mixHash + extraData
  fn seal_fields(&self, _header: &Header) -> usize { 2 }
  fn machine(&self) -> &EthereumMachine { &self.machine }
  fn maximum_uncle_count(&self, _block: BlockNumber) -> usize { 0 }

  // called only when sealing ?
  fn populate_from_parent(&self, header: &mut Header, parent: &Header) {
    // if in turn, set difficulty 
    //
    if self.is_signer_proposer(header.number()) {
        let mut address = Address::new();
        address.0 = NULL_AUTHOR.clone();
        header.set_author(address);
    }
  }

  // only called when we are sealing the block.  TODO rename this to make more sense
  fn close_block_extra_data(&self, _header: &Header) -> Option<Vec<u8>> {
    let mut h = _header.clone();

     if let Some(ref mut snapshot) = *self.snapshot.write() {
       trace!(target: "engine", "applying sealed block");
       let mut v: Vec<u8> = vec![0; SIGNER_VANITY_LENGTH as usize+SIGNER_SIG_LENGTH as usize];

       {
           let signers = &snapshot.final_state.signers;
           trace!(target: "engine", "applied.  found {} signers", signers.len());

           //let mut v: Vec<u8> = vec![0; SIGNER_VANITY_LENGTH as usize+SIGNER_SIG_LENGTH as usize];
           let mut sig_offset = SIGNER_VANITY_LENGTH as usize;

           if _header.number() %self.epoch_length == 0 {
               sig_offset += 20 * signers.len();

               for i in 0..signers.len() {
                 v[SIGNER_VANITY_LENGTH as usize+i*20..SIGNER_VANITY_LENGTH as usize+(i+1)*20].clone_from_slice(&signers[i]);
               }
           }

           h.set_extra_data(v.clone());

           let (sig, msg) = self.sign_header(&h).expect("should be able to sign header");
           v[sig_offset..].copy_from_slice(&sig[..]);

           trace!(target: "engine", "header hash: {}", h.hash());
           trace!(target: "engine", "Sig: {}", sig);
           trace!(target: "engine", "Message: {:02x}", msg.iter().format(""));

           //trace!(target: "engine", "we are {}", self.signer.read().address().unwrap());
       }

      snapshot.apply(_header);

       return Some(v);
     } else {
       panic!("failed to populate extra data when sealing");
     }

     return None;
  }

  fn set_signer(&self, ap: Arc<AccountProvider>, address: Address, password: Password) {
    trace!("setting the signer to {}", address);
    if let Some(ref mut snap) = *self.snapshot.write() {
        snap.set_signer(ap, address, password);
    }
  }

  /// None means that it requires external input (e.g. PoW) to seal a block.
  /// /// Some(true) means the engine is currently prime for seal generation (i.e. node
  ///     is the current validator).
  /// /// Some(false) means that the node might seal internally but is not qualified
  ///     now.
  ///
  fn seals_internally(&self) -> Option<bool> {
    Some(true)
  }

  /// Attempt to seal generate a proposal seal.
  ///
  /// This operation is synchronous and may (quite reasonably) not be available, in which case
  /// `Seal::None` will be returned.
  fn generate_seal(&self, block: &ExecutedBlock, _parent: &Header) -> Seal {
    let mut header = block.header.clone();

    trace!(target: "engine", "attempting to seal...");

    // don't seal the genesis block
    if header.number() == 0 {
      trace!(target: "engine", "attempted to seal genesis block");
      return Seal::None;
    }

    // if sealing period is 0, refuse to seal

    // let vote_snapshot = self.snapshot.get(bh);

    // if we are not authorized to sign, don't seal

    // if we signed recently, don't seal

    if block.header.timestamp() <= _parent.timestamp() + self.period {
      trace!(target: "engine", "block too early");
      return Seal::None;
    }

    if let SignerAuthorization::Unauthorized = self.snapshot.read().unwrap().get_own_authorization() {
        return Seal::None;
    }

    // sign the digest of the seal
    if self.is_signer_proposer(block.header().number()) {
        trace!(target: "engine", "seal generated for {}", block.header().number());
        //TODO add our vote here if this is not an epoch transition
        return Seal::Regular(vec![encode(&vec![0; 32]), encode(&vec![0; 8])]);
    } else {
        trace!(target: "engine", "we are not the current for block {}", block.header().number());
        Seal::None
    }
  }

  fn on_close_block(&self, block: &mut ExecutedBlock) -> Result<(), Error>{
      /*
       * TODO:
      if not checkpoint block:
        if the block was successfully sealed, then grab the signature from the seal data and
        append it to the block extraData
        */
    // trace!(target: "engine", "closing block {}...", block.header().number());

    if block.header().seal().len() > 0 {
        trace!(target: "engine", "seal length was {}", block.header().seal().len());
        //trace!(target: "engine", "signature written {}", );
    }

    Ok(())
  }

  fn on_new_block(
    &self,
    _block: &mut ExecutedBlock,
    _epoch_begin: bool,
    _ancestry: &mut Iterator<Item=ExtendedHeader>,
  ) -> Result<(), Error> {
    trace!(target: "engine", "new block {}", _block.header().number());

    /*
    if let Some(ref mut snapshot) = *self.snapshot.write() {
      snapshot.rollback();
    } else {
      panic!("could not get write access to snapshot");
    }
    */

    /*
    if let Some(ref mut snapshot) = *self.snapshot.write() {
        snapshot.apply(_block.header());
    }
    */

    Ok(())
  }

	fn executive_author (&self, header: &Header) -> Address {
        trace!(target: "engine", "called executive_author for block {}", header.number());

        if self.is_signer_proposer(header.number()) {
          return (*self.snapshot.read()).unwrap().signer.read().address().expect("asdf");
        } else {
            public_to_address(
                &recover(header).unwrap()
            )
        }
	}

  fn verify_block_basic(&self, _header: &Header) -> Result<(), Error> { 
    trace!(target: "engine", "verify_block_basic {}", _header.number());

    if let Some(ref mut snap) = *self.snapshot.write() {
      snap.apply(_header);
    } else {
      panic!("snapshot should be able to be committed");
    }

    Ok(()) 
  }

  fn verify_block_unordered(&self, _header: &Header) -> Result<(), Error> {
	  // Verifying the genesis block is not supported
	  // Retrieve the snapshot needed to verify this header and cache it
	  // Resolve the authorization key and check against signers
	  // Ensure that the difficulty corresponds to the turn-ness of the signer
	  Ok(())
  }


	fn signals_epoch_end(&self, header: &Header, aux: AuxiliaryData)
      -> super::EpochChange<EthereumMachine>
  {
    super::EpochChange::No
  }

	fn is_epoch_end(
		&self,
		chain_head: &Header,
		_finalized: &[H256],
		_chain: &Headers<Header>,
		_transition_store: &PendingTransitionStore,
	) -> Option<Vec<u8>> {
      None
  }

  fn epoch_verifier<'a>(&self, _header: &Header, proof: &'a [u8]) -> ConstructedVerifier<'a, EthereumMachine> {
    ConstructedVerifier::Trusted(Box::new(super::epoch::NoOp))
  }

  /*
   * Continuously trigger attempts to seal new blocks
   */
  fn step(&self) {
	if let Some(ref weak) = *self.client.read() {
		if let Some(c) = weak.upgrade() {
			c.update_sealing();
		}
	}
  }

  fn sign(&self, hash: H256) -> Result<Signature, Error> {
    Ok(self.snapshot.read().unwrap().signer.read().sign(hash)?)
  }

  fn stop(&self) { }

  fn register_client(&self, client: Weak<EngineClient>) {
	*self.client.write() = Some(client.clone());
	//self.validators.register_client(client);
  }

  fn verify_local_seal(&self, header: &Header) -> Result<(), Error> { Ok(()) }

  fn fork_choice(&self, new: &ExtendedHeader, current: &ExtendedHeader) -> super::ForkChoice {
    super::total_difficulty_fork_choice(new, current)
  }

  /*
   *  Extract signer addresses from header extraData
   */
  fn genesis_epoch_data<'a>(&self, _header: &Header, call: &Call) -> Result<Vec<u8>, String> {
    // extract signer list from genesis extradata
      {
        trace!(target: "engine", "genesis_epoch_data received");

        let mut snapshot = SignerSnapshot::new(self.epoch_length);
        snapshot.apply(_header);
        snapshot.commit();
        *self.snapshot.write() = Some(snapshot);
      }
    Ok(Vec::new())
  }

  fn is_timestamp_valid(&self, header_timestamp: u64, parent_timestamp: u64) -> bool {
    trace!(target: "engine", "is_timestamp_valid");
    header_timestamp >= parent_timestamp + self.period
  }
}
