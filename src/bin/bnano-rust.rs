fn main() {
    let code = bedit::wrapper::main(
        bedit::wrapper::WrapperSpec {
            wrapper: "bnano",
            editor: "nano",
        },
        std::env::args().skip(1).collect(),
    );
    std::process::exit(code);
}
