pub mod browsercontrol {
    pub mod v1 {
        // The generated tonic service traits return `Result<_, tonic::Status>`,
        // which clippy (>= 1.98) flags as `result_large_err`. This is generated
        // code we cannot annotate per-item, so allow it for the whole module.
        #![allow(clippy::result_large_err)]
        tonic::include_proto!("browsercontrol.v1");
    }
}
