fn main() {
    let code = bedit::wrapper::main(
        bedit::wrapper::WrapperSpec {
            wrapper: "bed",
            editor: "ed",
        },
        std::env::args().skip(1).collect(),
    );
    std::process::exit(code);
}
