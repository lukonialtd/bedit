fn main() {
    let code = bedit::wrapper::main(
        bedit::wrapper::WrapperSpec {
            wrapper: "bnvim",
            editor: "nvim",
        },
        std::env::args().skip(1).collect(),
    );
    std::process::exit(code);
}
