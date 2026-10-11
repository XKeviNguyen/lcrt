fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux") {
        // Development libraries are copied beside the binary by sherpa's build
        // script; installed libraries live under the application prefix.
        println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN:$ORIGIN/../lib/lcrt");
    }
}
