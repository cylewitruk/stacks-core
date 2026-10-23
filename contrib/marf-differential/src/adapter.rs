//! Shared replay loop for both backends.
//!
//! Every observation comes from calling the implementation under test: ops
//! outside a transaction go to the handle-level API (`MARF::insert`,
//! `MARF::commit`, ...), ops inside a `Begin` scope go to the transaction.
//! The only synthesized observation is `Err("NotOpened")` when no handle is
//! open, since neither implementation can be called at all then.

use std::path::{Path, PathBuf};

use crate::ops::{Observation, Op, OpenOpts, Tamper};

/// A tampered proof's verification outcome.
pub enum ProofCheck {
    /// Proof generation failed.
    Err(String),
    /// No value for the key.
    Missing,
    /// Tampered bytes did not deserialize.
    Deserialize,
    Verified(bool),
}

/// The implementation-specific hooks the replay loop needs.
pub trait MarfApi {
    type Handle;
    type Tx<'a>
    where
        Self: 'a;

    fn open(&self, path: &str, opts: &OpenOpts) -> Result<Self::Handle, String>;
    fn prune(&self, src: &str, dst: &str, tip: &str, height: u32) -> Result<(), String>;

    // handle-level
    fn h_begin_tx<'a>(h: &'a mut Self::Handle) -> Self::Tx<'a>;
    /// `MARF::begin`: begin in a committed transaction, trie stays open on the handle.
    fn h_begin(h: &mut Self::Handle, parent: &str, child: &str) -> Observation;
    fn h_read(h: &mut Self::Handle, op: &Op) -> Option<Observation>;
    fn h_insert(h: &mut Self::Handle, entries: &[(String, String)]) -> Observation;
    fn h_seal(h: &mut Self::Handle) -> Observation;
    fn h_commit(h: &mut Self::Handle) -> Observation;
    fn h_commit_to(h: &mut Self::Handle, block: &str) -> Observation;
    fn h_commit_mined(h: &mut Self::Handle, block: &str) -> Observation;
    fn h_drop_current(h: &mut Self::Handle) -> Observation;
    fn h_drop_unconfirmed(h: &mut Self::Handle) -> Observation;
    fn h_open_tip(h: &Self::Handle) -> Observation;

    // transaction-level
    fn t_begin(tx: &mut Self::Tx<'_>, parent: &str, child: &str) -> Observation;
    fn t_begin_unconfirmed(tx: &mut Self::Tx<'_>, parent: &str) -> Observation;
    fn t_read(tx: &mut Self::Tx<'_>, op: &Op) -> Option<Observation>;
    fn t_insert(tx: &mut Self::Tx<'_>, entries: &[(String, String)]) -> Observation;
    fn t_seal(tx: &mut Self::Tx<'_>) -> Observation;
    fn t_commit(tx: Self::Tx<'_>) -> Observation;
    fn t_commit_to(tx: Self::Tx<'_>, block: &str) -> Observation;
    fn t_commit_mined(tx: Self::Tx<'_>, block: &str) -> Observation;
    fn t_drop_current(tx: Self::Tx<'_>) -> Observation;
    fn t_drop_unconfirmed(tx: Self::Tx<'_>) -> Observation;
    fn t_open_tip(tx: &Self::Tx<'_>) -> Observation;
}

pub struct Driver<A: MarfApi> {
    api: A,
    dir: PathBuf,
    handle: Option<A::Handle>,
}

enum TxEnd {
    /// `n` ops consumed; the transaction ended.
    Consumed(usize),
    /// `n` ops consumed; op `n` must run outside (the transaction was dropped).
    Abandoned(usize),
}

impl<A: MarfApi> Driver<A> {
    pub fn new(api: A, dir: &Path) -> Self {
        Driver {
            api,
            dir: dir.to_path_buf(),
            handle: None,
        }
    }

    pub fn dir(&self) -> PathBuf {
        self.dir.clone()
    }

    fn db_path(&self) -> String {
        self.dir.join("marf.sqlite").to_string_lossy().into_owned()
    }

    fn entries(op: &Op) -> Vec<(String, String)> {
        match op {
            Op::Insert { key, value } => vec![(key.clone(), value.clone())],
            Op::InsertBatch { entries } => entries.clone(),
            _ => unreachable!(),
        }
    }

    fn run_tx(mut tx: A::Tx<'_>, ops: &[Op], out: &mut Vec<Observation>) -> TxEnd {
        let mut i = 0;
        while i < ops.len() {
            let op = &ops[i];
            if let Some(o) = A::t_read(&mut tx, op) {
                out.push(o);
                i += 1;
                continue;
            }
            match op {
                Op::Insert { .. } | Op::InsertBatch { .. } => {
                    out.push(A::t_insert(&mut tx, &Self::entries(op)))
                }
                Op::Seal => out.push(A::t_seal(&mut tx)),
                Op::OpenTip => out.push(A::t_open_tip(&tx)),
                // nested begins go to the real API, which reports InProgress
                Op::Begin { parent, child } | Op::BeginHandle { parent, child } => {
                    out.push(A::t_begin(&mut tx, parent, child))
                }
                Op::BeginUnconfirmed { parent } => {
                    out.push(A::t_begin_unconfirmed(&mut tx, parent))
                }
                Op::Commit => {
                    out.push(A::t_commit(tx));
                    return TxEnd::Consumed(i + 1);
                }
                Op::CommitTo { block } => {
                    out.push(A::t_commit_to(tx, block));
                    return TxEnd::Consumed(i + 1);
                }
                Op::CommitMined { block } => {
                    out.push(A::t_commit_mined(tx, block));
                    return TxEnd::Consumed(i + 1);
                }
                Op::DropCurrent => {
                    out.push(A::t_drop_current(tx));
                    return TxEnd::Consumed(i + 1);
                }
                Op::DropUnconfirmed => {
                    out.push(A::t_drop_unconfirmed(tx));
                    return TxEnd::Consumed(i + 1);
                }
                Op::Open(_) | Op::Close | Op::Prune { .. } => return TxEnd::Abandoned(i),
                _ => unreachable!("read op not handled by t_read: {op:?}"),
            }
            i += 1;
        }
        TxEnd::Consumed(ops.len())
    }

    pub fn run(&mut self, ops: &[Op]) -> Vec<Observation> {
        let mut out = Vec::with_capacity(ops.len());
        let mut i = 0;
        while i < ops.len() {
            let op = &ops[i];
            match op {
                Op::Open(o) => {
                    self.handle = None;
                    let path = self.db_path();
                    match self.api.open(&path, o) {
                        Ok(h) => {
                            self.handle = Some(h);
                            out.push(Observation::Ok);
                        }
                        Err(e) => out.push(Observation::Err(e)),
                    }
                    i += 1;
                    continue;
                }
                Op::Close => {
                    self.handle = None;
                    out.push(Observation::Ok);
                    i += 1;
                    continue;
                }
                Op::Prune { tip, height } => {
                    self.handle = None;
                    let src = self.db_path();
                    let dst_dir = self.dir.join(format!("prune-{height}"));
                    std::fs::create_dir_all(&dst_dir).expect("mkdir");
                    let dst = dst_dir.join("marf.sqlite").to_string_lossy().into_owned();
                    match self.api.prune(&src, &dst, tip, *height) {
                        Ok(()) => {
                            self.dir = dst_dir;
                            out.push(Observation::Ok);
                        }
                        Err(e) => out.push(Observation::Err(e)),
                    }
                    i += 1;
                    continue;
                }
                _ => {}
            }
            let Some(h) = self.handle.as_mut() else {
                out.push(Observation::Err("NotOpened".into()));
                i += 1;
                continue;
            };
            if let Some(o) = A::h_read(h, op) {
                out.push(o);
                i += 1;
                continue;
            }
            match op {
                Op::Begin { parent, child } => {
                    let mut tx = A::h_begin_tx(h);
                    let o = A::t_begin(&mut tx, parent, child);
                    let ok = o == Observation::Ok;
                    out.push(o);
                    if ok {
                        match Self::run_tx(tx, &ops[i + 1..], &mut out) {
                            TxEnd::Consumed(n) | TxEnd::Abandoned(n) => i += 1 + n,
                        }
                        continue;
                    }
                }
                Op::BeginHandle { parent, child } => out.push(A::h_begin(h, parent, child)),
                Op::BeginUnconfirmed { parent } => {
                    let mut tx = A::h_begin_tx(h);
                    let o = A::t_begin_unconfirmed(&mut tx, parent);
                    let ok = matches!(o, Observation::Block(Some(_)));
                    out.push(o);
                    if ok {
                        match Self::run_tx(tx, &ops[i + 1..], &mut out) {
                            TxEnd::Consumed(n) | TxEnd::Abandoned(n) => i += 1 + n,
                        }
                        continue;
                    }
                }
                Op::Insert { .. } | Op::InsertBatch { .. } => {
                    out.push(A::h_insert(h, &Self::entries(op)))
                }
                Op::Seal => out.push(A::h_seal(h)),
                Op::Commit => out.push(A::h_commit(h)),
                Op::CommitTo { block } => out.push(A::h_commit_to(h, block)),
                Op::CommitMined { block } => out.push(A::h_commit_mined(h, block)),
                Op::DropCurrent => out.push(A::h_drop_current(h)),
                Op::DropUnconfirmed => out.push(A::h_drop_unconfirmed(h)),
                Op::OpenTip => out.push(A::h_open_tip(h)),
                Op::Open(_) | Op::Close | Op::Prune { .. } => unreachable!(),
                _ => unreachable!("read op not handled by h_read: {op:?}"),
            }
            i += 1;
        }
        out
    }
}

/// Apply a tamper to serialized proof bytes.
pub fn tamper_bytes(bytes: &mut Vec<u8>, tamper: &Tamper) {
    match tamper {
        Tamper::FlipByte(i) => {
            if !bytes.is_empty() {
                let i = *i % bytes.len();
                bytes[i] ^= 0x01;
            }
        }
        Tamper::Truncate(n) => bytes.truncate(*n),
        _ => {}
    }
}

pub fn proof_observation(check: ProofCheck) -> Observation {
    match check {
        ProofCheck::Err(e) => Observation::Err(e),
        ProofCheck::Missing => Observation::Value(None),
        ProofCheck::Deserialize => Observation::Err("Deserialize".into()),
        ProofCheck::Verified(v) => Observation::Bool(v),
    }
}
