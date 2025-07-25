// Copyright 2017 TiKV Project Authors. Licensed under Apache-2.0.
use std::{
    borrow::Cow,
    cmp::{self, Ordering as CmpOrdering, Reverse},
    error::Error as StdError,
    fmt::{self, Display, Formatter},
    io::{self, ErrorKind, Read, Write},
    path::{Path, PathBuf},
    result, str,
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        Arc, Mutex, RwLock,
    },
    thread, time, u64,
};

use collections::{HashMap, HashMapEntry as Entry};
use encryption::{create_aes_ctr_crypter, from_engine_encryption_method, DataKeyManager, Iv};
use engine_traits::{CfName, EncryptionKeyManager, KvEngine, CF_DEFAULT, CF_LOCK, CF_WRITE};
use error_code::{self, ErrorCode, ErrorCodeExt};
use fail::fail_point;
use file_system::{
    calc_crc32, calc_crc32_and_size, delete_dir_if_exist, delete_file_if_exist, file_exists,
    get_file_size, sync_dir, File, Metadata, OpenOptions,
};
use keys::{enc_end_key, enc_start_key};
use kvproto::{
    encryptionpb::EncryptionMethod,
    metapb::Region,
    pdpb::SnapshotStat,
    raft_serverpb::{RaftSnapshotData, SnapshotCfFile, SnapshotMeta},
};
use openssl::symm::{Cipher, Crypter, Mode};
use protobuf::Message;
use raft::eraftpb::Snapshot as RaftSnapshot;
use thiserror::Error;
use tikv_util::{
    box_err, box_try,
    config::ReadableSize,
    debug, error, info,
    time::{duration_to_sec, Instant, Limiter, UnixSecs},
    warn, HandyRwLock,
};

use crate::{
    coprocessor::CoprocessorHost,
    store::{metrics::*, peer_storage::JOB_STATUS_CANCELLING},
    Error as RaftStoreError, Result as RaftStoreResult,
};

#[path = "snap/io.rs"]
pub mod snap_io;

// Data in CF_RAFT should be excluded for a snapshot.
pub const SNAPSHOT_CFS: &[CfName] = &[CF_DEFAULT, CF_LOCK, CF_WRITE];
pub const SNAPSHOT_CFS_ENUM_PAIR: &[(CfNames, CfName)] = &[
    (CfNames::default, CF_DEFAULT),
    (CfNames::lock, CF_LOCK),
    (CfNames::write, CF_WRITE),
];
pub const SNAPSHOT_VERSION: u64 = 2;
pub const TABLET_SNAPSHOT_VERSION: u64 = 3;
pub const IO_LIMITER_CHUNK_SIZE: usize = 4 * 1024;

/// Name prefix for the self-generated snapshot file.
const SNAP_GEN_PREFIX: &str = "gen";
/// Name prefix for the received snapshot file.
const SNAP_REV_PREFIX: &str = "rev";
const DEL_RANGE_PREFIX: &str = "del_range";

const TMP_FILE_SUFFIX: &str = ".tmp";
const SST_FILE_SUFFIX: &str = ".sst";
const CLONE_FILE_SUFFIX: &str = ".clone";
const META_FILE_SUFFIX: &str = ".meta";

const DELETE_RETRY_MAX_TIMES: u32 = 6;
const DELETE_RETRY_TIME_MILLIS: u64 = 500;

#[derive(Debug, Error)]
pub enum Error {
    #[error("abort")]
    Abort,

    #[error("too many snapshots")]
    TooManySnapshots,

    #[error("snap failed {0:?}")]
    Other(#[from] Box<dyn StdError + Sync + Send>),
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Other(Box::new(e))
    }
}

impl From<engine_traits::Error> for Error {
    fn from(e: engine_traits::Error) -> Self {
        Error::Other(Box::new(e))
    }
}

pub type Result<T> = result::Result<T, Error>;

impl ErrorCodeExt for Error {
    fn error_code(&self) -> ErrorCode {
        match self {
            Error::Abort => error_code::raftstore::SNAP_ABORT,
            Error::TooManySnapshots => error_code::raftstore::SNAP_TOO_MANY,
            Error::Other(_) => error_code::raftstore::SNAP_UNKNOWN,
        }
    }
}

// CF_LOCK is relatively small, so we use plain file for performance issue.
#[inline]
pub fn plain_file_used(cf: &str) -> bool {
    cf == CF_LOCK
}

#[inline]
pub fn check_abort(status: &AtomicUsize) -> Result<()> {
    if status.load(Ordering::Relaxed) == JOB_STATUS_CANCELLING {
        return Err(Error::Abort);
    }
    Ok(())
}

#[derive(Clone, Hash, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct SnapKey {
    pub region_id: u64,
    pub term: u64,
    pub idx: u64,
}

impl SnapKey {
    #[inline]
    pub fn new(region_id: u64, term: u64, idx: u64) -> SnapKey {
        SnapKey {
            region_id,
            term,
            idx,
        }
    }

    pub fn from_region_snap(region_id: u64, snap: &RaftSnapshot) -> SnapKey {
        let index = snap.get_metadata().get_index();
        let term = snap.get_metadata().get_term();
        SnapKey::new(region_id, term, index)
    }

    pub fn from_snap(snap: &RaftSnapshot) -> io::Result<SnapKey> {
        let mut snap_data = RaftSnapshotData::default();
        if let Err(e) = snap_data.merge_from_bytes(snap.get_data()) {
            return Err(io::Error::new(ErrorKind::Other, e));
        }
        Ok(SnapKey::from_region_snap(
            snap_data.get_region().get_id(),
            snap,
        ))
    }
}

impl Display for SnapKey {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{}_{}_{}", self.region_id, self.term, self.idx)
    }
}

#[derive(Default)]
pub struct SnapshotStatistics {
    pub size: u64,
    pub kv_count: usize,
}

impl SnapshotStatistics {
    pub fn new() -> SnapshotStatistics {
        SnapshotStatistics {
            ..Default::default()
        }
    }
}

pub struct ApplyOptions<EK>
where
    EK: KvEngine,
{
    pub db: EK,
    pub region: Region,
    pub abort: Arc<AtomicUsize>,
    pub write_batch_size: usize,
    pub coprocessor_host: CoprocessorHost<EK>,
    pub ingest_copy_symlink: bool,
}

// A helper function to copy snapshot.
// Only used in tests.
pub fn copy_snapshot(mut from: Box<Snapshot>, mut to: Box<Snapshot>) -> io::Result<()> {
    if !to.exists() {
        io::copy(&mut from, &mut to)?;
        to.save()?;
    }
    Ok(())
}

// Try to delete the specified snapshot, return true if the deletion is done.
fn retry_delete_snapshot(mgr: &SnapManagerCore, key: &SnapKey, snap: &Snapshot) -> bool {
    let d = time::Duration::from_millis(DELETE_RETRY_TIME_MILLIS);
    for _ in 0..DELETE_RETRY_MAX_TIMES {
        if mgr.delete_snapshot(key, snap, true) {
            return true;
        }
        thread::sleep(d);
    }
    false
}

// Create a SnapshotMeta that can be later put into RaftSnapshotData or written
// into file.
pub fn gen_snapshot_meta(cf_files: &[CfFile], for_balance: bool) -> RaftStoreResult<SnapshotMeta> {
    let mut meta = Vec::with_capacity(cf_files.len());
    for cf_file in cf_files {
        if !SNAPSHOT_CFS.iter().any(|cf| cf_file.cf == *cf) {
            return Err(box_err!(
                "failed to encode invalid snapshot cf {}",
                cf_file.cf
            ));
        }
        let size_vec = &cf_file.size;
        if !size_vec.is_empty() {
            for (i, size) in size_vec.iter().enumerate() {
                let mut cf_file_meta = SnapshotCfFile::new();
                cf_file_meta.set_cf(cf_file.cf.to_string());
                cf_file_meta.set_size(*size);
                cf_file_meta.set_checksum(cf_file.checksum[i]);
                meta.push(cf_file_meta);
            }
        } else {
            let mut cf_file_meta = SnapshotCfFile::new();
            cf_file_meta.set_cf(cf_file.cf.to_string());
            cf_file_meta.set_size(0);
            cf_file_meta.set_checksum(0);
            meta.push(cf_file_meta);
        }
    }
    let mut snapshot_meta = SnapshotMeta::default();
    snapshot_meta.set_cf_files(meta.into());
    snapshot_meta.set_for_balance(for_balance);
    Ok(snapshot_meta)
}

fn calc_checksum_and_size(
    path: &Path,
    encryption_key_manager: Option<&Arc<DataKeyManager>>,
) -> RaftStoreResult<(u32, u64)> {
    let (checksum, size) = if let Some(mgr) = encryption_key_manager {
        // Crc32 and file size need to be calculated based on decrypted contents.
        let file_name = path.to_str().unwrap();
        let mut r = snap_io::get_decrypter_reader(file_name, mgr)?;
        calc_crc32_and_size(&mut r)?
    } else {
        (calc_crc32(path)?, get_file_size(path)?)
    };
    Ok((checksum, size))
}

fn check_file_size(got_size: u64, expected_size: u64, path: &Path) -> RaftStoreResult<()> {
    if got_size != expected_size {
        return Err(box_err!(
            "invalid size {} for snapshot cf file {}, expected {}",
            got_size,
            path.display(),
            expected_size
        ));
    }
    Ok(())
}

fn check_file_checksum(
    got_checksum: u32,
    expected_checksum: u32,
    path: &Path,
) -> RaftStoreResult<()> {
    if got_checksum != expected_checksum {
        return Err(box_err!(
            "invalid checksum {} for snapshot cf file {}, expected {}",
            got_checksum,
            path.display(),
            expected_checksum
        ));
    }
    Ok(())
}

fn check_file_size_and_checksum(
    path: &Path,
    expected_size: u64,
    expected_checksum: u32,
    encryption_key_manager: Option<&Arc<DataKeyManager>>,
) -> RaftStoreResult<()> {
    let (checksum, size) = calc_checksum_and_size(path, encryption_key_manager)?;
    check_file_size(size, expected_size, path)?;
    check_file_checksum(checksum, expected_checksum, path)?;
    Ok(())
}

struct CfFileForRecving {
    file: File,
    encrypter: Option<(Cipher, Crypter)>,
    written_size: u64,
    write_digest: crc32fast::Hasher,
}

#[derive(Default)]
pub struct CfFile {
    pub cf: CfName,
    pub path: PathBuf,
    pub file_prefix: String,
    pub file_suffix: String,
    file_for_sending: Vec<Box<dyn Read + Send>>,
    file_for_recving: Vec<CfFileForRecving>,
    file_names: Vec<String>,
    pub kv_count: u64,
    pub size: Vec<u64>,
    pub checksum: Vec<u32>,
}

impl CfFile {
    pub fn new(cf: CfName, path: PathBuf, file_prefix: String, file_suffix: String) -> Self {
        CfFile {
            cf,
            path,
            file_prefix,
            file_suffix,
            ..Default::default()
        }
    }
    pub fn tmp_file_paths(&self) -> Vec<String> {
        self.file_names
            .iter()
            .map(|file_name| {
                self.path
                    .join(format!("{}{}", file_name, TMP_FILE_SUFFIX))
                    .to_str()
                    .unwrap()
                    .to_string()
            })
            .collect::<Vec<String>>()
    }

    pub fn clone_file_paths(&self) -> Vec<String> {
        self.file_names
            .iter()
            .map(|file_name| {
                self.path
                    .join(format!("{}{}", file_name, CLONE_FILE_SUFFIX))
                    .to_str()
                    .unwrap()
                    .to_string()
            })
            .collect::<Vec<String>>()
    }

    pub fn file_paths(&self) -> Vec<String> {
        self.file_names
            .iter()
            .map(|file_name| self.path.join(file_name).to_str().unwrap().to_string())
            .collect::<Vec<String>>()
    }

    pub fn add_file(&mut self, idx: usize) -> String {
        self.add_file_with_size_checksum(idx, 0, 0)
    }

    pub fn add_file_with_size_checksum(&mut self, idx: usize, size: u64, checksum: u32) -> String {
        assert!(self.size.len() >= idx);
        let file_name = self.gen_file_name(idx);
        if self.size.len() > idx {
            // Any logic similar to test_snap_corruption_on_size_or_checksum will trigger
            // this branch
            self.size[idx] = size;
            self.checksum[idx] = checksum;
            self.file_names[idx] = file_name.clone();
        } else {
            self.size.push(size);
            self.checksum.push(checksum);
            self.file_names.push(file_name.clone());
        }
        self.path.join(file_name).to_str().unwrap().to_string()
    }

    pub fn gen_file_name(&self, file_id: usize) -> String {
        if file_id == 0 {
            // for backward compatibility
            format!("{}{}", self.file_prefix, self.file_suffix)
        } else {
            format!("{}_{:04}{}", self.file_prefix, file_id, self.file_suffix)
        }
    }

    pub fn gen_clone_file_name(&self, file_id: usize) -> String {
        if file_id == 0 {
            // for backward compatibility
            format!(
                "{}{}{}",
                self.file_prefix, self.file_suffix, CLONE_FILE_SUFFIX
            )
        } else {
            format!(
                "{}_{:04}{}{}",
                self.file_prefix, file_id, self.file_suffix, CLONE_FILE_SUFFIX
            )
        }
    }

    pub fn gen_tmp_file_name(&self, file_id: usize) -> String {
        if file_id == 0 {
            // for backward compatibility
            format!(
                "{}{}{}",
                self.file_prefix, self.file_suffix, TMP_FILE_SUFFIX
            )
        } else {
            format!(
                "{}_{:04}{}{}",
                self.file_prefix, file_id, self.file_suffix, TMP_FILE_SUFFIX
            )
        }
    }
}

#[derive(Default)]
struct MetaFile {
    pub meta: Option<SnapshotMeta>,
    pub path: PathBuf,
    pub file: Option<File>,

    // for writing snapshot
    pub tmp_path: PathBuf,
}

pub struct Snapshot {
    key: SnapKey,
    display_path: String,
    dir_path: PathBuf,
    cf_files: Vec<CfFile>,
    cf_index: usize,
    cf_file_index: usize,
    meta_file: MetaFile,
    hold_tmp_files: bool,

    mgr: SnapManagerCore,
}

#[derive(PartialEq, Clone, Copy)]
enum CheckPolicy {
    ErrAllowed,
    ErrNotAllowed,
    None,
}

impl Snapshot {
    fn new<T: Into<PathBuf>>(
        dir: T,
        key: &SnapKey,
        is_sending: bool,
        check_policy: CheckPolicy,
        mgr: &SnapManagerCore,
    ) -> RaftStoreResult<Self> {
        let dir_path = dir.into();
        if !dir_path.exists() {
            file_system::create_dir_all(dir_path.as_path())?;
        }
        let snap_prefix = if is_sending {
            SNAP_GEN_PREFIX
        } else {
            SNAP_REV_PREFIX
        };
        let prefix = format!("{}_{}", snap_prefix, key);
        let display_path = Self::get_display_path(&dir_path, &prefix);

        let mut cf_files = Vec::with_capacity(SNAPSHOT_CFS.len());
        for cf in SNAPSHOT_CFS {
            let file_prefix = format!("{}_{}", prefix, cf);
            let cf_file = CfFile {
                cf,
                path: dir_path.clone(),
                file_prefix,
                file_suffix: SST_FILE_SUFFIX.to_string(),
                ..Default::default()
            };
            cf_files.push(cf_file);
        }

        let meta_filename = format!("{}{}", prefix, META_FILE_SUFFIX);
        let meta_path = dir_path.join(&meta_filename);
        let meta_tmp_path = dir_path.join(format!("{}{}", meta_filename, TMP_FILE_SUFFIX));
        let meta_file = MetaFile {
            path: meta_path,
            tmp_path: meta_tmp_path,
            ..Default::default()
        };

        let mut s = Snapshot {
            key: key.clone(),
            display_path,
            dir_path,
            cf_files,
            cf_index: 0,
            cf_file_index: 0,
            meta_file,
            hold_tmp_files: false,
            mgr: mgr.clone(),
        };

        if check_policy == CheckPolicy::None {
            return Ok(s);
        }

        // load snapshot meta if meta_file exists
        if file_exists(&s.meta_file.path) {
            if let Err(e) = s.load_snapshot_meta() {
                if check_policy == CheckPolicy::ErrNotAllowed {
                    return Err(e);
                }
                warn!(
                    "failed to load existent snapshot meta when try to build snapshot";
                    "snapshot" => %s.path(),
                    "err" => ?e,
                    "error_code" => %e.error_code(),
                );
                if !retry_delete_snapshot(mgr, key, &s) {
                    warn!(
                        "failed to delete snapshot because it's already registered elsewhere";
                        "snapshot" => %s.path(),
                    );
                    return Err(e);
                }
            }
        }
        Ok(s)
    }

    fn new_for_building<T: Into<PathBuf>>(
        dir: T,
        key: &SnapKey,
        mgr: &SnapManagerCore,
    ) -> RaftStoreResult<Self> {
        let mut s = Self::new(dir, key, true, CheckPolicy::ErrAllowed, mgr)?;
        s.init_for_building()?;
        Ok(s)
    }

    fn new_for_sending<T: Into<PathBuf>>(
        dir: T,
        key: &SnapKey,
        mgr: &SnapManagerCore,
    ) -> RaftStoreResult<Self> {
        let mut s = Self::new(dir, key, true, CheckPolicy::ErrNotAllowed, mgr)?;
        s.mgr.limiter = Limiter::new(f64::INFINITY);

        if !s.exists() {
            // Skip the initialization below if it doesn't exists.
            return Ok(s);
        }
        for cf_file in &mut s.cf_files {
            // initialize cf file size and reader
            let file_paths = cf_file.file_paths();
            for (i, file_path) in file_paths.iter().enumerate() {
                if cf_file.size[i] > 0 {
                    let path = Path::new(file_path);
                    let file = File::open(path)?;
                    cf_file
                        .file_for_sending
                        .push(Box::new(file) as Box<dyn Read + Send>);
                }
            }
        }
        Ok(s)
    }

    fn new_for_receiving<T: Into<PathBuf>>(
        dir: T,
        key: &SnapKey,
        mgr: &SnapManagerCore,
        snapshot_meta: SnapshotMeta,
    ) -> RaftStoreResult<Self> {
        let mut s = Self::new(dir, key, false, CheckPolicy::ErrNotAllowed, mgr)?;
        s.set_snapshot_meta(snapshot_meta)?;
        if s.exists() {
            return Ok(s);
        }

        let f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&s.meta_file.tmp_path)?;
        s.meta_file.file = Some(f);
        s.hold_tmp_files = true;

        for cf_file in &mut s.cf_files {
            if cf_file.size.is_empty() {
                continue;
            }
            let tmp_file_paths = cf_file.tmp_file_paths();
            let file_paths = cf_file.file_paths();
            for (idx, _) in tmp_file_paths.iter().enumerate() {
                if cf_file.size[idx] == 0 {
                    continue;
                }
                let file_path = Path::new(&tmp_file_paths[idx]);
                let f = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(file_path)?;
                cf_file.file_for_recving.push(CfFileForRecving {
                    file: f,
                    encrypter: None,
                    written_size: 0,
                    write_digest: crc32fast::Hasher::new(),
                });

                if let Some(mgr) = &s.mgr.encryption_key_manager {
                    let enc_info = mgr.new_file(&file_paths[idx])?;
                    let mthd = from_engine_encryption_method(enc_info.method);
                    if mthd != EncryptionMethod::Plaintext {
                        let file_for_recving = cf_file.file_for_recving.last_mut().unwrap();
                        file_for_recving.encrypter = Some(
                            create_aes_ctr_crypter(
                                mthd,
                                &enc_info.key,
                                Mode::Encrypt,
                                Iv::from_slice(&enc_info.iv)?,
                            )
                            .map_err(|e| RaftStoreError::Snapshot(box_err!(e)))?,
                        );
                    }
                }
            }
        }
        Ok(s)
    }

    fn new_for_applying<T: Into<PathBuf>>(
        dir: T,
        key: &SnapKey,
        mgr: &SnapManagerCore,
    ) -> RaftStoreResult<Self> {
        let mut s = Self::new(dir, key, false, CheckPolicy::ErrNotAllowed, mgr)?;
        s.mgr.limiter = Limiter::new(f64::INFINITY);
        Ok(s)
    }

    // If all files of the snapshot exist, return `Ok` directly. Otherwise create a
    // new file at the temporary meta file path, so that all other try will fail.
    fn init_for_building(&mut self) -> RaftStoreResult<()> {
        if self.exists() {
            return Ok(());
        }
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&self.meta_file.tmp_path)?;
        self.meta_file.file = Some(file);
        self.hold_tmp_files = true;
        Ok(())
    }

    fn read_snapshot_meta(&mut self) -> RaftStoreResult<SnapshotMeta> {
        let buf = file_system::read(&self.meta_file.path)?;
        let mut snapshot_meta = SnapshotMeta::default();
        snapshot_meta.merge_from_bytes(&buf)?;
        Ok(snapshot_meta)
    }

    // Validate and set SnapshotMeta of this Snapshot.
    pub fn set_snapshot_meta(&mut self, snapshot_meta: SnapshotMeta) -> RaftStoreResult<()> {
        let mut cf_file_count_from_meta: Vec<usize> = vec![];
        let mut file_count = 0;
        let mut current_cf = "";
        info!(
            "set_snapshot_meta total cf files count: {}",
            snapshot_meta.get_cf_files().len()
        );
        for cf_file in snapshot_meta.get_cf_files() {
            if current_cf.is_empty() {
                current_cf = cf_file.get_cf();
                file_count = 1;
                continue;
            }

            if current_cf != cf_file.get_cf() {
                cf_file_count_from_meta.push(file_count);
                current_cf = cf_file.get_cf();
                file_count = 1;
            } else {
                file_count += 1;
            }
        }
        cf_file_count_from_meta.push(file_count);

        if cf_file_count_from_meta.len() != self.cf_files.len() {
            return Err(box_err!(
                "invalid cf number of snapshot meta, expect {}, got {}",
                SNAPSHOT_CFS.len(),
                cf_file_count_from_meta.len()
            ));
        }
        let mut file_idx = 0;
        let mut cf_idx = 0;
        for meta in snapshot_meta.get_cf_files() {
            if cf_idx < cf_file_count_from_meta.len() && file_idx < cf_file_count_from_meta[cf_idx]
            {
                if meta.get_cf() != self.cf_files[cf_idx].cf {
                    return Err(box_err!(
                        "invalid {} cf in snapshot meta, expect {}, got {}",
                        cf_idx,
                        self.cf_files[cf_idx].cf,
                        meta.get_cf()
                    ));
                }
                if meta.get_size() != 0 {
                    let _ = self.cf_files[cf_idx].add_file_with_size_checksum(
                        file_idx,
                        meta.get_size(),
                        meta.get_checksum(),
                    );
                }
                file_idx += 1;
                if file_idx >= cf_file_count_from_meta[cf_idx] {
                    cf_idx += 1;
                    file_idx = 0;
                }
            }
        }
        self.meta_file.meta = Some(snapshot_meta);
        Ok(())
    }

    fn load_snapshot_meta(&mut self) -> RaftStoreResult<()> {
        let snapshot_meta = self.read_snapshot_meta()?;
        self.set_snapshot_meta(snapshot_meta)?;
        // check if there is a data corruption when the meta file exists
        // but cf files are deleted.
        if !self.exists() {
            return Err(box_err!(
                "snapshot {} is corrupted, some cf file is missing",
                self.path()
            ));
        }
        Ok(())
    }

    pub fn load_snapshot_meta_if_necessary(&mut self) -> RaftStoreResult<()> {
        if self.meta_file.meta.is_none() && file_exists(&self.meta_file.path) {
            return self.load_snapshot_meta();
        }
        Ok(())
    }

    fn get_display_path(dir_path: impl AsRef<Path>, prefix: &str) -> String {
        let cf_names = "(".to_owned() + SNAPSHOT_CFS.join("|").as_str() + ")";
        format!(
            "{}/{}_{}{}",
            dir_path.as_ref().display(),
            prefix,
            cf_names,
            SST_FILE_SUFFIX
        )
    }

    fn validate<F>(&self, post_check: F) -> RaftStoreResult<()>
    where
        F: Fn(&CfFile, usize) -> RaftStoreResult<()>,
    {
        for cf_file in &self.cf_files {
            let file_paths = cf_file.file_paths();
            for i in 0..file_paths.len() {
                if cf_file.size[i] == 0 {
                    // Skip empty file. The checksum of this cf file should be 0 and
                    // this is checked when loading the snapshot meta.
                    continue;
                }

                check_file_size_and_checksum(
                    Path::new(&file_paths[i]),
                    cf_file.size[i],
                    cf_file.checksum[i],
                    self.mgr.encryption_key_manager.as_ref(),
                )?;
                post_check(cf_file, i)?;
            }
        }
        Ok(())
    }

    fn switch_to_cf_file(&mut self, cf: &str) -> io::Result<()> {
        match self.cf_files.iter().position(|x| x.cf == cf) {
            Some(index) => {
                self.cf_index = index;
                Ok(())
            }
            None => Err(io::Error::new(
                ErrorKind::Other,
                format!("fail to find cf {}", cf),
            )),
        }
    }

    // Save `SnapshotMeta` to file.
    // Used in `do_build` and by external crates.
    pub fn save_meta_file(&mut self) -> RaftStoreResult<()> {
        let v = box_try!(self.meta_file.meta.as_ref().unwrap().write_to_bytes());
        if let Some(mut f) = self.meta_file.file.take() {
            // `meta_file` could be None for this case: in `init_for_building` the snapshot
            // exists so no temporary meta file is created, and this field is
            // None. However in `do_build` it's deleted so we build it again,
            // and then call `save_meta_file` with `meta_file` as None.
            // FIXME: We can fix it later by introducing a better snapshot delete mechanism.
            f.write_all(&v[..])?;
            f.flush()?;
            f.sync_all()?;
            file_system::rename(&self.meta_file.tmp_path, &self.meta_file.path)?;
            self.hold_tmp_files = false;
            Ok(())
        } else {
            Err(box_err!(
                "save meta file without metadata for {:?}",
                self.key
            ))
        }
    }

    fn do_build<EK: KvEngine>(
        &mut self,
        engine: &EK,
        kv_snap: &EK::Snapshot,
        region: &Region,
        allow_multi_files_snapshot: bool,
        for_balance: bool,
    ) -> RaftStoreResult<()>
    where
        EK: KvEngine,
    {
        fail_point!("snapshot_enter_do_build");
        if self.exists() {
            match self.validate(|_, _| -> RaftStoreResult<()> { Ok(()) }) {
                Ok(()) => return Ok(()),
                Err(e) => {
                    error!(?e;
                        "snapshot is corrupted, will rebuild";
                        "region_id" => region.get_id(),
                        "snapshot" => %self.path(),
                    );
                    if !retry_delete_snapshot(&self.mgr, &self.key, self) {
                        error!(
                            "failed to delete corrupted snapshot because it's \
                             already registered elsewhere";
                            "region_id" => region.get_id(),
                            "snapshot" => %self.path(),
                        );
                        return Err(e);
                    }
                    self.init_for_building()?;
                }
            }
        }

        let (begin_key, end_key) = (enc_start_key(region), enc_end_key(region));
        for (cf_enum, cf) in SNAPSHOT_CFS_ENUM_PAIR {
            self.switch_to_cf_file(cf)?;
            let cf_file = &mut self.cf_files[self.cf_index];
            let cf_stat = if plain_file_used(cf_file.cf) {
                snap_io::build_plain_cf_file::<EK>(
                    cf_file,
                    self.mgr.encryption_key_manager.as_ref(),
                    kv_snap,
                    &begin_key,
                    &end_key,
                )?
            } else {
                snap_io::build_sst_cf_file_list::<EK>(
                    cf_file,
                    engine,
                    kv_snap,
                    &begin_key,
                    &end_key,
                    self.mgr
                        .get_actual_max_per_file_size(allow_multi_files_snapshot),
                    &self.mgr.limiter,
                    self.mgr.encryption_key_manager.clone(),
                )?
            };
            SNAPSHOT_LIMIT_GENERATE_BYTES.inc_by(cf_stat.total_size as u64);
            cf_file.kv_count = cf_stat.key_count as u64;
            if cf_file.kv_count > 0 {
                // Use `kv_count` instead of file size to check empty files because encrypted
                // sst files contain some metadata so their sizes will never be 0.
                self.mgr.rename_tmp_cf_file_for_send(cf_file)?;
            } else {
                for tmp_file_path in cf_file.tmp_file_paths() {
                    let tmp_file_path = Path::new(&tmp_file_path);
                    delete_file_if_exist(tmp_file_path)?;
                }
                if let Some(ref mgr) = self.mgr.encryption_key_manager {
                    for tmp_file_path in cf_file.tmp_file_paths() {
                        mgr.delete_file(&tmp_file_path, None)?;
                    }
                }
            }

            SNAPSHOT_CF_KV_COUNT
                .get(*cf_enum)
                .observe(cf_stat.key_count as f64);
            SNAPSHOT_CF_SIZE
                .get(*cf_enum)
                .observe(cf_stat.total_size as f64);
            info!(
                "scan snapshot of one cf";
                "region_id" => region.get_id(),
                "snapshot" => self.path(),
                "cf" => cf,
                "key_count" => cf_stat.key_count,
                "size" => cf_stat.total_size,
            );
        }

        // save snapshot meta to meta file
        self.meta_file.meta = Some(gen_snapshot_meta(&self.cf_files[..], for_balance)?);
        self.save_meta_file()?;
        Ok(())
    }

    fn delete(&self) {
        macro_rules! try_delete_snapshot_files {
            ($cf_file:ident, $file_name_func:ident) => {
                let mut file_id = 0;
                loop {
                    let file_path = $cf_file.path.join($cf_file.$file_name_func(file_id));
                    if file_exists(&file_path) {
                        delete_file_if_exist(&file_path).unwrap();
                        file_id += 1;
                    } else {
                        break;
                    }
                }
            };
            ($cf_file:ident) => {
                let mut file_id = 0;
                loop {
                    let file_path = $cf_file.path.join($cf_file.gen_file_name(file_id));
                    if file_exists(&file_path) {
                        delete_file_if_exist(&file_path).unwrap();
                        if let Some(ref mgr) = self.mgr.encryption_key_manager {
                            mgr.delete_file(file_path.to_str().unwrap(), None).unwrap();
                        }
                        file_id += 1;
                    } else {
                        break;
                    }
                }
            };
        }

        debug!(
            "deleting snapshot file";
            "snapshot" => %self.path(),
        );
        for cf_file in &self.cf_files {
            // Delete cloned files.
            let clone_file_paths = cf_file.clone_file_paths();
            // in case the meta file is corrupted or deleted, delete snapshot files with
            // best effort
            if clone_file_paths.is_empty() {
                try_delete_snapshot_files!(cf_file, gen_clone_file_name);
            } else {
                // delete snapshot files according to meta file
                for clone_file_path in clone_file_paths {
                    delete_file_if_exist(clone_file_path).unwrap();
                }
            }

            // Delete temp files.
            if self.hold_tmp_files {
                let tmp_file_paths = cf_file.tmp_file_paths();
                if tmp_file_paths.is_empty() {
                    try_delete_snapshot_files!(cf_file, gen_tmp_file_name);
                } else {
                    for tmp_file_path in tmp_file_paths {
                        delete_file_if_exist(tmp_file_path).unwrap();
                    }
                }
            }

            // Delete cf files.
            let file_paths = cf_file.file_paths();
            if file_paths.is_empty() {
                try_delete_snapshot_files!(cf_file);
            } else {
                for file_path in &file_paths {
                    delete_file_if_exist(file_path).unwrap();
                }
                if let Some(ref mgr) = self.mgr.encryption_key_manager {
                    for file_path in &file_paths {
                        mgr.delete_file(file_path, None).unwrap();
                    }
                }
            }
        }
        if let Some(ref meta) = self.meta_file.meta {
            if !meta.tablet_snap_path.is_empty() {
                delete_dir_if_exist(&meta.tablet_snap_path).unwrap();
            }
        }
        delete_file_if_exist(&self.meta_file.path).unwrap();
        if self.hold_tmp_files {
            delete_file_if_exist(&self.meta_file.tmp_path).unwrap();
        }
    }

    // This is only used for v2 compatibility.
    fn new_for_tablet_snapshot<T: Into<PathBuf>>(
        dir: T,
        key: &SnapKey,
        mgr: &SnapManagerCore,
        tablet_snapshot_path: &str,
        for_balance: bool,
    ) -> RaftStoreResult<Self> {
        let mut s = Self::new(dir, key, false, CheckPolicy::ErrNotAllowed, mgr)?;
        s.init_for_building()?;
        let mut meta = gen_snapshot_meta(&s.cf_files[..], for_balance)?;
        meta.tablet_snap_path = tablet_snapshot_path.to_string();
        s.meta_file.meta = Some(meta);
        s.save_meta_file()?;
        Ok(s)
    }

    #[cfg(any(test, feature = "testexport"))]
    pub fn tablet_snap_path(&self) -> Option<String> {
        Some(self.meta_file.meta.as_ref()?.tablet_snap_path.clone())
    }

    pub fn snapshot_meta(&self) -> &Option<SnapshotMeta> {
        &self.meta_file.meta
    }
}

impl fmt::Debug for Snapshot {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("Snapshot")
            .field("key", &self.key)
            .field("display_path", &self.display_path)
            .finish()
    }
}

impl Snapshot {
    pub fn build<EK: KvEngine>(
        &mut self,
        engine: &EK,
        kv_snap: &EK::Snapshot,
        region: &Region,
        allow_multi_files_snapshot: bool,
        for_balance: bool,
        start: UnixSecs,
    ) -> RaftStoreResult<RaftSnapshotData> {
        let mut snap_data = RaftSnapshotData::default();
        snap_data.set_region(region.clone());

        let t = Instant::now();
        self.do_build::<EK>(
            engine,
            kv_snap,
            region,
            allow_multi_files_snapshot,
            for_balance,
        )?;

        let total_size = self.total_size();
        let total_count = self.total_count();
        // set snapshot meta data
        snap_data.set_file_size(total_size);
        snap_data.set_version(SNAPSHOT_VERSION);
        let meta = self.meta_file.meta.as_mut().unwrap();
        meta.set_start(start.into_inner());
        meta.set_generate_duration_sec(t.saturating_elapsed().as_secs());
        snap_data.set_meta(meta.clone());

        SNAPSHOT_BUILD_TIME_HISTOGRAM.observe(duration_to_sec(t.saturating_elapsed()));
        SNAPSHOT_KV_COUNT_HISTOGRAM.observe(total_count as f64);
        SNAPSHOT_SIZE_HISTOGRAM.observe(total_size as f64);
        info!(
            "scan snapshot";
            "region_id" => region.get_id(),
            "snapshot" => self.path(),
            "key_count" => total_count,
            "size" => total_size,
            "takes" => ?t.saturating_elapsed(),
        );

        Ok(snap_data)
    }

    pub fn apply<EK: KvEngine>(&mut self, options: ApplyOptions<EK>) -> Result<()> {
        let apply_without_ingest = self
            .mgr
            .can_apply_cf_without_ingest(self.total_size(), self.total_count());
        let post_check = |cf_file: &CfFile, offset: usize| {
            if !plain_file_used(cf_file.cf) {
                let file_paths = cf_file.file_paths();
                let clone_file_paths = cf_file.clone_file_paths();
                if options.ingest_copy_symlink && is_symlink(&file_paths[offset])? {
                    sst_importer::copy_sst_for_ingestion(
                        &file_paths[offset],
                        &clone_file_paths[offset],
                        self.mgr.encryption_key_manager.as_deref(),
                    )?;
                } else {
                    sst_importer::prepare_sst_for_ingestion(
                        &file_paths[offset],
                        &clone_file_paths[offset],
                        self.mgr.encryption_key_manager.as_deref(),
                    )?;
                }
            }
            Ok(())
        };

        box_try!(self.validate(post_check));

        let abort_checker = ApplyAbortChecker(options.abort);
        let coprocessor_host = options.coprocessor_host;
        let region = options.region;
        let key_mgr = self.mgr.encryption_key_manager.clone();
        let batch_size = options.write_batch_size;
        for cf_file in &mut self.cf_files {
            if cf_file.size.is_empty() {
                // Skip empty cf file.
                continue;
            }
            let cf = cf_file.cf;
            let mut cb = |kv: &[(Vec<u8>, Vec<u8>)]| {
                coprocessor_host.post_apply_plain_kvs_from_snapshot(&region, cf, kv)
            };
            if plain_file_used(cf_file.cf) {
                let path = &cf_file.file_paths()[0];
                snap_io::apply_plain_cf_file(
                    path,
                    key_mgr.as_ref(),
                    &abort_checker,
                    &options.db,
                    cf,
                    batch_size,
                    &mut cb,
                )?;
            } else {
                let _timer = INGEST_SST_DURATION_SECONDS.start_coarse_timer();
                let path = cf_file.path.to_str().unwrap(); // path is not used at all
                let clone_file_paths = cf_file.clone_file_paths();
                let clone_files = clone_file_paths
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<&str>>();
                if apply_without_ingest {
                    // Apply the snapshot without ingest, to accelerate the applying process.
                    snap_io::apply_sst_cf_files_without_ingest(
                        clone_files.as_slice(),
                        &options.db,
                        cf,
                        key_mgr.clone(),
                        &abort_checker,
                        batch_size,
                        &mut cb,
                    )?;
                } else {
                    // Apply the snapshot by ingest.
                    snap_io::apply_sst_cf_files_by_ingest(
                        clone_files.as_slice(),
                        &options.db,
                        cf,
                        enc_start_key(&region),
                        enc_end_key(&region),
                    )?;
                    coprocessor_host.post_apply_sst_from_snapshot(&region, cf, path);
                }
            }
        }
        Ok(())
    }

    pub fn path(&self) -> &str {
        &self.display_path
    }

    pub fn exists(&self) -> bool {
        self.cf_files.iter().all(|cf_file| {
            cf_file.size.is_empty()
                || (cf_file
                    .file_paths()
                    .iter()
                    .all(|file_path| file_exists(Path::new(file_path))))
        }) && file_exists(&self.meta_file.path)
    }

    pub fn meta(&self) -> io::Result<Metadata> {
        file_system::metadata(&self.meta_file.path)
    }

    pub fn meta_path(&self) -> &PathBuf {
        &self.meta_file.path
    }

    pub fn total_size(&self) -> u64 {
        self.cf_files
            .iter()
            .map(|cf| cf.size.iter().sum::<u64>())
            .sum()
    }

    pub fn total_count(&self) -> u64 {
        self.cf_files.iter().map(|cf| cf.kv_count).sum()
    }

    pub fn save(&mut self) -> io::Result<()> {
        debug!(
            "saving to snapshot file";
            "snapshot" => %self.path(),
        );
        for cf_file in &mut self.cf_files {
            if cf_file.size.is_empty() {
                // Skip empty cf file.
                continue;
            }

            // Check each cf file has been fully written, and the checksum matches.
            for (i, mut file_for_recving) in cf_file.file_for_recving.drain(..).enumerate() {
                file_for_recving.file.flush()?;
                file_for_recving.file.sync_all()?;

                if file_for_recving.written_size != cf_file.size[i] {
                    return Err(io::Error::new(
                        ErrorKind::InvalidData,
                        format!(
                            "snapshot file {} for cf {} size mismatches, \
                            real size {}, expected size {}",
                            cf_file.path.display(),
                            cf_file.cf,
                            file_for_recving.written_size,
                            cf_file.size[i]
                        ),
                    ));
                }

                let checksum = file_for_recving.write_digest.finalize();
                if checksum != cf_file.checksum[i] {
                    return Err(io::Error::new(
                        ErrorKind::InvalidData,
                        format!(
                            "snapshot file {} for cf {} checksum \
                            mismatches, real checksum {}, expected \
                            checksum {}",
                            cf_file.path.display(),
                            cf_file.cf,
                            checksum,
                            cf_file.checksum[i]
                        ),
                    ));
                }
            }

            let tmp_paths = cf_file.tmp_file_paths();
            let paths = cf_file.file_paths();
            for (i, tmp_path) in tmp_paths.iter().enumerate() {
                file_system::rename(tmp_path, &paths[i])?;
            }
        }
        sync_dir(&self.dir_path)?;

        // write meta file
        let v = self.meta_file.meta.as_ref().unwrap().write_to_bytes()?;
        {
            let mut meta_file = self.meta_file.file.take().unwrap();
            meta_file.write_all(&v[..])?;
            meta_file.sync_all()?;
        }
        file_system::rename(&self.meta_file.tmp_path, &self.meta_file.path)?;
        sync_dir(&self.dir_path)?;
        self.hold_tmp_files = false;
        Ok(())
    }

    pub fn cf_files(&self) -> &[CfFile] {
        &self.cf_files
    }
}

// To check whether a procedure about apply snapshot aborts or not.
struct ApplyAbortChecker(Arc<AtomicUsize>);
impl snap_io::StaleDetector for ApplyAbortChecker {
    fn is_stale(&self) -> bool {
        self.0.load(Ordering::Relaxed) == JOB_STATUS_CANCELLING
    }
}

impl Read for Snapshot {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        while self.cf_index < self.cf_files.len() {
            let cf_file = &mut self.cf_files[self.cf_index];
            if self.cf_file_index >= cf_file.size.len() || cf_file.size[self.cf_file_index] == 0 {
                self.cf_index += 1;
                self.cf_file_index = 0;
                continue;
            }
            let reader = cf_file
                .file_for_sending
                .get_mut(self.cf_file_index)
                .unwrap();
            match reader.read(buf) {
                Ok(0) => {
                    // EOF. Switch to next file.
                    self.cf_file_index += 1;
                    if self.cf_file_index == cf_file.size.len() {
                        self.cf_index += 1;
                        self.cf_file_index = 0;
                    }
                }
                Ok(n) => return Ok(n),
                e => return e,
            }
        }
        Ok(0)
    }
}

impl Write for Snapshot {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }

        let (mut next_buf, mut written_bytes) = (buf, 0);
        while self.cf_index < self.cf_files.len() {
            let cf_file = &mut self.cf_files[self.cf_index];
            if cf_file.size.is_empty() {
                self.cf_index += 1;
                continue;
            }

            assert!(cf_file.size[self.cf_file_index] != 0);
            let mut file_for_recving = cf_file
                .file_for_recving
                .get_mut(self.cf_file_index)
                .unwrap();
            let left = (cf_file.size.get(self.cf_file_index).unwrap()
                - file_for_recving.written_size) as usize;
            assert!(left > 0 && !next_buf.is_empty());
            let (write_len, switch, finished) = match next_buf.len().cmp(&left) {
                CmpOrdering::Greater => (left, true, false),
                CmpOrdering::Equal => (left, true, true),
                CmpOrdering::Less => (next_buf.len(), false, true),
            };

            file_for_recving
                .write_digest
                .update(&next_buf[0..write_len]);
            file_for_recving.written_size += write_len as u64;
            written_bytes += write_len;

            let file = &mut file_for_recving.file;
            let encrypt_buffer = if file_for_recving.encrypter.is_none() {
                Cow::Borrowed(&next_buf[0..write_len])
            } else {
                let (cipher, crypter) = file_for_recving.encrypter.as_mut().unwrap();
                let mut encrypt_buffer = vec![0; write_len + cipher.block_size()];
                let mut bytes = crypter.update(&next_buf[0..write_len], &mut encrypt_buffer)?;
                if switch {
                    bytes += crypter.finalize(&mut encrypt_buffer)?;
                }
                encrypt_buffer.truncate(bytes);
                Cow::Owned(encrypt_buffer)
            };
            let encrypt_len = encrypt_buffer.len();

            let mut start = 0;
            loop {
                let acquire = cmp::min(IO_LIMITER_CHUNK_SIZE, encrypt_len - start);
                self.mgr.limiter.blocking_consume(acquire);
                file.write_all(&encrypt_buffer[start..start + acquire])?;
                if start + acquire == encrypt_len {
                    break;
                }
                start += acquire;
            }
            if switch {
                next_buf = &next_buf[write_len..];
                self.cf_file_index += 1;
                if self.cf_file_index >= cf_file.size.len() {
                    self.cf_file_index = 0;
                    self.cf_index += 1;
                }
            }
            if finished {
                break;
            }
        }
        Ok(written_bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        if let Some(cf_file) = self.cf_files.get_mut(self.cf_index) {
            for file_for_recving in &mut cf_file.file_for_recving {
                file_for_recving.file.flush()?;
            }
        }
        Ok(())
    }
}

impl Drop for Snapshot {
    fn drop(&mut self) {
        // Cleanup if the snapshot is not built or received successfully.
        if self.hold_tmp_files {
            self.delete();
        }
    }
}

#[derive(PartialEq, Debug)]
pub enum SnapEntry {
    Generating = 1,
    Sending = 2,
    Receiving = 3,
    Applying = 4,
}

/// `SnapStats` is for snapshot statistics.
pub struct SnapStats {
    pub sending_count: usize,
    pub receiving_count: usize,
    pub stats: Vec<SnapshotStat>,
}

#[derive(Clone)]
struct SnapManagerCore {
    // directory to store snapfile.
    base: String,

    registry: Arc<RwLock<HashMap<SnapKey, Vec<SnapEntry>>>>,
    limiter: Limiter,
    temp_sst_id: Arc<AtomicU64>,
    encryption_key_manager: Option<Arc<DataKeyManager>>,
    max_per_file_size: Arc<AtomicU64>,
    enable_multi_snapshot_files: Arc<AtomicBool>,
    stats: Arc<Mutex<Vec<SnapshotStat>>>,
    // Minimal column family size & kv counts for applying by ingest.
    min_ingest_cf_size: u64,
    min_ingest_cf_kvs: u64,
    max_total_size: Arc<AtomicU64>,
    // Marker to represent the relative store is marked with Offline.
    offlined: Arc<AtomicBool>,
}

/// `SnapManagerCore` trace all current processing snapshots.
pub struct SnapManager {
    core: SnapManagerCore,
    // only used to receive snapshot from v2
    tablet_snap_manager: Option<TabletSnapManager>,
}

impl Clone for SnapManager {
    fn clone(&self) -> Self {
        SnapManager {
            core: self.core.clone(),
            tablet_snap_manager: self.tablet_snap_manager.clone(),
        }
    }
}

impl SnapManager {
    pub fn new<T: Into<String>>(path: T) -> Self {
        SnapManagerBuilder::default().build(path)
    }

    pub fn init(&self) -> io::Result<()> {
        let enc_enabled = self.core.encryption_key_manager.is_some();
        info!(
            "Initializing SnapManager, encryption is enabled: {}",
            enc_enabled
        );

        // Use write lock so only one thread initialize the directory at a time.
        let _lock = self.core.registry.wl();
        let path = Path::new(&self.core.base);
        if !path.exists() {
            file_system::create_dir_all(path)?;
            return Ok(());
        }
        if !path.is_dir() {
            return Err(io::Error::new(
                ErrorKind::Other,
                format!("{} should be a directory", path.display()),
            ));
        }
        for f in file_system::read_dir(path)? {
            let p = f?;
            if p.file_type()?.is_file() {
                if let Some(s) = p.file_name().to_str() {
                    if s.ends_with(TMP_FILE_SUFFIX) {
                        file_system::remove_file(p.path())?;
                    }
                }
            }
        }

        Ok(())
    }

    // [PerformanceCriticalPath]?? I/O involved API should be called in background
    // thread Return all snapshots which is idle not being used.
    pub fn list_idle_snap(&self) -> io::Result<Vec<(SnapKey, bool)>> {
        // Use a lock to protect the directory when scanning.
        let registry = self.core.registry.rl();
        let read_dir = file_system::read_dir(Path::new(&self.core.base))?;
        // Remove the duplicate snap keys.
        let mut v: Vec<_> = read_dir
            .filter_map(|p| {
                let p = match p {
                    Err(e) => {
                        error!(
                            "failed to list content of directory";
                            "directory" => %&self.core.base,
                            "err" => ?e,
                        );
                        return None;
                    }
                    Ok(p) => p,
                };
                match p.file_type() {
                    Ok(t) if t.is_file() => {}
                    _ => return None,
                }
                let file_name = p.file_name();
                let name = match file_name.to_str() {
                    None => return None,
                    Some(n) => n,
                };
                if name.starts_with(DEL_RANGE_PREFIX) {
                    // This is a temp file to store delete keys and ingest them into Engine.
                    return None;
                }

                let is_sending = name.starts_with(SNAP_GEN_PREFIX);
                let numbers: Vec<u64> = name.split('.').next().map_or_else(Vec::new, |s| {
                    s.split('_')
                        .skip(1)
                        .filter_map(|s| s.parse().ok())
                        .collect()
                });
                if numbers.len() < 3 {
                    error!(
                        "failed to parse snapkey";
                        "snap_key" => %name,
                    );
                    return None;
                }
                let snap_key = SnapKey::new(numbers[0], numbers[1], numbers[2]);
                if registry.contains_key(&snap_key) {
                    // Skip those registered snapshot.
                    return None;
                }
                Some((snap_key, is_sending))
            })
            .collect();
        v.sort();
        v.dedup();
        Ok(v)
    }

    pub fn get_temp_path_for_ingest(&self) -> String {
        let sst_id = self.core.temp_sst_id.fetch_add(1, Ordering::SeqCst);
        let filename = format!(
            "{}_{}{}{}",
            DEL_RANGE_PREFIX, sst_id, SST_FILE_SUFFIX, TMP_FILE_SUFFIX
        );
        let path = PathBuf::from(&self.core.base).join(filename);
        path.to_str().unwrap().to_string()
    }

    #[inline]
    pub fn has_registered(&self, key: &SnapKey) -> bool {
        self.core.registry.rl().contains_key(key)
    }

    /// Get a `Snapshot` can be used for `build`. Concurrent calls are allowed
    /// because only one caller can lock temporary disk files.
    ///
    /// NOTE: it calculates snapshot size by scanning the base directory.
    /// Don't call it in raftstore thread until the size limitation mechanism
    /// gets refactored.
    pub fn get_snapshot_for_building(&self, key: &SnapKey) -> RaftStoreResult<Box<Snapshot>> {
        let mut old_snaps = None;
        while self.get_total_snap_size()? > self.max_total_snap_size() {
            if old_snaps.is_none() {
                let snaps = self.list_idle_snap()?;
                let mut key_and_snaps = Vec::with_capacity(snaps.len());
                for (key, is_sending) in snaps {
                    if !is_sending {
                        continue;
                    }
                    let snap = match self.get_snapshot_for_sending(&key) {
                        Ok(snap) => snap,
                        Err(_) => continue,
                    };
                    if let Ok(modified) = snap.meta().and_then(|m| m.modified()) {
                        key_and_snaps.push((key, snap, modified));
                    }
                }
                key_and_snaps.sort_by_key(|&(_, _, modified)| Reverse(modified));
                old_snaps = Some(key_and_snaps);
            }
            match old_snaps.as_mut().unwrap().pop() {
                Some((key, snap, _)) => self.delete_snapshot(&key, snap.as_ref(), false),
                None => return Err(RaftStoreError::Snapshot(Error::TooManySnapshots)),
            };
        }

        let base = &self.core.base;
        let f = Snapshot::new_for_building(base, key, &self.core)?;
        Ok(Box::new(f))
    }

    pub fn get_snapshot_for_gc(
        &self,
        key: &SnapKey,
        is_sending: bool,
    ) -> RaftStoreResult<Box<Snapshot>> {
        let _lock = self.core.registry.rl();
        let base = &self.core.base;
        let s = Snapshot::new(base, key, is_sending, CheckPolicy::None, &self.core)?;
        fail_point!(
            "get_snapshot_for_gc",
            key.region_id == 2 && key.idx == 1,
            |_| { Err(box_err!("invalid cf number of snapshot meta")) }
        );
        Ok(Box::new(s))
    }

    pub fn get_snapshot_for_sending(&self, key: &SnapKey) -> RaftStoreResult<Box<Snapshot>> {
        let _lock = self.core.registry.rl();
        let base = &self.core.base;
        let mut s = Snapshot::new_for_sending(base, key, &self.core)?;
        let key_manager = match self.core.encryption_key_manager.as_ref() {
            Some(m) => m,
            None => return Ok(Box::new(s)),
        };
        for cf_file in &mut s.cf_files {
            let file_paths = cf_file.file_paths();
            for (i, file_path) in file_paths.iter().enumerate() {
                if cf_file.size[i] == 0 {
                    continue;
                }
                let reader = snap_io::get_decrypter_reader(file_path, key_manager)?;
                cf_file.file_for_sending[i] = reader;
            }
        }
        Ok(Box::new(s))
    }

    /// Get a `Snapshot` can be used for writing and then `save`. Concurrent
    /// calls are allowed because only one caller can lock temporary disk
    /// files.
    pub fn get_snapshot_for_receiving(
        &self,
        key: &SnapKey,
        snapshot_meta: SnapshotMeta,
    ) -> RaftStoreResult<Box<Snapshot>> {
        let _lock = self.core.registry.rl();
        let base = &self.core.base;
        let f = Snapshot::new_for_receiving(base, key, &self.core, snapshot_meta)?;
        Ok(Box::new(f))
    }

    // Tablet snapshot is the snapshot sent from raftstore-v2.
    // We enable v1 to receive it to enable tiflash node to receive and apply
    // snapshot from raftstore-v2.
    // To make it easy, we maintain an empty `store::snapshot` with tablet snapshot
    // path storing in it. So tiflash node can detect it and apply properly.
    pub fn gen_empty_snapshot_for_tablet_snapshot(
        &self,
        tablet_snap_key: &TabletSnapKey,
        for_balance: bool,
    ) -> RaftStoreResult<()> {
        let _lock = self.core.registry.rl();
        let base = &self.core.base;
        let tablet_snap_path = self
            .tablet_snap_manager
            .as_ref()
            .unwrap()
            .final_recv_path(tablet_snap_key);
        let snap_key = SnapKey::new(
            tablet_snap_key.region_id,
            tablet_snap_key.term,
            tablet_snap_key.idx,
        );
        let _ = Snapshot::new_for_tablet_snapshot(
            base,
            &snap_key,
            &self.core,
            tablet_snap_path.to_str().unwrap(),
            for_balance,
        )?;
        Ok(())
    }

    pub fn get_snapshot_for_applying(&self, key: &SnapKey) -> RaftStoreResult<Box<Snapshot>> {
        let _lock = self.core.registry.rl();
        let base = &self.core.base;
        let s = Snapshot::new_for_applying(base, key, &self.core)?;
        if !s.exists() {
            return Err(RaftStoreError::Other(From::from(format!(
                "snapshot of {:?} not exists.",
                key
            ))));
        }
        Ok(Box::new(s))
    }

    pub fn meta_file_exist(&self, key: &SnapKey) -> RaftStoreResult<()> {
        let _lock = self.core.registry.rl();
        let base = &self.core.base;
        // Use CheckPolicy::None to avoid reading meta file
        let s = Snapshot::new(base, key, false, CheckPolicy::None, &self.core)?;
        if !file_exists(s.meta_file.path.as_path()) {
            return Err(RaftStoreError::Other(From::from(format!(
                "snapshot of {:?} not exists.",
                key
            ))));
        }
        Ok(())
    }

    /// Get the approximate size of snap file exists in snap directory.
    ///
    /// Return value is not guaranteed to be accurate.
    ///
    /// NOTE: don't call it in raftstore thread.
    pub fn get_total_snap_size(&self) -> Result<u64> {
        let size_v1 = self.core.get_total_snap_size()?;
        let size_v2 = self
            .tablet_snap_manager
            .as_ref()
            .map(|s| s.total_snap_size().unwrap_or(0))
            .unwrap_or(0);
        Ok(size_v1 + size_v2)
    }

    pub fn max_total_snap_size(&self) -> u64 {
        self.core.max_total_size.load(Ordering::Acquire)
    }

    pub fn set_max_total_snap_size(&self, max_total_size: u64) {
        self.core
            .max_total_size
            .store(max_total_size, Ordering::Release);
    }

    pub fn set_max_per_file_size(&mut self, max_per_file_size: u64) {
        if max_per_file_size == 0 {
            self.core
                .max_per_file_size
                .store(u64::MAX, Ordering::Release);
        } else {
            self.core
                .max_per_file_size
                .store(max_per_file_size, Ordering::Release);
        }
    }

    pub fn get_actual_max_per_file_size(&self, allow_multi_files_snapshot: bool) -> u64 {
        self.core
            .get_actual_max_per_file_size(allow_multi_files_snapshot)
    }

    pub fn set_enable_multi_snapshot_files(&mut self, enable_multi_snapshot_files: bool) {
        self.core
            .enable_multi_snapshot_files
            .store(enable_multi_snapshot_files, Ordering::Release);
    }

    pub fn set_speed_limit(&self, bytes_per_sec: f64) {
        self.core.limiter.set_speed_limit(bytes_per_sec);
    }

    pub fn get_speed_limit(&self) -> f64 {
        self.core.limiter.speed_limit()
    }

    pub fn set_min_ingest_cf_limit(&mut self, bytes: ReadableSize) {
        self.core.min_ingest_cf_size = bytes.0;
        self.core.min_ingest_cf_kvs = std::cmp::max(10000, (bytes.as_mb_f64() * 10000.0) as u64);
    }

    pub fn collect_stat(&self, snap: SnapshotStat) {
        debug!(
            "collect snapshot stat";
            "region_id" => snap.region_id,
            "total_size" => snap.get_transport_size(),
            "total_duration_sec" => snap.get_total_duration_sec(),
            "generate_duration_sec" => snap.get_generate_duration_sec(),
            "send_duration_sec" => snap.get_generate_duration_sec(),
        );
        self.core.stats.lock().unwrap().push(snap);
    }

    pub fn register(&self, key: SnapKey, entry: SnapEntry) {
        debug!(
            "register snapshot";
            "key" => %key,
            "entry" => ?entry,
        );
        match self.core.registry.wl().entry(key) {
            Entry::Occupied(mut e) => {
                if e.get().contains(&entry) {
                    warn!(
                        "snap key is registered more than once!";
                        "key" => %e.key(),
                    );
                    return;
                }
                e.get_mut().push(entry);
            }
            Entry::Vacant(e) => {
                e.insert(vec![entry]);
            }
        }
    }

    pub fn deregister(&self, key: &SnapKey, entry: &SnapEntry) {
        debug!(
            "deregister snapshot";
            "key" => %key,
            "entry" => ?entry,
        );
        let mut need_clean = false;
        let mut handled = false;
        let registry = &mut self.core.registry.wl();
        if let Some(e) = registry.get_mut(key) {
            let last_len = e.len();
            e.retain(|e| e != entry);
            need_clean = e.is_empty();
            handled = last_len > e.len();
        }
        if need_clean {
            registry.remove(key);
        }
        if handled {
            return;
        }
        warn!(
            "stale deregister snapshot";
            "key" => %key,
            "entry" => ?entry,
        );
    }

    pub fn stats(&self) -> SnapStats {
        // send_count, generating_count, receiving_count, applying_count
        let (mut sending_cnt, mut receiving_cnt) = (0, 0);
        for v in self.core.registry.rl().values() {
            let (mut is_sending, mut is_receiving) = (false, false);
            for s in v {
                match *s {
                    SnapEntry::Sending | SnapEntry::Generating => is_sending = true,
                    SnapEntry::Receiving | SnapEntry::Applying => is_receiving = true,
                }
            }
            if is_sending {
                sending_cnt += 1;
            }
            if is_receiving {
                receiving_cnt += 1;
            }
        }

        let stats = std::mem::take(self.core.stats.lock().unwrap().as_mut());
        SnapStats {
            sending_count: sending_cnt,
            receiving_count: receiving_cnt,
            stats,
        }
    }

    pub fn delete_snapshot(&self, key: &SnapKey, snap: &Snapshot, check_entry: bool) -> bool {
        self.core.delete_snapshot(key, snap, check_entry)
    }

    pub fn tablet_snap_manager(&self) -> Option<&TabletSnapManager> {
        self.tablet_snap_manager.as_ref()
    }

    pub fn limiter(&self) -> &Limiter {
        &self.core.limiter
    }

    pub fn set_offline(&mut self, state: bool) {
        self.core.offlined.store(state, Ordering::Release);
    }

    pub fn is_offlined(&self) -> bool {
        self.core.offlined.load(Ordering::Acquire)
    }
}

impl SnapManagerCore {
    fn get_total_snap_size(&self) -> Result<u64> {
        let mut total_size = 0;
        for entry in file_system::read_dir(&self.base)? {
            let (entry, metadata) = match entry.and_then(|e| e.metadata().map(|m| (e, m))) {
                Ok((e, m)) => (e, m),
                Err(e) if e.kind() == ErrorKind::NotFound => continue,
                Err(e) => return Err(Error::from(e)),
            };

            let path = entry.path();
            let path_s = path.to_str().unwrap();
            if !metadata.is_file()
                // Ignore cloned files as they are hard links on posix systems.
                || path_s.ends_with(CLONE_FILE_SUFFIX)
                || path_s.ends_with(META_FILE_SUFFIX)
            {
                continue;
            }
            total_size += metadata.len();
        }
        Ok(total_size)
    }

    // Return true if it successfully delete the specified snapshot.
    fn delete_snapshot(&self, key: &SnapKey, snap: &Snapshot, check_entry: bool) -> bool {
        let registry = self.registry.rl();
        if check_entry {
            if let Some(e) = registry.get(key) {
                if e.len() > 1 {
                    info!(
                        "skip to delete snapshot since it's registered more than once";
                        "snapshot" => %snap.path(),
                        "registered_entries" => ?e,
                    );
                    return false;
                }
            }
        } else if registry.contains_key(key) {
            info!(
                "skip to delete snapshot since it's registered";
                "snapshot" => %snap.path(),
            );
            return false;
        }
        snap.delete();
        true
    }

    fn rename_tmp_cf_file_for_send(&self, cf_file: &mut CfFile) -> RaftStoreResult<()> {
        let tmp_file_paths = cf_file.tmp_file_paths();
        let file_paths = cf_file.file_paths();
        for (i, tmp_file_path) in tmp_file_paths.iter().enumerate() {
            let mgr = self.encryption_key_manager.as_ref();
            if let Some(mgr) = &mgr {
                let src = &tmp_file_path;
                let dst = &file_paths[i];
                // It's ok that the cf file is moved but machine fails before `mgr.rename_file`
                // because without metadata file, saved cf files are nothing.
                while let Err(e) = mgr.link_file(src, dst) {
                    if e.kind() == ErrorKind::AlreadyExists {
                        mgr.delete_file(dst, None)?;
                        continue;
                    }
                    return Err(e.into());
                }
                let r = file_system::rename(src, dst);
                let del_file = if r.is_ok() { src } else { dst };
                if let Err(e) = mgr.delete_file(del_file, None) {
                    warn!("fail to remove encryption metadata during 'rename_tmp_cf_file_for_send'";
                          "err" => ?e);
                }
                r?;
            } else {
                file_system::rename(tmp_file_path, &file_paths[i])?;
            }
            let file = Path::new(&file_paths[i]);
            let (checksum, size) = calc_checksum_and_size(file, mgr)?;
            cf_file.add_file_with_size_checksum(i, size, checksum);
        }
        Ok(())
    }

    pub fn get_actual_max_per_file_size(&self, allow_multi_files_snapshot: bool) -> u64 {
        if !allow_multi_files_snapshot {
            return u64::MAX;
        }

        if self.enable_multi_snapshot_files.load(Ordering::Relaxed) {
            return self.max_per_file_size.load(Ordering::Relaxed);
        }
        u64::MAX
    }

    pub fn can_apply_cf_without_ingest(&self, cf_size: u64, cf_kvs: u64) -> bool {
        fail_point!("apply_cf_without_ingest_false", |_| { false });
        if self.min_ingest_cf_size == 0 {
            return false;
        }
        // If the size and the count of keys of cf are relatively small, it's
        // recommended to directly write it into kvdb rather than ingest,
        // for mitigating performance issue when ingesting snapshot.
        cf_size <= self.min_ingest_cf_size && cf_kvs <= self.min_ingest_cf_kvs
    }
}

#[derive(Clone, Default)]
pub struct SnapManagerBuilder {
    max_write_bytes_per_sec: i64,
    max_total_size: u64,
    max_per_file_size: u64,
    enable_multi_snapshot_files: bool,
    enable_receive_tablet_snapshot: bool,
    key_manager: Option<Arc<DataKeyManager>>,
    min_ingest_snapshot_size: u64,
    min_ingest_snapshot_kvs: u64,
}

impl SnapManagerBuilder {
    #[must_use]
    pub fn max_write_bytes_per_sec(mut self, bytes: i64) -> SnapManagerBuilder {
        self.max_write_bytes_per_sec = bytes;
        self
    }
    #[must_use]
    pub fn max_total_size(mut self, bytes: u64) -> SnapManagerBuilder {
        self.max_total_size = bytes;
        self
    }
    pub fn max_per_file_size(mut self, bytes: u64) -> SnapManagerBuilder {
        self.max_per_file_size = bytes;
        self
    }
    pub fn enable_multi_snapshot_files(mut self, enabled: bool) -> SnapManagerBuilder {
        self.enable_multi_snapshot_files = enabled;
        self
    }
    pub fn enable_receive_tablet_snapshot(mut self, enabled: bool) -> SnapManagerBuilder {
        self.enable_receive_tablet_snapshot = enabled;
        self
    }
    pub fn min_ingest_snapshot_limit(mut self, bytes: ReadableSize) -> SnapManagerBuilder {
        self.min_ingest_snapshot_size = bytes.0;
        // Keeps the same assumptions in region size, "Assume the average size of KVs is
        // 100B". So, it calculate the count of kvs with `bytes / `MiB` * 10000`.
        self.min_ingest_snapshot_kvs = std::cmp::max(10000, (bytes.as_mb_f64() * 10000.0) as u64);
        self
    }
    #[must_use]
    pub fn encryption_key_manager(mut self, m: Option<Arc<DataKeyManager>>) -> SnapManagerBuilder {
        self.key_manager = m;
        self
    }
    pub fn build<T: Into<String>>(self, path: T) -> SnapManager {
        let limiter = Limiter::new(if self.max_write_bytes_per_sec > 0 {
            self.max_write_bytes_per_sec as f64
        } else {
            f64::INFINITY
        });
        let max_total_size = if self.max_total_size > 0 {
            self.max_total_size
        } else {
            u64::MAX
        };
        let path = path.into();
        assert!(!path.is_empty());
        let mut path_v2 = path.clone();
        path_v2.push_str("_v2");
        let tablet_snap_manager = if self.enable_receive_tablet_snapshot {
            Some(TabletSnapManager::new(&path_v2, self.key_manager.clone()).unwrap())
        } else {
            None
        };

        let mut snapshot = SnapManager {
            core: SnapManagerCore {
                base: path,
                registry: Default::default(),
                limiter,
                temp_sst_id: Arc::new(AtomicU64::new(0)),
                encryption_key_manager: self.key_manager,
                max_per_file_size: Arc::new(AtomicU64::new(u64::MAX)),
                enable_multi_snapshot_files: Arc::new(AtomicBool::new(
                    self.enable_multi_snapshot_files,
                )),
                max_total_size: Arc::new(AtomicU64::new(max_total_size)),
                stats: Default::default(),
                min_ingest_cf_size: self.min_ingest_snapshot_size,
                min_ingest_cf_kvs: self.min_ingest_snapshot_kvs,
                offlined: Arc::new(AtomicBool::new(false)),
            },
            tablet_snap_manager,
        };
        snapshot.set_max_per_file_size(self.max_per_file_size); // set actual max_per_file_size
        snapshot
    }
}

#[derive(Clone, Hash, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct TabletSnapKey {
    pub region_id: u64,
    pub to_peer: u64,
    pub term: u64,
    pub idx: u64,
}

impl TabletSnapKey {
    #[inline]
    pub fn new(region_id: u64, to_peer: u64, term: u64, idx: u64) -> TabletSnapKey {
        TabletSnapKey {
            region_id,
            to_peer,
            term,
            idx,
        }
    }

    pub fn from_region_snap(region_id: u64, to_peer: u64, snap: &RaftSnapshot) -> TabletSnapKey {
        let index = snap.get_metadata().get_index();
        let term = snap.get_metadata().get_term();
        TabletSnapKey::new(region_id, to_peer, term, index)
    }

    pub fn from_path<T: Into<PathBuf>>(path: T) -> Result<TabletSnapKey> {
        let path = path.into();
        let name = path.file_name().unwrap().to_str().unwrap();
        let numbers: Vec<u64> = name
            .split('_')
            .skip(1)
            .filter_map(|s| s.parse().ok())
            .collect();
        if numbers.len() < 4 {
            return Err(box_err!("invalid tablet snapshot file name:{}", name));
        }
        Ok(TabletSnapKey::new(
            numbers[0], numbers[1], numbers[2], numbers[3],
        ))
    }
}

impl Display for TabletSnapKey {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}_{}_{}_{}",
            self.region_id, self.to_peer, self.term, self.idx
        )
    }
}

pub struct ReceivingGuard<'a> {
    receiving: &'a Mutex<Vec<TabletSnapKey>>,
    key: TabletSnapKey,
}

impl Drop for ReceivingGuard<'_> {
    fn drop(&mut self) {
        let mut receiving = self.receiving.lock().unwrap();
        let pos = receiving.iter().position(|k| k == &self.key).unwrap();
        receiving.swap_remove(pos);
    }
}

/// `TabletSnapManager` manager tablet snapshot and shared between raftstore v2.
/// It's similar `SnapManager`, but simpler in tablet version.
///
///  TODO:
///     - clean up expired tablet checkpointer
#[derive(Clone)]
pub struct TabletSnapManager {
    // directory to store snapfile.
    base: PathBuf,
    key_manager: Option<Arc<DataKeyManager>>,
    receiving: Arc<Mutex<Vec<TabletSnapKey>>>,
    stats: Arc<Mutex<HashMap<TabletSnapKey, (Instant, SnapshotStat)>>>,
    sending_count: Arc<AtomicUsize>,
    recving_count: Arc<AtomicUsize>,
}

impl TabletSnapManager {
    pub fn new<T: Into<PathBuf>>(
        path: T,
        key_manager: Option<Arc<DataKeyManager>>,
    ) -> io::Result<Self> {
        let path = path.into();
        if !path.exists() {
            file_system::create_dir_all(&path)?;
        }
        if !path.is_dir() {
            return Err(io::Error::new(
                ErrorKind::Other,
                format!("{} should be a directory", path.display()),
            ));
        }
        encryption::clean_up_dir(&path, SNAP_GEN_PREFIX, key_manager.as_deref())?;
        encryption::clean_up_trash(&path, key_manager.as_deref())?;
        Ok(Self {
            base: path,
            key_manager,
            receiving: Arc::default(),
            stats: Arc::default(),
            sending_count: Arc::default(),
            recving_count: Arc::default(),
        })
    }

    pub fn begin_snapshot(&self, key: TabletSnapKey, start: Instant, generate_duration_sec: u64) {
        let mut stat = SnapshotStat::default();
        stat.set_generate_duration_sec(generate_duration_sec);
        self.stats.lock().unwrap().insert(key, (start, stat));
    }

    pub fn finish_snapshot(&self, key: TabletSnapKey, send: Instant) {
        let region_id = key.region_id;
        self.stats
            .lock()
            .unwrap()
            .entry(key)
            .and_modify(|(start, stat)| {
                stat.set_send_duration_sec(send.saturating_elapsed().as_secs());
                stat.set_total_duration_sec(start.saturating_elapsed().as_secs());
                stat.set_region_id(region_id);
            });
    }

    pub fn stats(&self) -> SnapStats {
        let stats: Vec<SnapshotStat> = self
            .stats
            .lock()
            .unwrap()
            .drain_filter(|_, (_, stat)| stat.get_region_id() > 0)
            .map(|(_, (_, stat))| stat)
            .filter(|stat| stat.get_total_duration_sec() > 1)
            .collect();
        SnapStats {
            sending_count: self.sending_count.load(Ordering::SeqCst),
            receiving_count: self.recving_count.load(Ordering::SeqCst),
            stats,
        }
    }

    pub fn tablet_gen_path(&self, key: &TabletSnapKey) -> PathBuf {
        let prefix = format!("{}_{}", SNAP_GEN_PREFIX, key);
        PathBuf::from(&self.base).join(prefix)
    }

    pub fn final_recv_path(&self, key: &TabletSnapKey) -> PathBuf {
        let prefix = format!("{}_{}", SNAP_REV_PREFIX, key);
        PathBuf::from(&self.base).join(prefix)
    }

    pub fn tmp_recv_path(&self, key: &TabletSnapKey) -> PathBuf {
        let prefix = format!("{}_{}{}", SNAP_REV_PREFIX, key, TMP_FILE_SUFFIX);
        PathBuf::from(&self.base).join(prefix)
    }

    pub fn delete_snapshot(&self, key: &TabletSnapKey) -> bool {
        let path = self.tablet_gen_path(key);
        debug!("delete tablet snapshot file";"path" => %path.display());
        if path.exists() {
            if let Err(e) = encryption::trash_dir_all(&path, self.key_manager.as_deref()) {
                error!(
                    "delete snapshot failed";
                    "path" => %path.display(),
                    "err" => ?e,
                );
                return false;
            }
        }
        true
    }

    pub fn list_snapshot(&self) -> Result<Vec<PathBuf>> {
        let mut paths = Vec::new();
        for entry in file_system::read_dir(&self.base)? {
            let entry = match entry {
                Ok(e) => e,
                Err(e) if e.kind() == ErrorKind::NotFound => continue,
                Err(e) => return Err(Error::from(e)),
            };

            let path = entry.path();
            if path.file_name().and_then(|n| n.to_str()).map_or(true, |n| {
                !n.starts_with(SNAP_GEN_PREFIX) || n.ends_with(TMP_FILE_SUFFIX)
            }) {
                continue;
            }
            paths.push(path);
        }
        Ok(paths)
    }

    pub fn total_snap_size(&self) -> Result<u64> {
        let mut total_size = 0;
        for entry in file_system::read_dir(&self.base)? {
            let entry = match entry {
                Ok(e) => e,
                Err(e) if e.kind() == ErrorKind::NotFound => continue,
                Err(e) => return Err(Error::from(e)),
            };

            let path = entry.path();
            // Generated snapshots are just checkpoints, only counts received snapshots.
            if !path
                .file_name()
                .and_then(|n| n.to_str())
                .map_or(true, |n| n.starts_with(SNAP_REV_PREFIX))
            {
                continue;
            }
            let entries = match file_system::read_dir(path) {
                Ok(entries) => entries,
                Err(e) if e.kind() == ErrorKind::NotFound => continue,
                Err(e) => return Err(Error::from(e)),
            };
            for e in entries {
                match e.and_then(|e| e.metadata()) {
                    Ok(m) => total_size += m.len(),
                    Err(e) if e.kind() == ErrorKind::NotFound => continue,
                    Err(e) => return Err(Error::from(e)),
                }
            }
        }
        Ok(total_size)
    }

    #[inline]
    pub fn root_path(&self) -> &Path {
        self.base.as_path()
    }

    pub fn start_receive(&self, key: TabletSnapKey) -> Option<ReceivingGuard<'_>> {
        let mut receiving = self.receiving.lock().unwrap();
        if receiving.iter().any(|k| k == &key) {
            return None;
        }
        receiving.push(key.clone());
        Some(ReceivingGuard {
            receiving: &self.receiving,
            key,
        })
    }

    pub fn sending_count(&self) -> &Arc<AtomicUsize> {
        &self.sending_count
    }

    pub fn recving_count(&self) -> &Arc<AtomicUsize> {
        &self.recving_count
    }

    #[inline]
    pub fn key_manager(&self) -> &Option<Arc<DataKeyManager>> {
        &self.key_manager
    }
}

fn is_symlink<P: AsRef<Path>>(path: P) -> Result<bool> {
    let metadata = box_try!(std::fs::symlink_metadata(path));
    Ok(metadata.is_symlink())
}

#[cfg(test)]
pub mod tests {
    use std::{
        cmp, fs,
        io::{self, Read, Seek, SeekFrom, Write},
        path::{Path, PathBuf},
        sync::{
            atomic::{AtomicBool, AtomicU64, AtomicUsize},
            Arc,
        },
    };

    use encryption::{DataKeyManager, EncryptionConfig, FileConfig, MasterKeyConfig};
    use encryption_export::data_key_manager_from_config;
    use engine_test::{
        ctor::{CfOptions, DbOptions, KvEngineConstructorExt, RaftDbOptions},
        kv::KvTestEngine,
        raft::RaftTestEngine,
    };
    use engine_traits::{
        Engines, ExternalSstFileInfo, KvEngine, RaftEngine, RaftLogBatch,
        Snapshot as EngineSnapshot, SstExt, SstWriter, SstWriterBuilder, SyncMutable, ALL_CFS,
        CF_DEFAULT, CF_LOCK, CF_RAFT, CF_WRITE,
    };
    use kvproto::{
        encryptionpb::EncryptionMethod,
        metapb::{Peer, Region},
        raft_serverpb::{RaftApplyState, RegionLocalState, SnapshotMeta},
    };
    use protobuf::Message;
    use raft::eraftpb::Entry;
    use tempfile::{Builder, TempDir};
    use tikv_util::time::Limiter;

    use super::*;
    use crate::{
        coprocessor::CoprocessorHost,
        store::{peer_storage::JOB_STATUS_RUNNING, INIT_EPOCH_CONF_VER, INIT_EPOCH_VER},
        Result,
    };

    const TEST_STORE_ID: u64 = 1;
    const TEST_KEY: &[u8] = b"akey";
    const TEST_WRITE_BATCH_SIZE: usize = 10 * 1024 * 1024;
    const TEST_META_FILE_BUFFER_SIZE: usize = 1000;
    const BYTE_SIZE: usize = 1;

    type DbBuilder<E> = fn(
        p: &Path,
        db_opt: Option<DbOptions>,
        cf_opts: Option<Vec<(&'static str, CfOptions)>>,
    ) -> Result<E>;

    pub fn open_test_empty_db<E>(
        path: &Path,
        db_opt: Option<DbOptions>,
        cf_opts: Option<Vec<(&'static str, CfOptions)>>,
    ) -> Result<E>
    where
        E: KvEngine + KvEngineConstructorExt,
    {
        let p = path.to_str().unwrap();
        let db_opt = db_opt.unwrap_or_default();
        let cf_opts = cf_opts.unwrap_or_else(|| {
            ALL_CFS
                .iter()
                .map(|cf| (*cf, CfOptions::default()))
                .collect()
        });
        let db = E::new_kv_engine_opt(p, db_opt, cf_opts).unwrap();
        Ok(db)
    }

    pub fn open_test_db<E>(
        path: &Path,
        db_opt: Option<DbOptions>,
        cf_opts: Option<Vec<(&'static str, CfOptions)>>,
    ) -> Result<E>
    where
        E: KvEngine + KvEngineConstructorExt,
    {
        let db = open_test_empty_db::<E>(path, db_opt, cf_opts).unwrap();
        let key = keys::data_key(TEST_KEY);
        // write some data into each cf
        for (i, cf) in db.cf_names().into_iter().enumerate() {
            let mut p = Peer::default();
            p.set_store_id(TEST_STORE_ID);
            p.set_id((i + 1) as u64);
            db.put_msg_cf(cf, &key[..], &p)?;
        }
        Ok(db)
    }

    pub fn open_test_db_with_100keys<E>(
        path: &Path,
        db_opt: Option<DbOptions>,
        cf_opts: Option<Vec<(&'static str, CfOptions)>>,
    ) -> Result<E>
    where
        E: KvEngine + KvEngineConstructorExt,
    {
        let db = open_test_empty_db::<E>(path, db_opt, cf_opts).unwrap();
        // write some data into each cf
        for (i, cf) in db.cf_names().into_iter().enumerate() {
            let mut p = Peer::default();
            p.set_store_id(TEST_STORE_ID);
            p.set_id((i + 1) as u64);
            for k in 0..100 {
                let key = keys::data_key(format!("akey{}", k).as_bytes());
                db.put_msg_cf(cf, &key[..], &p)?;
            }
        }
        Ok(db)
    }

    pub fn get_test_db_for_regions(
        path: &TempDir,
        raft_db_opt: Option<RaftDbOptions>,
        kv_db_opt: Option<DbOptions>,
        kv_cf_opts: Option<Vec<(&'static str, CfOptions)>>,
        regions: &[u64],
    ) -> Result<Engines<KvTestEngine, RaftTestEngine>> {
        let p = path.path();
        let kv: KvTestEngine = open_test_db(p.join("kv").as_path(), kv_db_opt, kv_cf_opts)?;
        let raft: RaftTestEngine =
            engine_test::raft::new_engine(p.join("raft").to_str().unwrap(), raft_db_opt)?;
        let mut lb = raft.log_batch(regions.len() * 128);
        for &region_id in regions {
            // Put apply state into kv engine.
            let mut apply_state = RaftApplyState::default();
            let mut apply_entry = Entry::default();
            apply_state.set_applied_index(10);
            apply_entry.set_index(10);
            apply_entry.set_term(0);
            apply_state.mut_truncated_state().set_index(10);
            kv.put_msg_cf(CF_RAFT, &keys::apply_state_key(region_id), &apply_state)?;
            lb.append(region_id, None, vec![apply_entry])?;

            // Put region info into kv engine.
            let region = gen_test_region(region_id, 1, 1);
            let mut region_state = RegionLocalState::default();
            region_state.set_region(region);
            kv.put_msg_cf(CF_RAFT, &keys::region_state_key(region_id), &region_state)?;
        }
        raft.consume(&mut lb, false).unwrap();
        Ok(Engines::new(kv, raft))
    }

    pub fn get_kv_count(snap: &impl EngineSnapshot) -> u64 {
        let mut kv_count = 0;
        for cf in SNAPSHOT_CFS {
            snap.scan(
                cf,
                &keys::data_key(b"a"),
                &keys::data_key(b"z"),
                false,
                |_, _| {
                    kv_count += 1;
                    Ok(true)
                },
            )
            .unwrap();
        }
        kv_count
    }

    pub fn gen_test_region(region_id: u64, store_id: u64, peer_id: u64) -> Region {
        let mut peer = Peer::default();
        peer.set_store_id(store_id);
        peer.set_id(peer_id);
        let mut region = Region::default();
        region.set_id(region_id);
        region.set_start_key(b"a".to_vec());
        region.set_end_key(b"z".to_vec());
        region.mut_region_epoch().set_version(INIT_EPOCH_VER);
        region.mut_region_epoch().set_conf_ver(INIT_EPOCH_CONF_VER);
        region.mut_peers().push(peer);
        region
    }

    pub fn assert_eq_db(expected_db: &impl KvEngine, db: &impl KvEngine) {
        let key = keys::data_key(TEST_KEY);
        for cf in SNAPSHOT_CFS {
            let p1: Option<Peer> = expected_db.get_msg_cf(cf, &key[..]).unwrap();
            if let Some(p1) = p1 {
                let p2: Option<Peer> = db.get_msg_cf(cf, &key[..]).unwrap();
                if let Some(p2) = p2 {
                    if p2 != p1 {
                        panic!(
                            "cf {}: key {:?}, value {:?}, expected {:?}",
                            cf, key, p2, p1
                        );
                    }
                } else {
                    panic!("cf {}: expect key {:?} has value", cf, key);
                }
            }
        }
    }

    fn create_manager_core(path: &str, max_per_file_size: u64) -> SnapManagerCore {
        SnapManagerCore {
            base: path.to_owned(),
            registry: Default::default(),
            limiter: Limiter::new(f64::INFINITY),
            temp_sst_id: Arc::new(AtomicU64::new(0)),
            encryption_key_manager: None,
            max_per_file_size: Arc::new(AtomicU64::new(max_per_file_size)),
            enable_multi_snapshot_files: Arc::new(AtomicBool::new(true)),
            max_total_size: Arc::new(AtomicU64::new(u64::MAX)),
            stats: Default::default(),
            min_ingest_cf_size: 0,
            min_ingest_cf_kvs: 0,
            offlined: Arc::new(AtomicBool::new(false)),
        }
    }

    fn create_encryption_key_manager(prefix: &str) -> (TempDir, Arc<DataKeyManager>) {
        let dir = Builder::new().prefix(prefix).tempdir().unwrap();
        let master_path = dir.path().join("master_key");

        let mut f = OpenOptions::new()
            .create_new(true)
            .append(true)
            .open(&master_path)
            .unwrap();
        // A 32 bytes key (in hex) followed by one '\n'.
        f.write_all(&[b'A'; 64]).unwrap();
        f.write_all(&[b'\n'; 1]).unwrap();

        let dict_path = dir.path().join("dict");
        file_system::create_dir_all(&dict_path).unwrap();

        let key_path = master_path.to_str().unwrap().to_owned();
        let dict_path = dict_path.to_str().unwrap().to_owned();

        let enc_cfg = EncryptionConfig {
            data_encryption_method: EncryptionMethod::Aes128Ctr,
            master_key: MasterKeyConfig::File {
                config: FileConfig { path: key_path },
            },
            ..Default::default()
        };
        let key_manager = data_key_manager_from_config(&enc_cfg, &dict_path)
            .unwrap()
            .map(|x| Arc::new(x));
        (dir, key_manager.unwrap())
    }

    pub fn gen_db_options_with_encryption(prefix: &str) -> (TempDir, DbOptions) {
        let (_enc_dir, key_manager) = create_encryption_key_manager(prefix);
        let mut db_opts = DbOptions::default();
        db_opts.set_key_manager(Some(key_manager));
        (_enc_dir, db_opts)
    }

    #[test]
    fn test_gen_snapshot_meta() {
        let mut cf_file = Vec::with_capacity(super::SNAPSHOT_CFS.len());
        for (i, cf) in super::SNAPSHOT_CFS.iter().enumerate() {
            let f = super::CfFile {
                cf,
                size: vec![100 * (i + 1) as u64, 100 * (i + 2) as u64],
                checksum: vec![1000 * (i + 1) as u32, 1000 * (i + 2) as u32],
                ..Default::default()
            };
            cf_file.push(f);
        }
        let meta = super::gen_snapshot_meta(&cf_file, false).unwrap();
        let cf_files = meta.get_cf_files();
        assert_eq!(cf_files.len(), super::SNAPSHOT_CFS.len() * 2); // each CF has two snapshot files;
        for (i, cf_file_meta) in meta.get_cf_files().iter().enumerate() {
            let cf_file_idx = i / 2;
            let size_idx = i % 2;
            if cf_file_meta.get_cf() != cf_file[cf_file_idx].cf {
                panic!(
                    "{}: expect cf {}, got {}",
                    i,
                    cf_file[cf_file_idx].cf,
                    cf_file_meta.get_cf()
                );
            }
            if cf_file_meta.get_size() != cf_file[cf_file_idx].size[size_idx] {
                panic!(
                    "{}: expect cf size {}, got {}",
                    i,
                    cf_file[cf_file_idx].size[size_idx],
                    cf_file_meta.get_size()
                );
            }
            if cf_file_meta.get_checksum() != cf_file[cf_file_idx].checksum[size_idx] {
                panic!(
                    "{}: expect cf checksum {}, got {}",
                    i,
                    cf_file[cf_file_idx].checksum[size_idx],
                    cf_file_meta.get_checksum()
                );
            }
        }
    }

    #[test]
    fn test_display_path() {
        let dir = Builder::new()
            .prefix("test-display-path")
            .tempdir()
            .unwrap();
        let key = SnapKey::new(1, 1, 1);
        let prefix = format!("{}_{}", SNAP_GEN_PREFIX, key);
        let display_path = Snapshot::get_display_path(dir.path(), &prefix);
        assert_ne!(display_path, "");
    }

    #[test]
    fn test_empty_snap_file() {
        test_snap_file(open_test_empty_db, u64::MAX);
        test_snap_file(open_test_empty_db, 100);
    }

    #[test]
    fn test_non_empty_snap_file() {
        test_snap_file(open_test_db, u64::MAX);
        test_snap_file(open_test_db_with_100keys, 100);
        test_snap_file(open_test_db_with_100keys, 500);
    }

    fn test_snap_file(get_db: DbBuilder<KvTestEngine>, max_file_size: u64) {
        let region_id = 1;
        let region = gen_test_region(region_id, 1, 1);
        let src_db_dir = Builder::new()
            .prefix("test-snap-file-db-src")
            .tempdir()
            .unwrap();
        let db = get_db(src_db_dir.path(), None, None).unwrap();
        let snapshot = db.snapshot();

        let src_dir = Builder::new()
            .prefix("test-snap-file-db-src")
            .tempdir()
            .unwrap();

        let key = SnapKey::new(region_id, 1, 1);

        let mgr_core = create_manager_core(src_dir.path().to_str().unwrap(), max_file_size);
        let mut s1 = Snapshot::new_for_building(src_dir.path(), &key, &mgr_core).unwrap();

        // Ensure that this snapshot file doesn't exist before being built.
        assert!(!s1.exists());
        assert_eq!(mgr_core.get_total_snap_size().unwrap(), 0);

        let mut snap_data = s1
            .build(&db, &snapshot, &region, true, false, UnixSecs::now())
            .unwrap();

        // Ensure that this snapshot file does exist after being built.
        assert!(s1.exists());
        let size = s1.total_size();
        // Ensure the `size_track` is modified correctly.
        assert_eq!(size, mgr_core.get_total_snap_size().unwrap());
        assert_eq!(s1.total_count(), get_kv_count(&snapshot));

        // Ensure this snapshot could be read for sending.
        let mut s2 = Snapshot::new_for_sending(src_dir.path(), &key, &mgr_core).unwrap();
        assert!(s2.exists());

        // TODO check meta data correct.
        let _ = s2.meta().unwrap();

        let mut s3 =
            Snapshot::new_for_receiving(src_dir.path(), &key, &mgr_core, snap_data.take_meta())
                .unwrap();
        assert!(!s3.exists());

        // Ensure snapshot data could be read out of `s2`, and write into `s3`.
        let copy_size = io::copy(&mut s2, &mut s3).unwrap();
        assert_eq!(copy_size, size);
        assert!(!s3.exists());
        s3.save().unwrap();
        assert!(s3.exists());

        // Ensure the tracked size is handled correctly after receiving a snapshot.
        assert_eq!(mgr_core.get_total_snap_size().unwrap(), size * 2);

        // Ensure `delete()` works to delete the source snapshot.
        s2.delete();
        assert!(!s2.exists());
        assert!(!s1.exists());
        assert_eq!(mgr_core.get_total_snap_size().unwrap(), size);

        // Ensure a snapshot could be applied to DB.
        let mut s4 = Snapshot::new_for_applying(src_dir.path(), &key, &mgr_core).unwrap();
        assert!(s4.exists());

        let dst_db_dir = Builder::new()
            .prefix("test-snap-file-dst")
            .tempdir()
            .unwrap();
        let dst_db_path = dst_db_dir.path().to_str().unwrap();
        // Change arbitrarily the cf order of ALL_CFS at destination db.
        let dst_cfs = [CF_WRITE, CF_DEFAULT, CF_LOCK, CF_RAFT];
        let dst_db = engine_test::kv::new_engine(dst_db_path, &dst_cfs).unwrap();
        let options = ApplyOptions {
            db: dst_db.clone(),
            region,
            abort: Arc::new(AtomicUsize::new(JOB_STATUS_RUNNING)),
            write_batch_size: TEST_WRITE_BATCH_SIZE,
            coprocessor_host: CoprocessorHost::<KvTestEngine>::default(),
            ingest_copy_symlink: false,
        };
        // Verify the snapshot applying is ok.
        s4.apply(options).unwrap();

        // Ensure `delete()` works to delete the dest snapshot.
        s4.delete();
        assert!(!s4.exists());
        assert!(!s3.exists());
        assert_eq!(mgr_core.get_total_snap_size().unwrap(), 0);

        // Verify the data is correct after applying snapshot.
        assert_eq_db(&db, &dst_db);
    }

    #[test]
    fn test_empty_snap_validation() {
        test_snap_validation(open_test_empty_db, u64::MAX);
        test_snap_validation(open_test_empty_db, 100);
    }

    #[test]
    fn test_non_empty_snap_validation() {
        test_snap_validation(open_test_db, u64::MAX);
        test_snap_validation(open_test_db_with_100keys, 500);
    }

    fn test_snap_validation(get_db: DbBuilder<KvTestEngine>, max_file_size: u64) {
        let region_id = 1;
        let region = gen_test_region(region_id, 1, 1);
        let db_dir = Builder::new()
            .prefix("test-snap-validation-db")
            .tempdir()
            .unwrap();
        let db = get_db(db_dir.path(), None, None).unwrap();
        let snapshot = db.snapshot();

        let dir = Builder::new()
            .prefix("test-snap-validation")
            .tempdir()
            .unwrap();
        let key = SnapKey::new(region_id, 1, 1);
        let mgr_core = create_manager_core(dir.path().to_str().unwrap(), max_file_size);
        let mut s1 = Snapshot::new_for_building(dir.path(), &key, &mgr_core).unwrap();
        assert!(!s1.exists());

        let _ = s1
            .build(&db, &snapshot, &region, true, false, UnixSecs::now())
            .unwrap();
        assert!(s1.exists());

        let mut s2 = Snapshot::new_for_building(dir.path(), &key, &mgr_core).unwrap();
        assert!(s2.exists());

        let _ = s2
            .build(&db, &snapshot, &region, true, false, UnixSecs::now())
            .unwrap();
        assert!(s2.exists());
    }

    // Make all the snapshot in the specified dir corrupted to have incorrect
    // checksum.
    fn corrupt_snapshot_checksum_in<T: Into<PathBuf>>(dir: T) -> Vec<SnapshotMeta> {
        let dir_path = dir.into();
        let mut res = Vec::new();
        let read_dir = file_system::read_dir(dir_path).unwrap();
        for p in read_dir {
            if p.is_ok() {
                let e = p.as_ref().unwrap();
                if e.file_name()
                    .into_string()
                    .unwrap()
                    .ends_with(META_FILE_SUFFIX)
                {
                    let mut snapshot_meta = SnapshotMeta::default();
                    let mut buf = Vec::with_capacity(TEST_META_FILE_BUFFER_SIZE);
                    {
                        let mut f = OpenOptions::new().read(true).open(e.path()).unwrap();
                        f.read_to_end(&mut buf).unwrap();
                    }

                    snapshot_meta.merge_from_bytes(&buf).unwrap();

                    for cf in snapshot_meta.mut_cf_files().iter_mut() {
                        let corrupted_checksum = cf.get_checksum() + 100;
                        cf.set_checksum(corrupted_checksum);
                    }

                    let buf = snapshot_meta.write_to_bytes().unwrap();
                    {
                        let mut f = OpenOptions::new()
                            .write(true)
                            .truncate(true)
                            .open(e.path())
                            .unwrap();
                        f.write_all(&buf[..]).unwrap();
                        f.flush().unwrap();
                    }

                    res.push(snapshot_meta);
                }
            }
        }
        res
    }

    // Make all the snapshot meta files in the specified corrupted to have incorrect
    // content.
    fn corrupt_snapshot_meta_file<T: Into<PathBuf>>(dir: T) -> usize {
        let mut total = 0;
        let dir_path = dir.into();
        let read_dir = file_system::read_dir(dir_path).unwrap();
        for p in read_dir {
            if p.is_ok() {
                let e = p.as_ref().unwrap();
                if e.file_name()
                    .into_string()
                    .unwrap()
                    .ends_with(META_FILE_SUFFIX)
                {
                    let mut f = OpenOptions::new()
                        .read(true)
                        .write(true)
                        .open(e.path())
                        .unwrap();
                    // Make the last byte of the meta file corrupted
                    // by turning over all bits of it
                    let pos = SeekFrom::End(-(BYTE_SIZE as i64));
                    f.seek(pos).unwrap();
                    let mut buf = [0; BYTE_SIZE];
                    f.read_exact(&mut buf[..]).unwrap();
                    buf[0] ^= u8::max_value();
                    f.seek(pos).unwrap();
                    f.write_all(&buf[..]).unwrap();
                    total += 1;
                }
            }
        }
        total
    }

    fn copy_snapshot(
        from_dir: &TempDir,
        to_dir: &TempDir,
        key: &SnapKey,
        mgr: &SnapManagerCore,
        snapshot_meta: SnapshotMeta,
    ) {
        let mut from = Snapshot::new_for_sending(from_dir.path(), key, mgr).unwrap();
        assert!(from.exists());

        let mut to = Snapshot::new_for_receiving(to_dir.path(), key, mgr, snapshot_meta).unwrap();

        assert!(!to.exists());
        let _ = io::copy(&mut from, &mut to).unwrap();
        to.save().unwrap();
        assert!(to.exists());
    }

    #[test]
    fn test_snap_corruption_on_checksum() {
        let region_id = 1;
        let region = gen_test_region(region_id, 1, 1);
        let db_dir = Builder::new()
            .prefix("test-snap-corruption-db")
            .tempdir()
            .unwrap();
        let db: KvTestEngine = open_test_db(db_dir.path(), None, None).unwrap();
        let snapshot = db.snapshot();

        let dir = Builder::new()
            .prefix("test-snap-corruption")
            .tempdir()
            .unwrap();
        let key = SnapKey::new(region_id, 1, 1);
        let mgr_core = create_manager_core(dir.path().to_str().unwrap(), u64::MAX);
        let mut s1 = Snapshot::new_for_building(dir.path(), &key, &mgr_core).unwrap();
        assert!(!s1.exists());

        let snap_data = s1
            .build(&db, &snapshot, &region, true, false, UnixSecs::now())
            .unwrap();
        assert!(s1.exists());

        let dst_dir = Builder::new()
            .prefix("test-snap-corruption-dst")
            .tempdir()
            .unwrap();
        copy_snapshot(
            &dir,
            &dst_dir,
            &key,
            &mgr_core,
            snap_data.get_meta().clone(),
        );

        let metas = corrupt_snapshot_checksum_in(dst_dir.path());
        assert_eq!(1, metas.len());

        let mut s2 = Snapshot::new_for_applying(dst_dir.path(), &key, &mgr_core).unwrap();
        assert!(s2.exists());

        let dst_db_dir = Builder::new()
            .prefix("test-snap-corruption-dst-db")
            .tempdir()
            .unwrap();
        let dst_db: KvTestEngine = open_test_empty_db(dst_db_dir.path(), None, None).unwrap();
        let options = ApplyOptions {
            db: dst_db,
            region,
            abort: Arc::new(AtomicUsize::new(JOB_STATUS_RUNNING)),
            write_batch_size: TEST_WRITE_BATCH_SIZE,
            coprocessor_host: CoprocessorHost::<KvTestEngine>::default(),
            ingest_copy_symlink: false,
        };
        s2.apply(options).unwrap_err();
    }

    #[test]
    fn test_snap_corruption_on_meta_file() {
        let region_id = 1;
        let region = gen_test_region(region_id, 1, 1);
        let db_dir = Builder::new()
            .prefix("test-snapshot-corruption-meta-db")
            .tempdir()
            .unwrap();
        let db: KvTestEngine = open_test_db_with_100keys(db_dir.path(), None, None).unwrap();
        let snapshot = db.snapshot();

        let dir = Builder::new()
            .prefix("test-snap-corruption-meta")
            .tempdir()
            .unwrap();
        let key = SnapKey::new(region_id, 1, 1);
        let mgr_core = create_manager_core(dir.path().to_str().unwrap(), 500);
        let mut s1 = Snapshot::new_for_building(dir.path(), &key, &mgr_core).unwrap();
        assert!(!s1.exists());

        let _ = s1
            .build(&db, &snapshot, &region, true, false, UnixSecs::now())
            .unwrap();
        assert!(s1.exists());

        assert_eq!(1, corrupt_snapshot_meta_file(dir.path()));

        Snapshot::new_for_sending(dir.path(), &key, &mgr_core).unwrap_err();

        let mut s2 = Snapshot::new_for_building(dir.path(), &key, &mgr_core).unwrap();
        assert!(!s2.exists());
        let mut snap_data = s2
            .build(&db, &snapshot, &region, true, false, UnixSecs::now())
            .unwrap();
        assert!(s2.exists());

        let dst_dir = Builder::new()
            .prefix("test-snap-corruption-meta-dst")
            .tempdir()
            .unwrap();
        copy_snapshot(
            &dir,
            &dst_dir,
            &key,
            &mgr_core,
            snap_data.get_meta().clone(),
        );

        assert_eq!(1, corrupt_snapshot_meta_file(dst_dir.path()));

        Snapshot::new_for_applying(dst_dir.path(), &key, &mgr_core).unwrap_err();
        Snapshot::new_for_receiving(dst_dir.path(), &key, &mgr_core, snap_data.take_meta())
            .unwrap_err();
    }

    #[test]
    fn test_snap_mgr_create_dir() {
        // Ensure `mgr` creates the specified directory when it does not exist.
        let temp_dir = Builder::new()
            .prefix("test-snap-mgr-create-dir")
            .tempdir()
            .unwrap();
        let temp_path = temp_dir.path().join("snap1");
        let path = temp_path.to_str().unwrap().to_owned();
        assert!(!temp_path.exists());
        let mut mgr = SnapManager::new(path);
        mgr.init().unwrap();
        assert!(temp_path.exists());

        // Ensure `init()` will return an error if specified target is a file.
        let temp_path2 = temp_dir.path().join("snap2");
        let path2 = temp_path2.to_str().unwrap().to_owned();
        File::create(temp_path2).unwrap();
        mgr = SnapManager::new(path2);
        mgr.init().unwrap_err();
    }

    #[test]
    fn test_snap_mgr_v2() {
        let temp_dir = Builder::new().prefix("test-snap-mgr-v2").tempdir().unwrap();
        let path = temp_dir.path().to_str().unwrap().to_owned();
        let mgr = SnapManager::new(path.clone());
        mgr.init().unwrap();
        assert_eq!(mgr.get_total_snap_size().unwrap(), 0);

        let db_dir = Builder::new()
            .prefix("test-snap-mgr-delete-temp-files-v2-db")
            .tempdir()
            .unwrap();
        let db: KvTestEngine = open_test_db(db_dir.path(), None, None).unwrap();
        let snapshot = db.snapshot();
        let key1 = SnapKey::new(1, 1, 1);
        let mgr_core = create_manager_core(&path, u64::MAX);
        let mut s1 = Snapshot::new_for_building(&path, &key1, &mgr_core).unwrap();
        let mut region = gen_test_region(1, 1, 1);
        let mut snap_data = s1
            .build(&db, &snapshot, &region, true, false, UnixSecs::now())
            .unwrap();
        let mut s = Snapshot::new_for_sending(&path, &key1, &mgr_core).unwrap();
        let expected_size = s.total_size();
        let mut s2 =
            Snapshot::new_for_receiving(&path, &key1, &mgr_core, snap_data.get_meta().clone())
                .unwrap();
        let n = io::copy(&mut s, &mut s2).unwrap();
        assert_eq!(n, expected_size);
        s2.save().unwrap();

        let key2 = SnapKey::new(2, 1, 1);
        region.set_id(2);
        snap_data.set_region(region);
        let s3 = Snapshot::new_for_building(&path, &key2, &mgr_core).unwrap();
        let s4 =
            Snapshot::new_for_receiving(&path, &key2, &mgr_core, snap_data.take_meta()).unwrap();

        assert!(s1.exists());
        assert!(s2.exists());
        assert!(!s3.exists());
        assert!(!s4.exists());

        let mgr = SnapManager::new(path);
        mgr.init().unwrap();
        assert_eq!(mgr.get_total_snap_size().unwrap(), expected_size * 2);

        assert!(s1.exists());
        assert!(s2.exists());
        assert!(!s3.exists());
        assert!(!s4.exists());

        mgr.get_snapshot_for_sending(&key1).unwrap().delete();
        assert_eq!(mgr.get_total_snap_size().unwrap(), expected_size);
        mgr.get_snapshot_for_applying(&key1).unwrap().delete();
        assert_eq!(mgr.get_total_snap_size().unwrap(), 0);
    }

    fn check_registry_around_deregister(mgr: &SnapManager, key: &SnapKey, entry: &SnapEntry) {
        let snap_keys = mgr.list_idle_snap().unwrap();
        assert!(snap_keys.is_empty());
        assert!(mgr.has_registered(key));
        mgr.deregister(key, entry);
        let mut snap_keys = mgr.list_idle_snap().unwrap();
        assert_eq!(snap_keys.len(), 1);
        let snap_key = snap_keys.pop().unwrap().0;
        assert_eq!(snap_key, *key);
        assert!(!mgr.has_registered(&snap_key));
    }

    #[test]
    fn test_snap_deletion_on_registry() {
        let src_temp_dir = Builder::new()
            .prefix("test-snap-deletion-on-registry-src")
            .tempdir()
            .unwrap();
        let src_path = src_temp_dir.path().to_str().unwrap().to_owned();
        let src_mgr = SnapManager::new(src_path);
        src_mgr.init().unwrap();

        let src_db_dir = Builder::new()
            .prefix("test-snap-deletion-on-registry-src-db")
            .tempdir()
            .unwrap();
        let db: KvTestEngine = open_test_db(src_db_dir.path(), None, None).unwrap();
        let snapshot = db.snapshot();

        let key = SnapKey::new(1, 1, 1);
        let region = gen_test_region(1, 1, 1);

        // Ensure the snapshot being built will not be deleted on GC.
        src_mgr.register(key.clone(), SnapEntry::Generating);
        let mut s1 = src_mgr.get_snapshot_for_building(&key).unwrap();
        let mut snap_data = s1
            .build(&db, &snapshot, &region, true, false, UnixSecs::now())
            .unwrap();

        check_registry_around_deregister(&src_mgr, &key, &SnapEntry::Generating);

        // Ensure the snapshot being sent will not be deleted on GC.
        src_mgr.register(key.clone(), SnapEntry::Sending);
        let mut s2 = src_mgr.get_snapshot_for_sending(&key).unwrap();
        let expected_size = s2.total_size();

        let dst_temp_dir = Builder::new()
            .prefix("test-snap-deletion-on-registry-dst")
            .tempdir()
            .unwrap();
        let dst_path = dst_temp_dir.path().to_str().unwrap().to_owned();
        let dst_mgr = SnapManager::new(dst_path);
        dst_mgr.init().unwrap();

        // Ensure the snapshot being received will not be deleted on GC.
        dst_mgr.register(key.clone(), SnapEntry::Receiving);
        let mut s3 = dst_mgr
            .get_snapshot_for_receiving(&key, snap_data.take_meta())
            .unwrap();
        let n = io::copy(&mut s2, &mut s3).unwrap();
        assert_eq!(n, expected_size);
        s3.save().unwrap();

        check_registry_around_deregister(&src_mgr, &key, &SnapEntry::Sending);
        check_registry_around_deregister(&dst_mgr, &key, &SnapEntry::Receiving);

        // Ensure the snapshot to be applied will not be deleted on GC.
        let mut snap_keys = dst_mgr.list_idle_snap().unwrap();
        assert_eq!(snap_keys.len(), 1);
        let snap_key = snap_keys.pop().unwrap().0;
        assert_eq!(snap_key, key);
        assert!(!dst_mgr.has_registered(&snap_key));
        dst_mgr.register(key.clone(), SnapEntry::Applying);
        let s4 = dst_mgr.get_snapshot_for_applying(&key).unwrap();
        let s5 = dst_mgr.get_snapshot_for_applying(&key).unwrap();
        dst_mgr.delete_snapshot(&key, s4.as_ref(), false);
        assert!(s5.exists());
    }

    #[test]
    fn test_snapshot_max_total_size() {
        let regions: Vec<u64> = (0..20).collect();
        let kv_path = Builder::new()
            .prefix("test-snapshot-max-total-size-db")
            .tempdir()
            .unwrap();
        // Disable property collection so that the total snapshot size
        // isn't dependent on them.
        let kv_cf_opts = ALL_CFS
            .iter()
            .map(|cf| {
                let mut cf_opts = CfOptions::new();
                cf_opts.set_no_range_properties(true);
                cf_opts.set_no_table_properties(true);
                (*cf, cf_opts)
            })
            .collect();
        let engine =
            get_test_db_for_regions(&kv_path, None, None, Some(kv_cf_opts), &regions).unwrap();

        let snapfiles_path = Builder::new()
            .prefix("test-snapshot-max-total-size-snapshots")
            .tempdir()
            .unwrap();
        let max_total_size = 10240;
        let snap_mgr = SnapManagerBuilder::default()
            .max_total_size(max_total_size)
            .build::<_>(snapfiles_path.path().to_str().unwrap());
        snap_mgr.init().unwrap();
        let snapshot = engine.kv.snapshot();

        // Add an oldest snapshot for receiving.
        let recv_key = SnapKey::new(100, 100, 100);
        let mut recv_head = {
            let mut s = snap_mgr.get_snapshot_for_building(&recv_key).unwrap();
            s.build(
                &engine.kv,
                &snapshot,
                &gen_test_region(100, 1, 1),
                true,
                false,
                UnixSecs::now(),
            )
            .unwrap()
        };
        let recv_remain = {
            let mut data = Vec::with_capacity(1024);
            let mut s = snap_mgr.get_snapshot_for_sending(&recv_key).unwrap();
            s.read_to_end(&mut data).unwrap();
            assert!(snap_mgr.delete_snapshot(&recv_key, s.as_ref(), true));
            data
        };
        let mut s = snap_mgr
            .get_snapshot_for_receiving(&recv_key, recv_head.take_meta())
            .unwrap();
        s.write_all(&recv_remain).unwrap();
        s.save().unwrap();

        let snap_size = snap_mgr.get_total_snap_size().unwrap();
        let max_snap_count = (max_total_size + snap_size - 1) / snap_size;
        for (i, region_id) in regions.into_iter().enumerate() {
            let key = SnapKey::new(region_id, 1, 1);
            let region = gen_test_region(region_id, 1, 1);
            let mut s = snap_mgr.get_snapshot_for_building(&key).unwrap();
            let _ = s
                .build(&engine.kv, &snapshot, &region, true, false, UnixSecs::now())
                .unwrap();

            // The first snap_size is for region 100.
            // That snapshot won't be deleted because it's not for generating.
            assert_eq!(
                snap_mgr.get_total_snap_size().unwrap(),
                snap_size * cmp::min(max_snap_count, (i + 2) as u64)
            );
        }
    }

    #[test]
    fn test_snap_temp_file_delete() {
        let src_temp_dir = Builder::new()
            .prefix("test_snap_temp_file_delete_snap")
            .tempdir()
            .unwrap();
        let mgr_path = src_temp_dir.path().to_str().unwrap();
        let src_mgr = SnapManager::new(mgr_path.to_owned());
        src_mgr.init().unwrap();
        let kv_temp_dir = Builder::new()
            .prefix("test_snap_temp_file_delete_kv")
            .tempdir()
            .unwrap();
        let engine = open_test_db(kv_temp_dir.path(), None, None).unwrap();
        let sst_path = src_mgr.get_temp_path_for_ingest();
        let mut writer = <KvTestEngine as SstExt>::SstWriterBuilder::new()
            .set_db(&engine)
            .build(&sst_path)
            .unwrap();
        writer.put(b"a", b"a").unwrap();
        let r = writer.finish().unwrap();
        assert!(file_system::file_exists(&sst_path));
        assert_eq!(r.file_path().to_str().unwrap(), sst_path.as_str());
        drop(src_mgr);
        let src_mgr = SnapManager::new(mgr_path.to_owned());
        src_mgr.init().unwrap();
        // The sst_path will be deleted by SnapManager because it is a temp filet.
        assert!(!file_system::file_exists(&sst_path));
    }

    #[test]
    fn test_snapshot_stats() {
        let snap_dir = Builder::new()
            .prefix("test_snapshot_stats")
            .tempdir()
            .unwrap();
        let start = Instant::now();
        let mgr = TabletSnapManager::new(snap_dir.path(), None).unwrap();
        let key = TabletSnapKey::new(1, 1, 1, 1);
        mgr.begin_snapshot(key.clone(), start - time::Duration::from_secs(2), 1);
        // filter out the snapshot that is not finished
        assert!(mgr.stats().stats.is_empty());
        mgr.finish_snapshot(key.clone(), start - time::Duration::from_secs(1));
        let stats = mgr.stats().stats;
        assert_eq!(stats.len(), 1);
        assert_eq!(stats[0].get_total_duration_sec(), 2);
        assert!(mgr.stats().stats.is_empty());

        // filter out the total duration seconds less than one sencond.
        let path = mgr.tablet_gen_path(&key);
        std::fs::create_dir_all(&path).unwrap();
        assert!(path.exists());
        mgr.delete_snapshot(&key);
        assert_eq!(mgr.stats().stats.len(), 0);
        assert!(!path.exists());
    }

    #[test]
    fn test_build_with_encryption() {
        let (_enc_dir, key_manager) =
            create_encryption_key_manager("test_build_with_encryption_enc");

        let snap_dir = Builder::new()
            .prefix("test_build_with_encryption_snap")
            .tempdir()
            .unwrap();
        let _mgr_path = snap_dir.path().to_str().unwrap();
        let snap_mgr = SnapManagerBuilder::default()
            .encryption_key_manager(Some(key_manager))
            .build(snap_dir.path().to_str().unwrap());
        snap_mgr.init().unwrap();

        let kv_dir = Builder::new()
            .prefix("test_build_with_encryption_kv")
            .tempdir()
            .unwrap();
        let db: KvTestEngine = open_test_db(kv_dir.path(), None, None).unwrap();
        let snapshot = db.snapshot();
        let key = SnapKey::new(1, 1, 1);
        let region = gen_test_region(1, 1, 1);

        // Test one snapshot can be built multi times. DataKeyManager should be handled
        // correctly.
        for _ in 0..2 {
            let mut s1 = snap_mgr.get_snapshot_for_building(&key).unwrap();
            let _ = s1
                .build(&db, &snapshot, &region, true, false, UnixSecs::now())
                .unwrap();
            assert!(snap_mgr.delete_snapshot(&key, &s1, false));
        }
    }

    #[test]
    fn test_generate_snap_for_tablet_snapshot() {
        let snap_dir = Builder::new().prefix("test_snapshot").tempdir().unwrap();
        let snap_mgr = SnapManagerBuilder::default()
            .enable_receive_tablet_snapshot(true)
            .build(snap_dir.path().to_str().unwrap());
        snap_mgr.init().unwrap();
        let tablet_snap_key = TabletSnapKey::new(1, 2, 3, 4);
        snap_mgr
            .gen_empty_snapshot_for_tablet_snapshot(&tablet_snap_key, false)
            .unwrap();

        let snap_key = SnapKey::new(1, 3, 4);
        let s = snap_mgr.get_snapshot_for_applying(&snap_key).unwrap();
        let expect_path = snap_mgr
            .tablet_snap_manager()
            .as_ref()
            .unwrap()
            .final_recv_path(&tablet_snap_key);
        assert_eq!(expect_path.to_str().unwrap(), s.tablet_snap_path().unwrap());
    }

    #[test]
    fn test_init_enable_receive_tablet_snapshot() {
        let builder = SnapManagerBuilder::default().enable_receive_tablet_snapshot(true);
        let snap_dir = Builder::new()
            .prefix("test_snap_path_does_not_exist")
            .tempdir()
            .unwrap();
        let path = snap_dir.path().join("snap");
        let snap_mgr = builder.build(path.as_path().to_str().unwrap());
        snap_mgr.init().unwrap();

        assert!(path.exists());
        let mut path = path.as_path().to_str().unwrap().to_string();
        path.push_str("_v2");
        assert!(Path::new(&path).exists());

        let builder = SnapManagerBuilder::default().enable_receive_tablet_snapshot(true);
        let snap_dir = Builder::new()
            .prefix("test_snap_path_exist")
            .tempdir()
            .unwrap();
        let path = snap_dir.path();
        let snap_mgr = builder.build(path.to_str().unwrap());
        snap_mgr.init().unwrap();

        let mut path = path.to_str().unwrap().to_string();
        path.push_str("_v2");
        assert!(Path::new(&path).exists());

        let builder = SnapManagerBuilder::default().enable_receive_tablet_snapshot(true);
        let snap_dir = Builder::new()
            .prefix("test_tablet_snap_path_exist")
            .tempdir()
            .unwrap();
        let path = snap_dir.path().join("snap/v2");
        fs::create_dir_all(path).unwrap();
        let path = snap_dir.path().join("snap");
        let snap_mgr = builder.build(path.to_str().unwrap());
        snap_mgr.init().unwrap();
        assert!(path.exists());
    }

    #[test]
    fn test_from_path() {
        let snap_dir = Builder::new().prefix("test_from_path").tempdir().unwrap();
        let path = snap_dir.path().join("gen_1_2_3_4");
        let key = TabletSnapKey::from_path(path).unwrap();
        let expect_key = TabletSnapKey::new(1, 2, 3, 4);
        assert_eq!(expect_key, key);
        let path = snap_dir.path().join("gen_1_2_3_4.tmp");
        TabletSnapKey::from_path(path).unwrap_err();
    }
}
