fn main() {
    println!("cargo:rerun-if-changed=src/services/android_auto/aa.proto");
    protobuf_codegen::Codegen::new()
        .pure()
        .include("src/services/android_auto")
        .input("src/services/android_auto/aa.proto")
        .cargo_out_dir("aa")
        .run_from_script();
}
