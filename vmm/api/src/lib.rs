pub mod sandbox_grpc {
    include!(concat!(env!("OUT_DIR"), "/kuasar.sandbox.v1.rs"));
}

pub mod ssi_grpc {
    include!(concat!(env!("OUT_DIR"), "/ssi.v1alpha1.rs"));
}
