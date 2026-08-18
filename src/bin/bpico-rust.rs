fn main() {
    let code = bedit::wrapper::main(
        bedit::wrapper::WrapperSpec {
            wrapper: "bpico",
            editor: "pico",
        },
        std::env::args().skip(1).collect(),
    );
    std::process::exit(code);
}
