//! Isolated typed-value writeback benchmark on a disposable full-archive clone.
use std::path::Path;
use std::time::Instant;
use blockstack_lib::clarity_vm::database::value_extents::ValueExtentStore;
use clarity::vm::database::{DataStoreValue, TypedValueData};
use clarity::vm::types::Value;
use rusqlite::Connection;
use serde_json::json;
use sha2::{Digest,Sha256};

fn main() {
    let args: Vec<_> = std::env::args().collect();
    assert_eq!(args.len(),2,"usage: ptrhash_writeback DISPOSABLE_DB");
    let path=Path::new(&args[1]);
    let db=Connection::open(path).unwrap();
    let mut store=ValueExtentStore::open_registered(&db,path,true).unwrap().unwrap();
    let mut first_pass=0.0; let mut repeat_pass=0.0; let mut commit=0.0;
    let mut before=0; let mut appended=Vec::new();
    let all_start=Instant::now();
    for batch in 0u64..120 {
        // Preparation is outside append_indexed timing but inside the loop's total elapsed time.
        let values: Vec<_>=(0u64..256).map(|i| {
            let mut bytes=vec![0x37;128];
            bytes[..16].copy_from_slice(&b"ptrhash-proto-v1!"[..16]);
            bytes[16..24].copy_from_slice(&(batch*256+i).to_le_bytes());
            DataStoreValue::Typed(TypedValueData::prepare(Value::buff_from(bytes).unwrap()).unwrap())
        }).collect();
        if batch==20 { before=std::fs::metadata(format!("{}.values",path.display())).unwrap().len(); }
        store.begin_block().unwrap(); db.execute_batch("BEGIN IMMEDIATE").unwrap();
        let start=Instant::now(); let locations=store.append_indexed(&db,&values).unwrap();
        if batch>=20 { first_pass+=start.elapsed().as_secs_f64(); }
        let start=Instant::now(); assert_eq!(store.append_indexed(&db,&values).unwrap(),locations);
        if batch>=20 { repeat_pass+=start.elapsed().as_secs_f64(); }
        let start=Instant::now(); store.publish_block().unwrap(); db.execute_batch("COMMIT").unwrap();store.commit_dedup_cache();
        if batch>=20 { commit+=start.elapsed().as_secs_f64(); }
        // Read and compare every typed value after commit, outside the measured write phases.
        for ((hash,extent),value) in locations.iter().zip(&values) {
            assert_eq!(store.read(*extent,hash).unwrap().canonical().unwrap(),value.canonical());
        }
        if batch>=20 { appended.extend(locations); }
    }
    let measured_end=std::fs::metadata(format!("{}.values",path.display())).unwrap().len();
    let mut digest=Sha256::new(); for (hash,extent) in &appended {digest.update(hash.0);digest.update(extent.offset.to_le_bytes());digest.update(extent.length.to_le_bytes());}
    let signature:String=digest.finalize().iter().map(|b|format!("{b:02x}")).collect();
    println!("{}",json!({"warmup_batches":20,"measured_batches":100,"values_per_batch":256,"measured_new_values":25600,"repeat_reads":25600,"first_append_seconds":first_pass,"repeat_append_seconds":repeat_pass,"publish_commit_seconds":commit,"loop_seconds_including_warmup_and_verification":all_start.elapsed().as_secs_f64(),"appended_bytes":measured_end-before,"location_signature":signature}));
}
