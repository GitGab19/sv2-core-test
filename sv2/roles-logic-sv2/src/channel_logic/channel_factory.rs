//! # Channel Factory
//!
//! This module contains logic for creating and managing channels.

use crate::{
    job_creator::{self, JobsCreators},
    utils::{GroupId, Id, Mutex},
    Error,
};

use codec_sv2::binary_sv2;
use mining_sv2::{
    ExtendedExtranonce, NewExtendedMiningJob, OpenExtendedMiningChannelSuccess,
    OpenMiningChannelError, SetCustomMiningJob, SetCustomMiningJobSuccess, SetNewPrevHash,
    SubmitSharesError, SubmitSharesExtended, SubmitSharesStandard, Target,
};
use parsers_sv2::Mining;

use hex::DisplayHex;
use nohash_hasher::BuildNoHashHasher;
use std::{collections::HashMap, convert::TryInto, sync::Arc};
use template_distribution_sv2::{NewTemplate, SetNewPrevHash as SetNewPrevHashFromTp};

use tracing::{debug, error, info, trace, warn};

use bitcoin::{
    block::{Header, Version},
    hash_types,
    hashes::sha256d::Hash,
    CompactTarget, TxOut,
};

/// A stripped type of `SetCustomMiningJob` without the (`channel_id, `request_id` and `token`)
/// fields
#[derive(Debug)]
pub struct PartialSetCustomMiningJob {
    pub version: u32,
    pub prev_hash: binary_sv2::U256<'static>,
    pub min_ntime: u32,
    pub nbits: u32,
    pub coinbase_tx_version: u32,
    pub coinbase_prefix: binary_sv2::B0255<'static>,
    pub coinbase_tx_input_n_sequence: u32,
    pub coinbase_tx_value_remaining: u64,
    pub coinbase_tx_outputs: binary_sv2::B064K<'static>,
    pub coinbase_tx_locktime: u32,
    pub merkle_path: binary_sv2::Seq0255<'static, binary_sv2::U256<'static>>,
    pub future_job: bool,
}

/// Represents the action that needs to be done when a new share is received.
#[derive(Debug, Clone)]
pub enum OnNewShare {
    /// Used when the received is malformed, is for an inexistent channel or do not meet downstream
    /// target.
    SendErrorDownstream(SubmitSharesError<'static>),
    /// Used when an extended channel in a proxy receive a share, and the share meet upstream
    /// target, in this case a new share must be sent upstream. Also an optional template id is
    /// returned, when a job declarator want to send a valid share upstream could use the
    /// template for get the up job id.
    SendSubmitShareUpstream((Share, Option<u64>)),
    /// Used when a group channel in a proxy receive a share that is not malformed and is for a
    /// valid channel in that case we relay the same exact share upstream with a new request id.
    RelaySubmitShareUpstream,
    /// Indicate that the share meet bitcoin target, when there is an upstream the we should send
    /// the share upstream, whenever possible we should also notify the TP about it.
    /// When a pool negotiate a job with downstream we do not have the template_id so we set it to
    /// None
    /// (share, template id, coinbase,complete extranonce)
    ShareMeetBitcoinTarget((Share, Option<u64>, Vec<u8>, Vec<u8>)),
    /// Indicate that the share meet downstream target, in the case we could send a success
    /// response downstream.
    ShareMeetDownstreamTarget,
}

impl OnNewShare {
    /// Converts standard share into extended share
    pub fn into_extended(&mut self, extranonce: Vec<u8>, up_id: u32) {
        match self {
            OnNewShare::SendErrorDownstream(_) => (),
            OnNewShare::SendSubmitShareUpstream((share, template_id)) => match share {
                Share::Extended(_) => (),
                Share::Standard((share, _)) => {
                    let share = SubmitSharesExtended {
                        channel_id: up_id,
                        sequence_number: share.sequence_number,
                        job_id: share.job_id,
                        nonce: share.nonce,
                        ntime: share.ntime,
                        version: share.version,
                        extranonce: extranonce.try_into().unwrap(),
                    };
                    *self = Self::SendSubmitShareUpstream((Share::Extended(share), *template_id));
                }
            },
            OnNewShare::RelaySubmitShareUpstream => (),
            OnNewShare::ShareMeetBitcoinTarget((share, t_id, coinbase, ext)) => match share {
                Share::Extended(_) => (),
                Share::Standard((share, _)) => {
                    let share = SubmitSharesExtended {
                        channel_id: up_id,
                        sequence_number: share.sequence_number,
                        job_id: share.job_id,
                        nonce: share.nonce,
                        ntime: share.ntime,
                        version: share.version,
                        extranonce: extranonce.try_into().unwrap(),
                    };
                    *self = Self::ShareMeetBitcoinTarget((
                        Share::Extended(share),
                        *t_id,
                        coinbase.clone(),
                        ext.to_vec(),
                    ));
                }
            },
            OnNewShare::ShareMeetDownstreamTarget => todo!(),
        }
    }
}

/// A share can be either extended or standard
#[derive(Clone, Debug)]
pub enum Share {
    Extended(SubmitSharesExtended<'static>),
    // share, group id
    Standard((SubmitSharesStandard, u32)),
}

/// Helper type used before a `SetNewPrevHash` has a channel_id
#[derive(Clone, Debug)]
pub struct StagedPhash {
    job_id: u32,
    prev_hash: binary_sv2::U256<'static>,
    min_ntime: u32,
    nbits: u32,
}

impl StagedPhash {
    /// Converts a Staged PrevHash into a SetNewPrevHash message
    pub fn into_set_p_hash(
        &self,
        channel_id: u32,
        new_job_id: Option<u32>,
    ) -> SetNewPrevHash<'static> {
        SetNewPrevHash {
            channel_id,
            job_id: new_job_id.unwrap_or(self.job_id),
            prev_hash: self.prev_hash.clone(),
            min_ntime: self.min_ntime,
            nbits: self.nbits,
        }
    }
}

impl Share {
    /// Get share sequence number
    pub fn get_sequence_number(&self) -> u32 {
        match self {
            Share::Extended(s) => s.sequence_number,
            Share::Standard(s) => s.0.sequence_number,
        }
    }

    /// Get share channel id
    pub fn get_channel_id(&self) -> u32 {
        match self {
            Share::Extended(s) => s.channel_id,
            Share::Standard(s) => s.0.channel_id,
        }
    }

    /// Get share timestamp
    pub fn get_n_time(&self) -> u32 {
        match self {
            Share::Extended(s) => s.ntime,
            Share::Standard(s) => s.0.ntime,
        }
    }

    /// Get share nonce
    pub fn get_nonce(&self) -> u32 {
        match self {
            Share::Extended(s) => s.nonce,
            Share::Standard(s) => s.0.nonce,
        }
    }

    /// Get share job id
    pub fn get_job_id(&self) -> u32 {
        match self {
            Share::Extended(s) => s.job_id,
            Share::Standard(s) => s.0.job_id,
        }
    }

    /// Get share version
    pub fn get_version(&self) -> u32 {
        match self {
            Share::Extended(s) => s.version,
            Share::Standard(s) => s.0.version,
        }
    }
}

#[derive(Debug)]
/// Basic logic shared between all the channel factories
struct ChannelFactory {
    ids: Arc<Mutex<GroupId>>,
    extended_channels:
        HashMap<u32, OpenExtendedMiningChannelSuccess<'static>, BuildNoHashHasher<u32>>,
    extranonces: ExtendedExtranonce,
    share_per_min: f32,
    // (NewExtendedMiningJob,group ids that already received the future job)
    future_jobs: Vec<(NewExtendedMiningJob<'static>, Vec<u32>)>,
    // (SetNewPrevHash,group ids that already received the set prev_hash)
    last_prev_hash: Option<(StagedPhash, Vec<u32>)>,
    last_prev_hash_: Option<hash_types::BlockHash>,
    // (NewExtendedMiningJob,group ids that already received the job)
    last_valid_job: Option<(NewExtendedMiningJob<'static>, Vec<u32>)>,
    kind: ExtendedChannelKind,
    job_ids: Id,
    channel_to_group_id: HashMap<u32, u32, BuildNoHashHasher<u32>>,
    future_templates: HashMap<u32, NewTemplate<'static>, BuildNoHashHasher<u32>>,
}

impl ChannelFactory {
    /// Called when a `OpenExtendedMiningChannel` message is received.
    /// Here we save the downstream's target (based on hashrate) and the
    /// channel's extranonce details before returning the relevant SV2 mining messages
    /// to be sent downstream. For the mining messages, we will first return an
    /// `OpenExtendedMiningChannelSuccess` if the channel is successfully opened. Then we add
    /// the `NewExtendedMiningJob` and `SetNewPrevHash` messages if the relevant data is
    /// available. If the channel opening fails, we return `OpenExtendedMiningChannelError`.
    pub fn new_extended_channel(
        &mut self,
        request_id: u32,
        hash_rate: f32,
        min_extranonce_size: u16,
    ) -> Result<Vec<Mining<'static>>, Error> {
        let extended_channels_group = 0;
        let max_extranonce_size = self.extranonces.get_range2_len() as u16;
        if min_extranonce_size <= max_extranonce_size {
            // SECURITY is very unlikely to finish the ids btw this unwrap could be used by an
            // attacker that want to disrupt the service maybe we should have a method
            // to reuse ids that are no longer connected?
            let channel_id = self
                .ids
                .safe_lock(|ids| ids.new_channel_id(extended_channels_group))
                .unwrap();
            self.channel_to_group_id.insert(channel_id, 0);
            let target = match crate::utils::hash_rate_to_target(
                hash_rate.into(),
                self.share_per_min.into(),
            ) {
                Ok(target) => target,
                Err(e) => {
                    error!(
                        "Impossible to get target: {:?}. Request id: {:?}",
                        e, request_id
                    );
                    return Err(e);
                }
            };
            let extranonce_prefix = self
                .extranonces
                .next_prefix_extended(max_extranonce_size as usize)
                .unwrap()
                .into_b032();
            let success = OpenExtendedMiningChannelSuccess {
                request_id,
                channel_id,
                target,
                extranonce_size: max_extranonce_size,
                extranonce_prefix,
            };
            self.extended_channels.insert(channel_id, success.clone());
            let mut result = vec![Mining::OpenExtendedMiningChannelSuccess(success)];
            if let Some((job, _)) = &self.last_valid_job {
                let mut job = job.clone();
                job.set_future();
                let j_id = job.job_id;
                result.push(Mining::NewExtendedMiningJob(job));
                if let Some((new_prev_hash, _)) = &self.last_prev_hash {
                    let mut new_prev_hash = new_prev_hash.into_set_p_hash(channel_id, None);
                    new_prev_hash.job_id = j_id;
                    result.push(Mining::SetNewPrevHash(new_prev_hash.clone()))
                };
            } else if let Some((new_prev_hash, _)) = &self.last_prev_hash {
                let new_prev_hash = new_prev_hash.into_set_p_hash(channel_id, None);
                result.push(Mining::SetNewPrevHash(new_prev_hash.clone()))
            };
            for (job, _) in &self.future_jobs {
                result.push(Mining::NewExtendedMiningJob(job.clone()))
            }
            Ok(result)
        } else {
            Ok(vec![Mining::OpenMiningChannelError(
                OpenMiningChannelError::unsupported_extranonce_size(request_id),
            )])
        }
    }

    /// Called when we want to replicate a channel already opened by another actor.
    /// It is used only in the jd client from the template provider module to mock a pool.
    /// Anything else should open channel with the new_extended_channel function
    pub fn replicate_upstream_extended_channel_only_jd(
        &mut self,
        target: binary_sv2::U256<'static>,
        extranonce: mining_sv2::Extranonce,
        channel_id: u32,
        extranonce_size: u16,
    ) -> Option<()> {
        self.channel_to_group_id.insert(channel_id, 0);
        let extranonce_prefix = extranonce.into();
        let success = OpenExtendedMiningChannelSuccess {
            request_id: 0,
            channel_id,
            target,
            extranonce_size,
            extranonce_prefix,
        };
        self.extended_channels.insert(channel_id, success.clone());
        Some(())
    }

    /// Called when a new prev hash is received. If the respective job is available in the future
    /// job queue, we move the future job into the valid job slot and store the prev hash as the
    /// current prev hash to be referenced.
    fn on_new_prev_hash(&mut self, m: StagedPhash) -> Result<(), Error> {
        while let Some(mut job) = self.future_jobs.pop() {
            if job.0.job_id == m.job_id {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs() as u32;
                job.0.set_no_future(now);
                self.last_valid_job = Some(job);
                break;
            }
            self.last_valid_job = None;
        }
        self.future_jobs = vec![];
        self.last_prev_hash_ = Some(crate::utils::u256_to_block_hash(m.prev_hash.clone()));
        self.last_prev_hash = Some((m, vec![]));
        Ok(())
    }

    /// Called when a `NewExtendedMiningJob` arrives. If the job is future, we add it to the future
    /// queue. If the job is not future, we pair it with a the most recent prev hash
    fn on_new_extended_mining_job(
        &mut self,
        m: NewExtendedMiningJob<'static>,
    ) -> Result<HashMap<u32, Mining<'static>, BuildNoHashHasher<u32>>, Error> {
        match (m.is_future(), &self.last_prev_hash) {
            (true, _) => {
                let mut result = HashMap::with_hasher(BuildNoHashHasher::default());
                self.prepare_jobs_for_downstream_on_new_extended(&mut result, &m)?;
                self.future_jobs.push((m, vec![]));
                Ok(result)
            }
            (false, Some(_)) => {
                let mut result = HashMap::with_hasher(BuildNoHashHasher::default());
                self.prepare_jobs_for_downstream_on_new_extended(&mut result, &m)?;
                // If job is not future it must always be paired with the last received prev hash
                self.last_valid_job = Some((m, vec![]));
                if let Some((_p_hash, _)) = &self.last_prev_hash {
                    Ok(result)
                } else {
                    Err(Error::JobIsNotFutureButPrevHashNotPresent)
                }
            }
            // This should not happen when a non future job is received we always need to have a
            // prev hash
            (false, None) => Err(Error::JobIsNotFutureButPrevHashNotPresent),
        }
    }

    // When a new extended job is received we use this function to prepare the jobs to be sent
    // downstream (standard for hom and this job for non hom)
    fn prepare_jobs_for_downstream_on_new_extended(
        &mut self,
        result: &mut HashMap<u32, Mining, BuildNoHashHasher<u32>>,
        m: &NewExtendedMiningJob<'static>,
    ) -> Result<(), Error> {
        for id in self.extended_channels.keys() {
            let mut extended = m.clone();
            extended.channel_id = *id;
            let extended_job = Mining::NewExtendedMiningJob(extended);
            result.insert(*id, extended_job);
        }
        Ok(())
    }

    // If there is job creator, bitcoin_target is retrieved from there. If not, it is set to 0.
    // If there is a job creator we pass the correct template id. If not, we pass `None`
    // allow comparison chain because clippy wants to make job management assertion into a match
    // clause
    #[allow(clippy::comparison_chain)]
    #[allow(clippy::too_many_arguments)]
    fn check_target<TxHash: std::convert::AsRef<[u8]>>(
        &mut self,
        mut m: Share,
        bitcoin_target: Target,
        template_id: Option<u64>,
        up_id: u32,
        merkle_path: Vec<TxHash>,
        coinbase_tx_prefix: &[u8],
        coinbase_tx_suffix: &[u8],
        prev_blockhash: hash_types::BlockHash,
        bits: u32,
    ) -> Result<OnNewShare, Error> {
        debug!("Checking target for share {:?}", m);
        let upstream_target = match &self.kind {
            ExtendedChannelKind::Pool => Target::new(0, 0),
            ExtendedChannelKind::Proxy {
                upstream_target, ..
            }
            | ExtendedChannelKind::ProxyJd {
                upstream_target, ..
            } => upstream_target.clone(),
        };

        let (downstream_target, extranonce) = self
            .get_channel_specific_mining_info(&m)
            .ok_or(Error::ShareDoNotMatchAnyChannel)?;
        let extranonce_1_len = self.extranonces.get_range0_len();
        let extranonce_2 = extranonce[extranonce_1_len..].to_vec();
        match &mut m {
            Share::Extended(extended_share) => {
                extended_share.extranonce = extranonce_2.try_into()?;
            }
            Share::Standard(_) => (),
        };
        trace!(
            "On checking target coinbase prefix is: {:?}",
            coinbase_tx_prefix
        );
        trace!(
            "On checking target coinbase suffix is: {:?}",
            coinbase_tx_suffix
        );
        // Safe unwrap a sha256 can always be converted into [u8;32]
        let merkle_root: [u8; 32] = crate::utils::merkle_root_from_path(
            coinbase_tx_prefix,
            coinbase_tx_suffix,
            &extranonce[..],
            &merkle_path[..],
        )
        .ok_or(Error::InvalidCoinbase)?
        .try_into()
        .unwrap();
        let version = match &m {
            Share::Extended(share) => share.version as i32,
            Share::Standard(share) => share.0.version as i32,
        };

        let header = Header {
            version: Version::from_consensus(version),
            prev_blockhash,
            merkle_root: (*Hash::from_bytes_ref(&merkle_root)).into(),
            time: m.get_n_time(),
            bits: CompactTarget::from_consensus(bits),
            nonce: m.get_nonce(),
        };

        trace!("On checking target header is: {:?}", header);
        let hash_ = header.block_hash();
        let hash: [u8; 32] = *hash_.to_raw_hash().as_ref();

        if tracing::level_enabled!(tracing::Level::DEBUG)
            || tracing::level_enabled!(tracing::Level::TRACE)
        {
            let bitcoin_target_log: binary_sv2::U256 = bitcoin_target.clone().into();
            let mut bitcoin_target_log = bitcoin_target_log.to_vec();
            bitcoin_target_log.reverse();
            debug!("Bitcoin target : {:?}", bitcoin_target_log.as_hex());
            let upstream_target: binary_sv2::U256 = upstream_target.clone().into();
            let mut upstream_target = upstream_target.to_vec();
            upstream_target.reverse();
            debug!("Upstream target: {:?}", upstream_target.to_vec().as_hex());
            let mut hash = hash;
            hash.reverse();
            debug!("Hash           : {:?}", hash.to_vec().as_hex());
        }
        let hash: Target = hash.into();

        if hash <= bitcoin_target {
            let mut print_hash: [u8; 32] = *hash_.to_raw_hash().as_ref();
            print_hash.reverse();

            info!(
                "Share hash meet bitcoin target: {:?}",
                print_hash.to_vec().as_hex()
            );

            let coinbase = [coinbase_tx_prefix, &extranonce[..], coinbase_tx_suffix]
                .concat()
                .to_vec();
            match self.kind {
                ExtendedChannelKind::Proxy { .. } | ExtendedChannelKind::ProxyJd { .. } => {
                    let upstream_extranonce_space = self.extranonces.get_range0_len();
                    let extranonce_ = extranonce[upstream_extranonce_space..].to_vec();
                    let mut res = OnNewShare::ShareMeetBitcoinTarget((
                        m,
                        template_id,
                        coinbase,
                        extranonce.to_vec(),
                    ));
                    res.into_extended(extranonce_, up_id);
                    Ok(res)
                }
                ExtendedChannelKind::Pool => Ok(OnNewShare::ShareMeetBitcoinTarget((
                    m,
                    template_id,
                    coinbase,
                    extranonce.to_vec(),
                ))),
            }
        } else if hash <= upstream_target {
            match self.kind {
                ExtendedChannelKind::Proxy { .. } | ExtendedChannelKind::ProxyJd { .. } => {
                    let upstream_extranonce_space = self.extranonces.get_range0_len();
                    let extranonce = extranonce[upstream_extranonce_space..].to_vec();
                    let mut res = OnNewShare::SendSubmitShareUpstream((m, template_id));
                    res.into_extended(extranonce, up_id);
                    Ok(res)
                }
                ExtendedChannelKind::Pool => {
                    Ok(OnNewShare::SendSubmitShareUpstream((m, template_id)))
                }
            }
        } else if hash <= downstream_target {
            Ok(OnNewShare::ShareMeetDownstreamTarget)
        } else {
            error!("Share does not meet any target: {:?}", m);
            let error = SubmitSharesError {
                channel_id: m.get_channel_id(),
                sequence_number: m.get_sequence_number(),
                // Infallible unwrap we already know the len of the error code (is a
                // static string)
                error_code: SubmitSharesError::difficulty_too_low_error_code()
                    .to_string()
                    .try_into()
                    .unwrap(),
            };
            Ok(OnNewShare::SendErrorDownstream(error))
        }
    }

    /// Returns the downstream target and extranonce for the channel
    fn get_channel_specific_mining_info(&self, m: &Share) -> Option<(mining_sv2::Target, Vec<u8>)> {
        match m {
            Share::Extended(share) => {
                let channel = self.extended_channels.get(&m.get_channel_id())?;
                let extranonce_prefix = channel.extranonce_prefix.to_vec();
                let dowstream_target = channel.target.clone().into();
                let extranonce = [&extranonce_prefix[..], &share.extranonce.to_vec()[..]]
                    .concat()
                    .to_vec();
                if extranonce.len() != self.extranonces.get_len() {
                    error!(
                        "Extranonce is not of the right len expected {} actual {}",
                        self.extranonces.get_len(),
                        extranonce.len()
                    );
                }
                Some((dowstream_target, extranonce))
            }
            Share::Standard((_share, _group_id)) => {
                unimplemented!()
            }
        }
    }
    /// Updates the downstream target for the given channel_id
    fn update_target_for_channel(&mut self, channel_id: u32, new_target: Target) -> Option<bool> {
        let channel = self.extended_channels.get_mut(&channel_id)?;
        channel.target = new_target.into();
        Some(true)
    }
}

/// Used by a pool to in order to manage all downstream channel. It adds job creation capabilities
/// to ChannelFactory.
#[derive(Debug)]
pub struct PoolChannelFactory {
    inner: ChannelFactory,
    job_creator: JobsCreators,
    pool_coinbase_outputs: Vec<TxOut>,
    // extended_channel_id -> SetCustomMiningJob
    negotiated_jobs: HashMap<u32, SetCustomMiningJob<'static>, BuildNoHashHasher<u32>>,
}

impl PoolChannelFactory {
    /// constructor
    pub fn new(
        ids: Arc<Mutex<GroupId>>,
        extranonces: ExtendedExtranonce,
        job_creator: JobsCreators,
        share_per_min: f32,
        kind: ExtendedChannelKind,
        pool_coinbase_outputs: Vec<TxOut>,
    ) -> Self {
        let inner = ChannelFactory {
            ids,
            extended_channels: HashMap::with_hasher(BuildNoHashHasher::default()),
            extranonces,
            share_per_min,
            future_jobs: Vec::new(),
            last_prev_hash: None,
            last_prev_hash_: None,
            last_valid_job: None,
            kind,
            job_ids: Id::new(),
            channel_to_group_id: HashMap::with_hasher(BuildNoHashHasher::default()),
            future_templates: HashMap::with_hasher(BuildNoHashHasher::default()),
        };

        Self {
            inner,
            job_creator,
            pool_coinbase_outputs,
            negotiated_jobs: HashMap::with_hasher(BuildNoHashHasher::default()),
        }
    }

    /// Calls [`ChannelFactory::new_extended_channel`]
    pub fn new_extended_channel(
        &mut self,
        request_id: u32,
        hash_rate: f32,
        min_extranonce_size: u16,
    ) -> Result<Vec<Mining<'static>>, Error> {
        self.inner
            .new_extended_channel(request_id, hash_rate, min_extranonce_size)
    }

    /// Called when we want to replicate a channel already opened by another actor.
    /// is used only in the jd client from the template provider module to mock a pool.
    /// Anything else should open channel with the new_extended_channel function
    pub fn replicate_upstream_extended_channel_only_jd(
        &mut self,
        target: binary_sv2::U256<'static>,
        extranonce: mining_sv2::Extranonce,
        channel_id: u32,
        extranonce_size: u16,
    ) -> Option<()> {
        self.inner.replicate_upstream_extended_channel_only_jd(
            target,
            extranonce,
            channel_id,
            extranonce_size,
        )
    }

    /// Called only when a new prev hash is received by a Template Provider. It matches the
    /// message with a `job_id` and calls [`ChannelFactory::on_new_prev_hash`]
    /// it return the job_id
    pub fn on_new_prev_hash_from_tp(
        &mut self,
        m: &SetNewPrevHashFromTp<'static>,
    ) -> Result<u32, Error> {
        let job_id = self.job_creator.on_new_prev_hash(m).unwrap_or(0);
        let new_prev_hash = StagedPhash {
            job_id,
            prev_hash: m.prev_hash.clone(),
            min_ntime: m.header_timestamp,
            nbits: m.n_bits,
        };
        self.inner.on_new_prev_hash(new_prev_hash)?;
        Ok(job_id)
    }

    /// Called only when a new template is received by a Template Provider
    pub fn on_new_template(
        &mut self,
        m: &mut NewTemplate<'static>,
    ) -> Result<HashMap<u32, Mining<'static>, BuildNoHashHasher<u32>>, Error> {
        let new_job =
            self.job_creator
                .on_new_template(m, true, self.pool_coinbase_outputs.clone())?;
        self.inner.on_new_extended_mining_job(new_job)
    }

    /// Called when a `SubmitSharesStandard` message is received from the downstream. We check the
    /// shares against the channel's respective target and return `OnNewShare` to let us know if
    /// and where the shares should be relayed
    pub fn on_submit_shares_standard(
        &mut self,
        m: SubmitSharesStandard,
    ) -> Result<OnNewShare, Error> {
        match self.inner.channel_to_group_id.get(&m.channel_id) {
            Some(g_id) => {
                let referenced_job = self
                    .inner
                    .last_valid_job
                    .clone()
                    .ok_or(Error::ShareDoNotMatchAnyJob)?
                    .0;
                let merkle_path = referenced_job.merkle_path.to_vec();
                let template_id = self
                    .job_creator
                    .get_template_id_from_job(referenced_job.job_id)
                    .ok_or(Error::NoTemplateForId)?;
                let target = self.job_creator.last_target();
                let prev_blockhash = self
                    .inner
                    .last_prev_hash_
                    .ok_or(Error::ShareDoNotMatchAnyJob)?;
                let bits = self
                    .inner
                    .last_prev_hash
                    .as_ref()
                    .ok_or(Error::ShareDoNotMatchAnyJob)?
                    .0
                    .nbits;
                self.inner.check_target(
                    Share::Standard((m, *g_id)),
                    target,
                    Some(template_id),
                    0,
                    merkle_path,
                    referenced_job.coinbase_tx_prefix.as_ref(),
                    referenced_job.coinbase_tx_suffix.as_ref(),
                    prev_blockhash,
                    bits,
                )
            }
            None => {
                let err = SubmitSharesError {
                    channel_id: m.channel_id,
                    sequence_number: m.sequence_number,
                    error_code: SubmitSharesError::invalid_channel_error_code()
                        .to_string()
                        .try_into()
                        .unwrap(),
                };
                Ok(OnNewShare::SendErrorDownstream(err))
            }
        }
    }

    /// Called when a `SubmitSharesExtended` message is received from the downstream. We check the
    /// shares against the channel's respective target and return `OnNewShare` to let us know if
    /// and where the shares should be relayed
    pub fn on_submit_shares_extended(
        &mut self,
        m: SubmitSharesExtended,
    ) -> Result<OnNewShare, Error> {
        let target = self.job_creator.last_target();
        // When downstream set a custom mining job we add the job to the negotiated job
        // hashmap, with the extended channel id as a key. Whenever the pool receive a share must
        // first check if the channel have a negotiated job if so we can not retrieve the template
        // via the job creator but we create a new one from the set custom job.
        if self.negotiated_jobs.contains_key(&m.channel_id) {
            let referenced_job = self.negotiated_jobs.get(&m.channel_id).unwrap();
            let merkle_path = referenced_job.merkle_path.to_vec();
            let extended_job = job_creator::extended_job_from_custom_job(
                referenced_job,
                self.inner.extranonces.get_len() as u8,
            )
            .unwrap();
            let prev_blockhash = crate::utils::u256_to_block_hash(referenced_job.prev_hash.clone());
            let bits = referenced_job.nbits;
            self.inner.check_target(
                Share::Extended(m.into_static()),
                target,
                None,
                0,
                merkle_path,
                extended_job.coinbase_tx_prefix.as_ref(),
                extended_job.coinbase_tx_suffix.as_ref(),
                prev_blockhash,
                bits,
            )
        } else {
            let referenced_job = self
                .inner
                .last_valid_job
                .clone()
                .ok_or(Error::ShareDoNotMatchAnyJob)?
                .0;
            let merkle_path = referenced_job.merkle_path.to_vec();
            let template_id = self
                .job_creator
                .get_template_id_from_job(referenced_job.job_id)
                .ok_or(Error::NoTemplateForId)?;
            let prev_blockhash = self
                .inner
                .last_prev_hash_
                .ok_or(Error::ShareDoNotMatchAnyJob)?;
            let bits = self
                .inner
                .last_prev_hash
                .as_ref()
                .ok_or(Error::ShareDoNotMatchAnyJob)?
                .0
                .nbits;
            self.inner.check_target(
                Share::Extended(m.into_static()),
                target,
                Some(template_id),
                0,
                merkle_path,
                referenced_job.coinbase_tx_prefix.as_ref(),
                referenced_job.coinbase_tx_suffix.as_ref(),
                prev_blockhash,
                bits,
            )
        }
    }

    /// Utility function to return a new group id
    pub fn new_group_id(&mut self) -> u32 {
        let new_id = self.inner.ids.safe_lock(|ids| ids.new_group_id()).unwrap();
        new_id
    }

    /// Utility function to return a new standard channel id
    pub fn new_standard_id_for_hom(&mut self) -> u32 {
        let hom_group_id = 0;
        let new_id = self
            .inner
            .ids
            .safe_lock(|ids| ids.new_channel_id(hom_group_id))
            .unwrap();
        new_id
    }

    /// Returns the full extranonce, extranonce1 (static for channel) + extranonce2 (miner nonce
    /// space)
    pub fn extranonce_from_downstream_extranonce(
        &self,
        ext: mining_sv2::Extranonce,
    ) -> Option<mining_sv2::Extranonce> {
        self.inner
            .extranonces
            .extranonce_from_downstream_extranonce(ext)
            .ok()
    }

    /// Called when a new custom mining job arrives
    pub fn on_new_set_custom_mining_job(
        &mut self,
        set_custom_mining_job: SetCustomMiningJob<'static>,
    ) -> SetCustomMiningJobSuccess {
        if self.check_set_custom_mining_job(&set_custom_mining_job) {
            self.negotiated_jobs.insert(
                set_custom_mining_job.channel_id,
                set_custom_mining_job.clone(),
            );
            SetCustomMiningJobSuccess {
                channel_id: set_custom_mining_job.channel_id,
                request_id: set_custom_mining_job.request_id,
                job_id: self.inner.job_ids.next(),
            }
        } else {
            todo!()
        }
    }

    fn check_set_custom_mining_job(
        &self,
        _set_custom_mining_job: &SetCustomMiningJob<'static>,
    ) -> bool {
        true
    }

    /// Get extended channel ids
    pub fn get_extended_channels_ids(&self) -> Vec<u32> {
        self.inner.extended_channels.keys().copied().collect()
    }

    pub fn get_shares_per_minute(&self) -> f32 {
        self.inner.share_per_min
    }

    /// Update coinbase outputs
    pub fn update_pool_outputs(&mut self, outs: Vec<TxOut>) {
        self.pool_coinbase_outputs = outs;
    }

    /// Calls [`ChannelFactory::update_target_for_channel`]
    /// Set a particular downstream channel target.
    pub fn update_target_for_channel(
        &mut self,
        channel_id: u32,
        new_target: Target,
    ) -> Option<bool> {
        self.inner.update_target_for_channel(channel_id, new_target)
    }

    /// Set the target for this channel. This is the upstream target.
    pub fn set_target(&mut self, new_target: &mut Target) {
        self.inner.kind.set_target(new_target);
    }
}

/// Used by proxies that want to open extended channels with upstream. If the proxy has job
/// declaration capabilities, we set the job creator and the coinbase outs.
#[derive(Debug)]
pub struct ProxyExtendedChannelFactory {
    inner: ChannelFactory,
    job_creator: Option<JobsCreators>,
    pool_coinbase_outputs: Option<Vec<TxOut>>,
    // Id assigned to the extended channel by upstream
    extended_channel_id: u32,
}

impl ProxyExtendedChannelFactory {
    /// Constructor
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        ids: Arc<Mutex<GroupId>>,
        extranonces: ExtendedExtranonce,
        job_creator: Option<JobsCreators>,
        share_per_min: f32,
        kind: ExtendedChannelKind,
        pool_coinbase_outputs: Option<Vec<TxOut>>,
        extended_channel_id: u32,
    ) -> Self {
        match &kind {
            ExtendedChannelKind::Proxy { .. } => {
                if job_creator.is_some() {
                    panic!("Channel factory of kind Proxy can not be initialized with a JobCreators");
                };
            },
            ExtendedChannelKind::ProxyJd { .. } => {
                if job_creator.is_none() {
                    panic!("Channel factory of kind ProxyJd must be initialized with a JobCreators");
                };
            }
            ExtendedChannelKind::Pool => panic!("Try to construct an ProxyExtendedChannelFactory with pool kind, kind must be Proxy or ProxyJd"),
        };
        let inner = ChannelFactory {
            ids,
            extended_channels: HashMap::with_hasher(BuildNoHashHasher::default()),
            extranonces,
            share_per_min,
            future_jobs: Vec::new(),
            last_prev_hash: None,
            last_prev_hash_: None,
            last_valid_job: None,
            kind,
            job_ids: Id::new(),
            channel_to_group_id: HashMap::with_hasher(BuildNoHashHasher::default()),
            future_templates: HashMap::with_hasher(BuildNoHashHasher::default()),
        };
        ProxyExtendedChannelFactory {
            inner,
            job_creator,
            pool_coinbase_outputs,
            extended_channel_id,
        }
    }

    /// Calls [`ChannelFactory::new_extended_channel`]
    pub fn new_extended_channel(
        &mut self,
        request_id: u32,
        hash_rate: f32,
        min_extranonce_size: u16,
    ) -> Result<Vec<Mining>, Error> {
        self.inner
            .new_extended_channel(request_id, hash_rate, min_extranonce_size)
    }

    /// Called only when a new prev hash is received by a Template Provider when job declaration is
    /// used. It matches the message with a `job_id`, creates a new custom job, and calls
    /// [`ChannelFactory::on_new_prev_hash`]
    pub fn on_new_prev_hash_from_tp(
        &mut self,
        m: &SetNewPrevHashFromTp<'static>,
    ) -> Result<Option<(PartialSetCustomMiningJob, u32)>, Error> {
        if let Some(job_creator) = self.job_creator.as_mut() {
            let job_id = job_creator.on_new_prev_hash(m).unwrap_or(0);
            let new_prev_hash = StagedPhash {
                job_id,
                prev_hash: m.prev_hash.clone(),
                min_ntime: m.header_timestamp,
                nbits: m.n_bits,
            };
            let mut custom_job = None;
            if let Some(template) = self.inner.future_templates.get(&job_id) {
                custom_job = Some((
                    PartialSetCustomMiningJob {
                        version: template.version,
                        prev_hash: new_prev_hash.prev_hash.clone(),
                        min_ntime: new_prev_hash.min_ntime,
                        nbits: new_prev_hash.nbits,
                        coinbase_tx_version: template.coinbase_tx_version,
                        coinbase_prefix: template.coinbase_prefix.clone(),
                        coinbase_tx_input_n_sequence: template.coinbase_tx_input_sequence,
                        coinbase_tx_value_remaining: template.coinbase_tx_value_remaining,
                        coinbase_tx_outputs: template.coinbase_tx_outputs.clone(),
                        coinbase_tx_locktime: template.coinbase_tx_locktime,
                        merkle_path: template.merkle_path.clone(),
                        future_job: template.future_template,
                    },
                    job_id,
                ));
            }
            self.inner.future_templates = HashMap::with_hasher(BuildNoHashHasher::default());
            self.inner.on_new_prev_hash(new_prev_hash)?;
            Ok(custom_job)
        } else {
            panic!("A channel factory without job creator do not have declaration capabilities")
        }
    }

    /// Called only when a new template is received by a Template Provider when job declaration is
    /// used. It creates a new custom job and calls
    /// [`ChannelFactory::on_new_extended_mining_job`]
    #[allow(clippy::type_complexity)]
    pub fn on_new_template(
        &mut self,
        m: &mut NewTemplate<'static>,
    ) -> Result<
        (
            // downstream job_id -> downstream message (newextjob or newjob)
            HashMap<u32, Mining<'static>, BuildNoHashHasher<u32>>,
            // PartialSetCustomMiningJob to send to the pool
            Option<PartialSetCustomMiningJob>,
            // job_id registered in the channel, the one that SetNewPrevHash refer to (upstsream
            // job id)
            u32,
        ),
        Error,
    > {
        if let (Some(job_creator), Some(pool_coinbase_outputs)) = (
            self.job_creator.as_mut(),
            self.pool_coinbase_outputs.as_mut(),
        ) {
            let new_job = job_creator.on_new_template(m, true, pool_coinbase_outputs.clone())?;
            let id = new_job.job_id;
            if !new_job.is_future() && self.inner.last_prev_hash.is_some() {
                let prev_hash = self.last_prev_hash().unwrap();
                let min_ntime = self.last_min_ntime().unwrap();
                let nbits = self.last_nbits().unwrap();
                let custom_mining_job = PartialSetCustomMiningJob {
                    version: m.version,
                    prev_hash,
                    min_ntime,
                    nbits,
                    coinbase_tx_version: m.coinbase_tx_version,
                    coinbase_prefix: m.coinbase_prefix.clone(),
                    coinbase_tx_input_n_sequence: m.coinbase_tx_input_sequence,
                    coinbase_tx_value_remaining: m.coinbase_tx_value_remaining,
                    coinbase_tx_outputs: m.coinbase_tx_outputs.clone(),
                    coinbase_tx_locktime: m.coinbase_tx_locktime,
                    merkle_path: m.merkle_path.clone(),
                    future_job: m.future_template,
                };
                return Ok((
                    self.inner.on_new_extended_mining_job(new_job)?,
                    Some(custom_mining_job),
                    id,
                ));
            } else if new_job.is_future() {
                self.inner
                    .future_templates
                    .insert(new_job.job_id, m.clone());
            }
            Ok((self.inner.on_new_extended_mining_job(new_job)?, None, id))
        } else {
            panic!("Either channel factory has no job creator or pool_coinbase_outputs are not yet set")
        }
    }

    /// Called when a `SubmitSharesStandard` message is received from the downstream. We check the
    /// shares against the channel's respective target and return `OnNewShare` to let us know if
    /// and where the shares should be relayed
    pub fn on_submit_shares_extended(
        &mut self,
        m: SubmitSharesExtended<'static>,
    ) -> Result<OnNewShare, Error> {
        let merkle_path = self
            .inner
            .last_valid_job
            .as_ref()
            .ok_or(Error::ShareDoNotMatchAnyJob)?
            .0
            .merkle_path
            .to_vec();

        let referenced_job = self
            .inner
            .last_valid_job
            .clone()
            .ok_or(Error::ShareDoNotMatchAnyJob)?
            .0;

        if referenced_job.job_id != m.job_id {
            let error = SubmitSharesError {
                channel_id: m.channel_id,
                sequence_number: m.sequence_number,
                // Infallible unwrap we already know the len of the error code (is a
                // static string)
                error_code: SubmitSharesError::invalid_job_id_error_code()
                    .to_string()
                    .try_into()
                    .unwrap(),
            };
            return Ok(OnNewShare::SendErrorDownstream(error));
        }

        if let Some(job_creator) = self.job_creator.as_mut() {
            let template_id = job_creator
                .get_template_id_from_job(referenced_job.job_id)
                .ok_or(Error::NoTemplateForId)?;
            let bitcoin_target = job_creator.last_target();
            let prev_blockhash = self
                .inner
                .last_prev_hash_
                .ok_or(Error::ShareDoNotMatchAnyJob)?;
            let bits = self
                .inner
                .last_prev_hash
                .as_ref()
                .ok_or(Error::ShareDoNotMatchAnyJob)?
                .0
                .nbits;
            self.inner.check_target(
                Share::Extended(m),
                bitcoin_target,
                Some(template_id),
                self.extended_channel_id,
                merkle_path,
                referenced_job.coinbase_tx_prefix.as_ref(),
                referenced_job.coinbase_tx_suffix.as_ref(),
                prev_blockhash,
                bits,
            )
        } else {
            let bitcoin_target = [0; 32];
            // if there is not job_creator is not proxy duty to check if target is below or above
            // bitcoin target so we set bitcoin_target = 0.
            let prev_blockhash = self
                .inner
                .last_prev_hash_
                .ok_or(Error::ShareDoNotMatchAnyJob)?;
            let bits = self
                .inner
                .last_prev_hash
                .as_ref()
                .ok_or(Error::ShareDoNotMatchAnyJob)?
                .0
                .nbits;
            self.inner.check_target(
                Share::Extended(m),
                bitcoin_target.into(),
                None,
                self.extended_channel_id,
                merkle_path,
                referenced_job.coinbase_tx_prefix.as_ref(),
                referenced_job.coinbase_tx_suffix.as_ref(),
                prev_blockhash,
                bits,
            )
        }
    }

    /// Called when a `SubmitSharesStandard` message is received from the Downstream. We check the
    /// shares against the channel's respective target and return `OnNewShare` to let us know if
    /// and where the shares should be relayed
    pub fn on_submit_shares_standard(
        &mut self,
        m: SubmitSharesStandard,
    ) -> Result<OnNewShare, Error> {
        let merkle_path = self
            .inner
            .last_valid_job
            .as_ref()
            .ok_or(Error::ShareDoNotMatchAnyJob)?
            .0
            .merkle_path
            .to_vec();
        let referenced_job = self
            .inner
            .last_valid_job
            .clone()
            .ok_or(Error::ShareDoNotMatchAnyJob)?
            .0;
        match self.inner.channel_to_group_id.get(&m.channel_id) {
            Some(g_id) => {
                if let Some(job_creator) = self.job_creator.as_mut() {
                    let template_id = job_creator
                        .get_template_id_from_job(
                            self.inner.last_valid_job.as_ref().unwrap().0.job_id,
                        )
                        .ok_or(Error::NoTemplateForId)?;
                    let bitcoin_target = job_creator.last_target();
                    let prev_blockhash = self
                        .inner
                        .last_prev_hash_
                        .ok_or(Error::ShareDoNotMatchAnyJob)?;
                    let bits = self
                        .inner
                        .last_prev_hash
                        .as_ref()
                        .ok_or(Error::ShareDoNotMatchAnyJob)?
                        .0
                        .nbits;
                    self.inner.check_target(
                        Share::Standard((m, *g_id)),
                        bitcoin_target,
                        Some(template_id),
                        self.extended_channel_id,
                        merkle_path,
                        referenced_job.coinbase_tx_prefix.as_ref(),
                        referenced_job.coinbase_tx_suffix.as_ref(),
                        prev_blockhash,
                        bits,
                    )
                } else {
                    let bitcoin_target = [0; 32];
                    let prev_blockhash = self
                        .inner
                        .last_prev_hash_
                        .ok_or(Error::ShareDoNotMatchAnyJob)?;
                    let bits = self
                        .inner
                        .last_prev_hash
                        .as_ref()
                        .ok_or(Error::ShareDoNotMatchAnyJob)?
                        .0
                        .nbits;
                    // if there is not job_creator is not proxy duty to check if target is below or
                    // above bitcoin target so we set bitcoin_target = 0.
                    self.inner.check_target(
                        Share::Standard((m, *g_id)),
                        bitcoin_target.into(),
                        None,
                        self.extended_channel_id,
                        merkle_path,
                        referenced_job.coinbase_tx_prefix.as_ref(),
                        referenced_job.coinbase_tx_suffix.as_ref(),
                        prev_blockhash,
                        bits,
                    )
                }
            }
            None => {
                let err = SubmitSharesError {
                    channel_id: m.channel_id,
                    sequence_number: m.sequence_number,
                    error_code: SubmitSharesError::invalid_channel_error_code()
                        .to_string()
                        .try_into()
                        .unwrap(),
                };
                Ok(OnNewShare::SendErrorDownstream(err))
            }
        }
    }

    /// Calls [`ChannelFactory::on_new_prev_hash`]
    pub fn on_new_prev_hash(&mut self, m: SetNewPrevHash<'static>) -> Result<(), Error> {
        self.inner.on_new_prev_hash(StagedPhash {
            job_id: m.job_id,
            prev_hash: m.prev_hash.clone().into_static(),
            min_ntime: m.min_ntime,
            nbits: m.nbits,
        })
    }

    /// Calls [`ChannelFactory::on_new_extended_mining_job`]
    pub fn on_new_extended_mining_job(
        &mut self,
        m: NewExtendedMiningJob<'static>,
    ) -> Result<HashMap<u32, Mining<'static>, BuildNoHashHasher<u32>>, Error> {
        self.inner.on_new_extended_mining_job(m)
    }

    /// Set new target
    pub fn set_target(&mut self, new_target: &mut Target) {
        self.inner.kind.set_target(new_target);
    }

    /// Get last valid job version
    pub fn last_valid_job_version(&self) -> Option<u32> {
        self.inner.last_valid_job.as_ref().map(|j| j.0.version)
    }

    /// Returns the full extranonce, extranonce1 (static for channel) + extranonce2 (miner nonce
    /// space)
    pub fn extranonce_from_downstream_extranonce(
        &self,
        ext: mining_sv2::Extranonce,
    ) -> Option<mining_sv2::Extranonce> {
        self.inner
            .extranonces
            .extranonce_from_downstream_extranonce(ext)
            .ok()
    }

    /// Returns the most recent prev hash
    pub fn last_prev_hash(&self) -> Option<binary_sv2::U256<'static>> {
        self.inner
            .last_prev_hash
            .as_ref()
            .map(|f| f.0.prev_hash.clone())
    }

    /// Get last min ntime
    pub fn last_min_ntime(&self) -> Option<u32> {
        self.inner.last_prev_hash.as_ref().map(|f| f.0.min_ntime)
    }

    /// Get last nbits
    pub fn last_nbits(&self) -> Option<u32> {
        self.inner.last_prev_hash.as_ref().map(|f| f.0.nbits)
    }

    /// Get extranonce_size
    pub fn extranonce_size(&self) -> usize {
        self.inner.extranonces.get_len()
    }

    /// Get extranonce_2 size
    pub fn channel_extranonce2_size(&self) -> usize {
        self.inner.extranonces.get_len() - self.inner.extranonces.get_range0_len()
    }

    // Only used when the proxy is using Job Declaration
    /// Updates pool outputs
    pub fn update_pool_outputs(&mut self, outs: Vec<TxOut>) {
        self.pool_coinbase_outputs = Some(outs);
    }

    /// Get this channel id
    pub fn get_this_channel_id(&self) -> u32 {
        self.extended_channel_id
    }

    /// Returns the extranonce1 len of the upstream. For a proxy, this would
    /// be the extranonce_prefix len
    pub fn get_upstream_extranonce1_len(&self) -> usize {
        self.inner.extranonces.get_range0_len()
    }

    /// Calls [`ChannelFactory::update_target_for_channel`]
    pub fn update_target_for_channel(
        &mut self,
        channel_id: u32,
        new_target: Target,
    ) -> Option<bool> {
        self.inner.update_target_for_channel(channel_id, new_target)
    }
}

/// Used by proxies for tracking upstream targets.
#[derive(Debug, Clone)]
pub enum ExtendedChannelKind {
    Proxy { upstream_target: Target },
    ProxyJd { upstream_target: Target },
    Pool,
}
impl ExtendedChannelKind {
    /// Set target
    pub fn set_target(&mut self, new_target: &mut Target) {
        match self {
            ExtendedChannelKind::Proxy { upstream_target }
            | ExtendedChannelKind::ProxyJd { upstream_target } => {
                std::mem::swap(upstream_target, new_target)
            }
            ExtendedChannelKind::Pool => warn!("Try to set upstream target for a pool"),
        }
    }
}
