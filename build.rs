fn main() {
    if std::process::Command::new("protoc")
        .arg("--version")
        .output()
        .is_err()
    {
        panic!(
            "\n\nError: `protoc` (protobuf compiler) not found.\n\n\
             Install it with your system package manager and ensure it is on PATH.\n\n"
        );
    }
}
