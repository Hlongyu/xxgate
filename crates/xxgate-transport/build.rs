fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux") {
        // btls prefixes BoringSSL symbols but still exports search paths with
        // libssl.a/libcrypto.a. Resolve native-tls's system OpenSSL by its exact
        // shared-library filename so those archives cannot shadow it.
        println!("cargo:rustc-link-lib=dylib:+verbatim=libssl.so");
        println!("cargo:rustc-link-lib=dylib:+verbatim=libcrypto.so");
    }
}
