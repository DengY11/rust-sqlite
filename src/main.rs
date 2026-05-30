fn main() -> rustsql::common::error::Result<()> {
    rustsql::repl::run_from_args(std::env::args_os())
}

#[cfg(test)]
mod tests {
    #[test]
    fn main_has_expected_entry_signature() {
        let entry: fn() -> rustsql::common::error::Result<()> = super::main;
        let _ = entry;
    }
}
