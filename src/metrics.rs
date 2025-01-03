use lazy_static::lazy_static;
use prometheus::{opts, register_gauge_vec, register_int_gauge, GaugeVec, IntGauge};

lazy_static! {
    pub static ref EPOCH: IntGauge = register_int_gauge!(opts!("epoch", "Epoch number",),).unwrap();
    pub static ref SIGNER_COUNTER: GaugeVec =
        register_gauge_vec!(opts!("signer_counter", "Number of signer.",), &["tag"]).unwrap();
    pub static ref SLICE_COUNTER: GaugeVec = register_gauge_vec!(
        opts!("slice_counter", "Number of slice in each quorum.",),
        &["quorum", "tag"]
    )
    .unwrap();
}
