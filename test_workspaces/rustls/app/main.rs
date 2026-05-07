fn main() {
    let roots = rustls::RootCertStore::empty();
    assert!(roots.is_empty());

    let version = rustls::ProtocolVersion::TLSv1_3;
    assert_eq!(format!("{version:?}"), "TLSv1_3");
}
